use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::adapters::base::SessionAdapter;
use crate::adapters::claude::ClaudeAdapter;

use super::attach::{
    attach_pipe, clear_composer, close_pipe, feed, hooked_wait_client_ready,
    hooked_wait_engine_behind, restore_draft, Client,
};
use super::engine::{hooked_engine_for_job, pane_for_job, EngineSession};
use super::lifecycle::{bg_env, run_capture};
use super::{sleep_s, AGENTS_TIMEOUT};

#[cfg(test)]
use super::testhook;

// --------------------------------------------------------------------------
// keyboard: piping keystrokes into the engine over `claude attach <jobId>`
// --------------------------------------------------------------------------
// `claude attach` reads stdin even when it is a pipe, so a jobId addresses the
// engine's keyboard the same way it addresses everything else — no tmux, no
// pane, no viewer. A pane viewer stays attached and unflickered while this
// second client types (real-machine verified, 2.1.240), and the attach itself
// wakes a parked engine, so the keyboard path self-heals the ~1h park for free.
pub(super) const CLEAR_LINE: &str = "\u{15}"; // C-u: drop whatever is in the composer (claude keeps it
                                              // on its own kill ring — Ctrl+Y pastes it back)
pub(super) const RESTORE_KILL: &str = "\u{19}"; // C-y: paste the kill ring back into the composer
const SUBMIT: &str = "\r";
const ESCAPE: &str = "\u{1b}"; // interrupts the running turn

// Only used when the job is on nobody's screen: claude's own pty host
// starts at this size, so it is the least surprising thing to wear.
pub(super) const DEFAULT_PTY_COLS: u16 = 200;
pub(super) const DEFAULT_PTY_ROWS: u16 = 50;

pub(super) const ENGINE_READY_TIMEOUT: f64 = 20.0; // our own attach is the wake; the entry follows it
pub(super) const CLIENT_READY_TIMEOUT: f64 = 15.0; // observed ~0.3s to the journal entry
const TYPE_READY_TIMEOUT: f64 = 25.0; // total budget for "the client is forwarding stdin"
const TYPE_RETRY_AFTER: f64 = 5.0; // re-type (C-u first, so it is idempotent) after this
const SUBMIT_CONFIRM_TIMEOUT: f64 = 20.0; // the user turn is written the moment it lands

// A slash command's `<command-name>` record is written when the command
// *finishes* (a /compact can take a minute), so waiting for it would block the
// caller on work it does not need to see. This window only has to be long
// enough for the failure shape — the command submitted as plain text, which
// writes its turn immediately.
const SLASH_CONFIRM_TIMEOUT: f64 = 5.0;
const INTERRUPT_CONFIRM_TIMEOUT: f64 = 12.0;
const KEY_POLL_INTERVAL: f64 = 0.4;
const ECHO_POLL_INTERVAL: f64 = 0.05; // in-memory read of our own attach stream
const RENAME_CONFIRM_TIMEOUT: f64 = 5.0; // a control/rename lands in ~0.1s; this is slack
const RENAME_POLL_INTERVAL: f64 = 0.1; // registry file reads are cheap
pub(super) const CONTROL_KEY_GAP: f64 = 0.25; // a control byte must not ride in the text's chunk
pub(super) const ATTACH_EXIT_TIMEOUT: f64 = 10.0;

const ECHO_PREFIX_CHARS: usize = 40; // head/tail slice: unique enough, short enough to survive a wrap
const PASTE_PLACEHOLDER: &str = "[Pastedtext#"; // squashed `[Pasted text #N]`
const INTERRUPT_MARKER: &str = "[Request interrupted by user]";

pub(super) const BUF_CAP: usize = 262144;

macro_rules! tunable {
    ($fn_name:ident, $field:ident, $default:expr) => {
        fn $fn_name() -> f64 {
            #[cfg(test)]
            {
                if let Some(Some(v)) = testhook::with(|h| h.$field) {
                    return v;
                }
            }
            $default
        }
    };
}

tunable!(type_retry_after, type_retry_after, TYPE_RETRY_AFTER);
tunable!(type_ready_timeout, type_ready_timeout, TYPE_READY_TIMEOUT);
tunable!(
    slash_confirm_timeout,
    slash_confirm_timeout,
    SLASH_CONFIRM_TIMEOUT
);
tunable!(
    submit_confirm_timeout,
    submit_confirm_timeout,
    SUBMIT_CONFIRM_TIMEOUT
);
tunable!(
    interrupt_confirm_timeout,
    interrupt_confirm_timeout,
    INTERRUPT_CONFIRM_TIMEOUT
);
tunable!(
    rename_confirm_timeout,
    rename_confirm_timeout,
    RENAME_CONFIRM_TIMEOUT
);
tunable!(
    rename_poll_interval,
    rename_poll_interval,
    RENAME_POLL_INTERVAL
);

/// Outcome of a keystroke pipe. `confirmed` names the evidence:
/// `transcript` (the engine recorded the turn/command/interrupt), `status`
/// (the engine left `busy`) or `written` (the bytes went into the pipe and
/// nothing contradicted it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyResult {
    pub ok: bool,
    pub confirmed: String,
    pub why: String,
}

impl KeyResult {
    fn failure(why: impl Into<String>) -> Self {
        KeyResult {
            ok: false,
            confirmed: String::new(),
            why: why.into(),
        }
    }

    fn success(confirmed: &str, why: &str) -> Self {
        KeyResult {
            ok: true,
            confirmed: confirmed.to_string(),
            why: why.to_string(),
        }
    }
}

/// The full terminal-escape strip: CSI, OSC, APC/DCS/SOS/PM (claude emits
/// `cc-daemon-hint`), charset selection and keypad-mode toggles.
pub(crate) fn strip_ansi(text: &str) -> String {
    fn match_escape(chars: &[char], start: usize) -> Option<usize> {
        let n = chars.len();
        match *chars.get(start + 1)? {
            '[' => {
                let mut i = start + 2;
                while i < n && matches!(chars[i], '0'..='9' | ';' | ':' | '<' | '=' | '>' | '?') {
                    i += 1;
                }
                while i < n && (' '..='/').contains(&chars[i]) {
                    i += 1;
                }
                if i < n && ('@'..='~').contains(&chars[i]) {
                    Some(i + 1)
                } else {
                    None
                }
            }
            ']' => {
                let mut i = start + 2;
                while i < n && chars[i] != '\u{7}' && chars[i] != '\u{1b}' {
                    i += 1;
                }
                if i < n && chars[i] == '\u{7}' {
                    Some(i + 1)
                } else if i + 1 < n && chars[i] == '\u{1b}' && chars[i + 1] == '\\' {
                    Some(i + 2)
                } else {
                    None
                }
            }
            '_' | 'P' | 'X' | '^' => {
                let mut i = start + 2;
                while i < n && chars[i] != '\u{1b}' {
                    i += 1;
                }
                if i + 1 < n && chars[i] == '\u{1b}' && chars[i + 1] == '\\' {
                    Some(i + 2)
                } else {
                    None
                }
            }
            '(' | ')' => {
                if matches!(*chars.get(start + 2)?, '0'..='9' | 'A' | 'B') {
                    Some(start + 3)
                } else {
                    None
                }
            }
            '=' | '>' => Some(start + 2),
            _ => None,
        }
    }

    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\u{1b}' {
            if let Some(end) = match_escape(&chars, i) {
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// The attach client's own pty stream (`Client::text_since`) is raw terminal
/// output: the layout lives in cursor moves, not in spaces, so whitespace and
/// box drawing are noise for a substring test.
fn squash(text: &str) -> String {
    text.chars()
        .filter(|c| !(c.is_whitespace() || ('\u{2500}'..='\u{259f}').contains(c)))
        .collect()
}

/// Is there real, human-typed text sitting in the engine's composer?
///
/// Read from the member's own tmux pane — the one place the composer is
/// rendered by a real terminal emulator — through the draft guard's styled
/// capture, whose dim tracking keeps autocomplete ghost text from counting as
/// a draft. Only a pane that is certainly-or-likely showing this very job is
/// read; anything else — viewer elsewhere, panel list, no pane — returns
/// false, which just skips the restore. This gates the kill-ring paste:
/// claude's kill ring survives a C-u on an *empty* composer unchanged, so
/// pasting without the gate would resurrect whatever the ring happened to
/// hold (real-machine verified).
pub(crate) fn composer_has_draft(job_id: &str) -> bool {
    let Some(pane) = hooked_pane_for_job(job_id) else {
        return false;
    };
    if pane.is_empty() {
        return false;
    }
    let Some((view_job, certainty)) = probe_view(&pane) else {
        return false;
    };
    if view_job != job_id || (certainty != "certain" && certainty != "likely") {
        return false;
    }
    hooked_suspected_draft(&pane, "claude")
}

fn hooked_pane_for_job(job_id: &str) -> Option<String> {
    #[cfg(test)]
    {
        if let Some(Some(v)) = testhook::with(|h| h.pane_for_job.clone()) {
            return v;
        }
    }
    pane_for_job(job_id)
}

/// (job_id, certainty) of what the pane's viewer is showing; None when the
/// probe itself blew up (which closes the draft gate, never crashes it).
fn probe_view(pane: &str) -> Option<(String, String)> {
    #[cfg(test)]
    {
        if let Some(Some(res)) = testhook::with(|h| h.view_probe.clone()) {
            return res.ok();
        }
    }
    let view = crate::adapters::claude_view::view_for_pane(pane, None);
    Some((view.job_id, view.certainty))
}

fn hooked_suspected_draft(pane: &str, profile: &str) -> bool {
    #[cfg(test)]
    {
        if let Some(Some(v)) = testhook::with(|h| {
            if h.suspected_draft.is_some() {
                h.suspected_calls
                    .push((pane.to_string(), profile.to_string()));
            }
            h.suspected_draft
        }) {
            return v;
        }
    }
    crate::draft_guard::suspected_draft(pane, profile).unwrap_or(false)
}

fn hooked_composer_draft(job_id: &str) -> bool {
    #[cfg(test)]
    {
        if let Some(Some(v)) = testhook::with(|h| h.composer_draft) {
            return v;
        }
    }
    composer_has_draft(job_id)
}

/// What "the composer is showing *text*" can look like on the pty screen.
///
/// Three shapes, any of which counts: the head of the text, its tail (a long
/// paste scrolls the composer viewport to the cursor, so the head is off
/// screen), and the `[Pasted text #N]` placeholder the TUI folds a long paste
/// into, which carries none of the text at all.
fn echo_needles(text: &str) -> Vec<String> {
    let squashed = squash(text);
    if squashed.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = squashed.chars().collect();
    let head: String = chars.iter().take(ECHO_PREFIX_CHARS).collect();
    let tail: String = chars[chars.len().saturating_sub(ECHO_PREFIX_CHARS)..]
        .iter()
        .collect();
    let mut needles = Vec::new();
    for needle in [head, tail, PASTE_PLACEHOLDER.to_string()] {
        if !needles.contains(&needle) {
            needles.push(needle);
        }
    }
    needles
}

/// The job's transcript file and its current size — the offset new records
/// are read from once the submit lands.
fn transcript_cursor(engine: Option<&EngineSession>) -> (Option<PathBuf>, u64) {
    let Some(engine) = engine else {
        return (None, 0);
    };
    if engine.session_id.is_empty() {
        return (None, 0);
    }
    let cwd = if engine.cwd.is_empty() {
        None
    } else {
        Some(engine.cwd.as_str())
    };
    let Some(path) = ClaudeAdapter.find_session_file(&engine.session_id, cwd) else {
        return (None, 0);
    };
    match fs::metadata(&path) {
        Ok(meta) => {
            let size = meta.len();
            (Some(path), size)
        }
        Err(_) => (Some(path), 0),
    }
}

fn hooked_transcript_cursor(engine: Option<&EngineSession>) -> (Option<PathBuf>, u64) {
    #[cfg(test)]
    {
        if let Some(Some(v)) = testhook::with(|h| h.transcript_cursor.clone()) {
            return v;
        }
    }
    transcript_cursor(engine)
}

/// Whatever the transcript gained after *offset*.
fn transcript_since(path: Option<&Path>, offset: u64) -> String {
    let Some(path) = path else {
        return String::new();
    };
    let Ok(mut handle) = fs::File::open(path) else {
        return String::new();
    };
    if handle.seek(SeekFrom::Start(offset)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if handle.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn user_text(record: &Value) -> Option<String> {
    if record.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let content = record.get("message").and_then(|m| m.get("content"))?;
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .map(|b| b.get("text").and_then(Value::as_str).unwrap_or(""))
                .collect::<Vec<_>>()
                .join(""),
        ),
        _ => None,
    }
}

fn is_slash_command(text: &str) -> bool {
    let stripped = text.trim();
    stripped.starts_with('/') && !stripped.contains('\n')
}

enum SubmitVerdict {
    Landed,
    Corrupted,
    /// Nothing yet — keep waiting.
    None,
}

/// What the transcript says about the submit.
///
/// A slash command lands as a `<command-name>` entry: the engine ran the
/// command instead of sending its literal text to the model. Anything else
/// lands as a user turn whose content equals what was typed *exactly*.
/// `Corrupted` is the case exact matching exists for: a turn that ends with
/// the typed text but carries something in front of it is a leftover
/// composer draft that got submitted along with the delivery — the one thing
/// a substring match would wave through.
fn submit_verdict(path: Option<&Path>, offset: u64, text: &str) -> SubmitVerdict {
    let chunk = transcript_since(path, offset);
    if chunk.is_empty() {
        return SubmitVerdict::None;
    }
    let mut turns: Vec<String> = Vec::new();
    for line in chunk.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue; // a half-written tail line; the next poll sees it whole
        };
        if record.is_object() {
            if let Some(turn) = user_text(&record) {
                turns.push(turn);
            }
        }
    }
    if is_slash_command(text) {
        let command = text.split_whitespace().next().unwrap_or("");
        if chunk.contains(&format!("<command-name>{command}</command-name>")) {
            return SubmitVerdict::Landed;
        }
    } else if turns.iter().any(|turn| turn == text) {
        return SubmitVerdict::Landed;
    }
    if turns
        .iter()
        .any(|turn| turn != text && turn.ends_with(text) && !turn.contains("<command-name>"))
    {
        return SubmitVerdict::Corrupted;
    }
    SubmitVerdict::None
}

/// Type *text* into the engine's composer and press Enter.
///
/// The composer is cleared first (C-u), so an unsent draft can never be
/// concatenated onto the delivered text — and so a re-type after a lost
/// keystroke is idempotent rather than doubled. Readiness is not a sleep:
/// the text is typed, then the attach stream is polled until the composer
/// echoes it back, which is the proof that the attach client is forwarding
/// stdin; a slice without an echo re-types.
///
/// A real draft the C-u killed is pasted back (C-y) once the submit is
/// confirmed: claude parks the killed text on its kill ring, so the engine
/// itself restores the exact bytes. Gated by the dim-aware draft parser
/// (autocomplete ghost text never counts, and a C-u that killed nothing must
/// not paste whatever the ring held before) and forfeited on a re-type (the
/// second C-u overwrites the single-slot ring with our own text).
///
/// ponytail: two pipes typing into the same job at once (a cvim sendback and
/// a hand-run `hive inject`) interleave — one of them wins the composer and
/// the other fails loudly on the transcript compare, never silently.
/// Serialize with an flock under `hive-control/<jobId>.lock` if that ever
/// bites.
pub fn type_into_job(job_id: &str, text: &str, claude_bin: &str) -> KeyResult {
    if job_id.is_empty() || text.is_empty() {
        return KeyResult::failure(if job_id.is_empty() {
            "no job id"
        } else {
            "nothing to type"
        });
    }
    let Some(mut proc) = attach_pipe(job_id, claude_bin) else {
        return KeyResult::failure(format!("could not run `{claude_bin} attach {job_id}`"));
    };
    let result = type_inner(&mut proc, job_id, text);
    close_pipe(proc);
    result
}

fn type_inner(proc: &mut Client, job_id: &str, text: &str) -> KeyResult {
    let Some(engine) = hooked_wait_engine_behind(job_id, proc) else {
        return KeyResult::failure(format!("job {job_id} has no engine (removed?)"));
    };
    let (transcript, offset) = hooked_transcript_cursor(Some(&engine));
    if !hooked_wait_client_ready(proc) {
        return KeyResult::failure(format!("`attach {job_id}` never came up"));
    }

    let draft = hooked_composer_draft(job_id);
    let needles = echo_needles(text);
    let ready = Duration::from_secs_f64(type_ready_timeout().max(0.0));
    let retry = type_retry_after().max(0.0);
    let start = Instant::now();
    let mut next_retype: Option<Instant> = None;
    let mut clears = 0u32;
    let mut echoed = false;
    let mut mark = proc.mark();
    while start.elapsed() < ready {
        if next_retype.is_none_or(|t| Instant::now() >= t) {
            mark = proc.mark(); // only output after this counts as our echo
            if !clear_composer(proc) || !feed(proc, text) {
                return KeyResult::failure("the attach client closed its stdin");
            }
            clears += 1;
            next_retype = Some(Instant::now() + Duration::from_secs_f64(retry));
        }
        let screen = squash(&strip_ansi(&proc.text_since(mark)));
        if needles.is_empty() || needles.iter().any(|n| screen.contains(n.as_str())) {
            echoed = true;
            break;
        }
        sleep_s(ECHO_POLL_INTERVAL);
    }
    let restore = draft && clears == 1;
    if !echoed {
        return KeyResult::failure(format!(
            "job {job_id} never echoed the typed text back into its composer"
        ));
    }
    if !feed(proc, SUBMIT) {
        return KeyResult::failure("the attach client closed its stdin before Enter");
    }
    if transcript.is_none() {
        if restore {
            restore_draft(proc);
        }
        return KeyResult::success("written", "no transcript to confirm against");
    }
    let slash = is_slash_command(text);
    let confirm = Duration::from_secs_f64(
        (if slash {
            slash_confirm_timeout()
        } else {
            submit_confirm_timeout()
        })
        .max(0.0),
    );
    let confirm_start = Instant::now();
    while confirm_start.elapsed() < confirm {
        match submit_verdict(transcript.as_deref(), offset, text) {
            SubmitVerdict::Landed => {
                if restore {
                    restore_draft(proc);
                }
                return KeyResult::success("transcript", "");
            }
            SubmitVerdict::Corrupted => {
                return KeyResult::failure(format!(
                    "job {job_id} submitted the text with a leftover draft in front of it"
                ));
            }
            SubmitVerdict::None => {}
        }
        sleep_s(KEY_POLL_INTERVAL);
    }
    if slash {
        // ponytail: a slash command's record comes late (or never — /cost and
        // other UI-only commands write none), so silence here is not evidence
        // of failure; the composer echo already proved the client was
        // forwarding, and a command swallowed as text would have shown up as
        // a turn by now. If a lost `/compact` ever needs catching, the
        // missing signal is "the composer emptied after Enter".
        if restore {
            restore_draft(proc);
        }
        return KeyResult::success("written", "a slash command with no transcript record yet");
    }
    KeyResult::failure(format!(
        "job {job_id} took the text but no matching turn reached its transcript"
    ))
}

fn hooked_rename(sock: &str, name: &str, session_id: &str) -> bool {
    #[cfg(test)]
    {
        if let Some(Some(v)) = testhook::with(|h| {
            if h.rename_result.is_some() {
                h.renames
                    .push((sock.to_string(), name.to_string(), session_id.to_string()));
            }
            h.rename_result
        }) {
            return v;
        }
    }
    crate::adapters::claude_sessions::rename(sock, name, session_id)
}

/// Make the job's own label read *name*; true when it already did or now does.
///
/// A job minted before hive knew whose pane it was on carries a placeholder
/// (`hive-<pane>`): `hive create` and `hive join` tag the pane the human's
/// claude already runs in, and a `--resume` relaunch keeps the job's old
/// label, so the mint cannot see a tag that does not exist yet. The rename is a
/// `control/rename` frame on the engine's inbox socket: the dispatcher
/// handles it immediately — mid-turn included — and it never touches the
/// composer, so a human's draft and a running turn are left alone. The
/// registry flip confirms it, the same oracle the agents panel reads.
///
/// The name is not cosmetic: the view probe recognizes a session on screen
/// by matching the panel title against member names, so a placeholder-named
/// member reads as a stranger in its own pane.
pub fn ensure_job_named(job_id: &str, name: &str) -> bool {
    if job_id.is_empty() || name.is_empty() {
        return false;
    }
    let Some(engine) = hooked_engine_for_job(job_id) else {
        return false;
    };
    if engine.name == name {
        return true;
    }
    if !hooked_rename(&engine.socket_path, name, &engine.session_id) {
        return false;
    }
    let deadline = Instant::now() + Duration::from_secs_f64(rename_confirm_timeout().max(0.0));
    while Instant::now() < deadline {
        if let Some(refreshed) = hooked_engine_for_job(job_id) {
            if refreshed.name == name {
                return true;
            }
        }
        sleep_s(rename_poll_interval());
    }
    false
}

/// Send Escape to the engine — interrupt whatever turn is running.
///
/// Escape leaves no composer echo, so the readiness gate the typing path
/// uses does not apply, and Escape is never repeated: a second one lands on
/// the engine's "edit previous message" chord. It is written once, then
/// confirmed against the transcript's interrupt marker or the engine leaving
/// `busy`. An engine that was never busy has nothing to interrupt and
/// nothing that could confirm one: that returns right away, a success with
/// `written` — not a failure, and not a wait.
pub fn interrupt_job(job_id: &str, claude_bin: &str) -> KeyResult {
    if job_id.is_empty() {
        return KeyResult::failure("no job id");
    }
    let Some(mut proc) = attach_pipe(job_id, claude_bin) else {
        return KeyResult::failure(format!("could not run `{claude_bin} attach {job_id}`"));
    };
    let result = interrupt_inner(&mut proc, job_id);
    close_pipe(proc);
    result
}

fn interrupt_inner(proc: &mut Client, job_id: &str) -> KeyResult {
    let Some(engine) = hooked_wait_engine_behind(job_id, proc) else {
        return KeyResult::failure(format!("job {job_id} has no engine (removed?)"));
    };
    let (transcript, offset) = hooked_transcript_cursor(Some(&engine));
    let was_busy = engine.status == "busy";
    if !hooked_wait_client_ready(proc) {
        return KeyResult::failure(format!("`attach {job_id}` never came up"));
    }
    if !feed(proc, ESCAPE) {
        return KeyResult::failure("the attach client closed its stdin");
    }
    if !was_busy {
        // Nothing was running, so nothing can confirm: waiting out the
        // window could only relabel a success. cvim sends this before every
        // sendback, and the member is idle most of the time.
        sleep_s(CONTROL_KEY_GAP); // let the client forward it before EOF
        return KeyResult::success("written", "the engine was not busy");
    }
    let confirm = Duration::from_secs_f64(interrupt_confirm_timeout().max(0.0));
    let start = Instant::now();
    while start.elapsed() < confirm {
        if transcript_since(transcript.as_deref(), offset).contains(INTERRUPT_MARKER) {
            return KeyResult::success("transcript", "");
        }
        if let Some(current) = hooked_engine_for_job(job_id) {
            if current.status != "busy" {
                return KeyResult::success("status", "");
            }
        }
        sleep_s(KEY_POLL_INTERVAL);
    }
    KeyResult::failure(format!("job {job_id} is still busy after Escape"))
}

/// Best-effort `claude stop` — parks the job (still in `--all`, still
/// wakeable); never fails loudly.
pub fn stop_job(job_id: &str, claude_bin: &str) {
    if job_id.is_empty() {
        return;
    }
    let argv = vec![
        claude_bin.to_string(),
        "stop".to_string(),
        job_id.to_string(),
    ];
    let _ = run_capture(&argv, AGENTS_TIMEOUT, None, &bg_env(None));
}
