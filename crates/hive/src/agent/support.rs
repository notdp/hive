use std::time::{Duration, Instant};

use anyhow::bail;

use super::seams::*;
#[cfg(test)]
use super::testhook;

pub const AGENT_STARTUP_TIMEOUT: f64 = 90.0;
pub(super) const TMUX_REQUIRED_MESSAGE: &str =
    "Hive requires tmux. Start or attach to a tmux session first.";

/// Where a workflow node's task landed, as the engine handed it back:
/// the id the engine's own turn-end signal is read under. `Untracked` is
/// a task the engine took but handed no id back for — running, with
/// nothing to read its result under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnHandle {
    Codex { thread_id: String, turn_id: String },
    Grok { key: String, rid: u64 },
    Untracked(String),
}

/// Escape a string for safe shell use.
pub(crate) fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn resolve_session_id_from_runtime(pane_id: &str) -> Option<String> {
    let resolved_pane = if !pane_id.is_empty() {
        pane_id.to_string()
    } else {
        crate::identity::current_pane_id().unwrap_or_default()
    };
    if resolved_pane.is_empty() {
        return None;
    }
    hooked_resolve_session_id_for_pane(&resolved_pane)
}

/// Best-effort lookup for the current pane's agent session ID.
pub fn detect_current_session_id(pane_id: &str) -> Option<String> {
    resolve_session_id_from_runtime(pane_id)
}

/// A native transport (codex daemon / grok leader / claude inbox) did not accept the
/// message. Normal hive delivery never falls back to keystrokes; callers
/// surface this as an explicit submit failure (the hived answers `hive send`
/// with `ok: false, error: "transport refused ..."`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryError(pub String);

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for DeliveryError {}

/// Submit text to an interactive agent TUI, preserving any pending draft.
pub(crate) fn submit_interactive_text(pane_id: &str, text: &str, cli: &str) -> anyhow::Result<()> {
    let profile_name = resolve_profile_name(pane_id, cli);
    if profile_name == "claude" {
        let job_id = hooked_job_id_for_pane(pane_id).unwrap_or_default();
        if !job_id.is_empty() {
            // A claude member's keyboard is the job, not the pane: hive pipes
            // the keystrokes into `claude attach <jobId>` itself. Nothing here
            // touches tmux — the pane's viewer is a screen, and what the human
            // has it showing (another session, the panel list, nothing at all)
            // cannot misroute or block a delivery.
            let result = hooked_type_into_job(&job_id, text);
            if !result.ok {
                bail!("claude job {job_id} did not take the text: {}", result.why);
            }
            return Ok(());
        }
        // No job record: an interactive claude TUI on the pane tty, typed at
        // through tmux like any other CLI. Refuse rather than type into the
        // pane shell when that TUI is not running — or into an attach viewer,
        // whose composer belongs to whatever session it is showing.
        if hooked_interactive_claude_pid(pane_id).is_none() {
            bail!("no interactive claude process on pane {pane_id} to receive keystrokes");
        }
    }

    if hooked_is_pane_in_mode(pane_id) {
        hooked_cancel_pane_mode(pane_id);
        hooked_sleep(0.05);
    }

    let buffer_name = save_and_clear_draft(pane_id, &profile_name);

    hooked_send_keys(pane_id, text, false)?;
    hooked_sleep(0.05);
    hooked_send_key(pane_id, "Enter")?;

    if !buffer_name.is_empty() {
        restore_draft(pane_id, &profile_name, &buffer_name);
    }
    Ok(())
}

/// Best-effort: if a draft exists, save it to a tmux buffer and clear input.
///
/// Returns the buffer name to restore later, or '' when there is no draft.
/// The composer is only cleared once tmux confirms the draft is in the
/// buffer — a save that did not happen must not cost the user the draft —
/// and a clear that failed halfway still reports its buffer so the restore
/// pastes it back.
pub(crate) fn save_and_clear_draft(pane_id: &str, profile_name: &str) -> String {
    if !hooked_supported_profile(profile_name) {
        return String::new();
    }
    let buffer_name = format!("hive_draft_{}", pane_id.replace('%', ""));
    let draft_text = match hooked_parse_draft(pane_id, profile_name) {
        Ok(text) => text,
        Err(_) => return String::new(),
    };
    if draft_text.is_empty() {
        return String::new();
    }
    if hooked_load_buffer(&buffer_name, &draft_text).is_err() {
        return String::new();
    }
    if hooked_clear_input(pane_id, profile_name).is_ok() {
        let _ = hooked_wait_input_empty(pane_id, profile_name, 1.0);
    }
    buffer_name
}

fn restore_draft(pane_id: &str, profile_name: &str, buffer_name: &str) {
    if hooked_wait_input_empty(pane_id, profile_name, 2.0).is_ok() {
        hooked_paste_buffer(buffer_name, pane_id, true);
    }
    hooked_delete_buffer(buffer_name);
}

/// Prefer runtime detection; fall back to the declared cli.
fn resolve_profile_name(pane_id: &str, cli: &str) -> String {
    #[cfg(test)]
    if let Some(Some(name)) = testhook::with(|h| h.resolve_profile_name.clone()) {
        return name;
    }
    let mut profile = crate::agent_cli::detect_profile_for_pane(pane_id);
    if profile.is_none() && !cli.is_empty() {
        profile = crate::agent_cli::get_profile(cli);
    }
    match profile {
        Some(p) => p.name.to_string(),
        None => cli.to_string(),
    }
}

/// Wait for the codex TUI process to appear on the pane's TTY.
///
/// The pane's thread identity is minted (and recorded) before the launch
/// command runs, so readiness is just the TUI being up — process evidence,
/// no screen scraping. Best-effort like the banner wait it replaces: a
/// timeout is not fatal.
pub(crate) fn wait_codex_attached(pane_id: &str, timeout: f64, interval: f64) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
    loop {
        if let Some(profile) = hooked_detect_cli_process_for_pane(pane_id) {
            if profile.name == "codex" {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        hooked_sleep(interval);
    }
}

/// Wait for the pane's grok TUI to be up on the member's session.
///
/// The session directory `$GROK_HOME/sessions/<quoted cwd>/<sid>/` is the
/// session's on-disk trace — the leader's for a minted session
/// (`session/new`), the TUI's for a fork — and the cwd segment is grok's
/// own encoding, so the session is matched by id under any of them. The
/// directory alone is not readiness: for a minted or resumed session it is
/// expected on disk before the pane runs anything, so readiness also needs
/// a live grok process on the pane; no screen scraping either way.
/// "Expected" is an assumption, not a verified fact: whether the leader
/// writes the directory eagerly at `session/new` or lazily at the first
/// prompt is observable only live (codex 0.153.2 turned lazy on its
/// rollout), and a lazy write would hold this wait to its timeout and skip
/// the hived's eager connect. The grok acceptance run is the oracle: spawn
/// time well under `AGENT_STARTUP_TIMEOUT`, `connect-grok` connected.
/// Best-effort like the codex thread wait: a timeout is not fatal.
pub(crate) fn wait_grok_session_ready(
    pane_id: &str,
    session_id: &str,
    timeout: f64,
    interval: f64,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
    loop {
        let sessions_dir = crate::adapters::grok_leader::grok_home().join("sessions");
        let mut found = false;
        if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
            for entry in entries.flatten() {
                if entry.path().join(session_id).exists() {
                    found = true;
                    break;
                }
            }
        }
        if found {
            if let Some(profile) = hooked_detect_cli_process_for_pane(pane_id) {
                if profile.name == "grok" {
                    return true;
                }
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        hooked_sleep(interval);
    }
}

pub(crate) fn uuid4() -> String {
    let mut bytes = [0u8; 16];
    let read = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes));
    if read.is_err() {
        // ponytail: sha256(time, pid) fallback — /dev/urandom exists everywhere hive runs
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(format!(
            "{:?}-{}",
            std::time::SystemTime::now(),
            std::process::id()
        ));
        bytes.copy_from_slice(&hasher.finalize()[..16]);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}
