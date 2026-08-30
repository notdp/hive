//! Claude background jobs: the engine behind a hive claude member.
//!
//! A claude member is a `claude --bg` job. The engine (a full Claude Code TUI
//! on a pty owned by claude's own supervisor daemon, argv `claude bg-spare`)
//! runs outside tmux; the member's pane only shows it through a `claude attach
//! <jobId>` viewer, so the pane process table says nothing about the member's
//! life. Identity is the jobId — durable across engine restarts, wakes and
//! upgrades (the engine pid is not). Which job belongs to which tmux pane is
//! recorded in a per-pane `.job` file under the claude config tree, written by
//! whoever binds the pane to a job (spawn, managed launch, fork) — the same
//! shape as codex's pane `.thread` records.
//!
//! Signal surfaces (2.1.240 real-machine verified):
//!
//! - `<claude-config>/sessions/<enginePid>.json`: the live engine's registry
//!   entry — `kind:"bg"`, `jobId`, `status` (idle|busy|waiting; not a
//!   documented enum), `waitingFor` (only while waiting), `statusUpdatedAt`,
//!   `sessionId`, `messagingSocketPath`. The attach viewer never registers.
//! - `claude agents --json --all`: the durable job ledger. A sleeping engine
//!   (supervisor parks jobs idle ~1h) or a stopped one keeps its row but loses
//!   `pid`/`status` — that field absence is the asleep-vs-dead separator.
//!   ~270ms per call, so it runs only on resolution misses, never per tick.
//! - `claude attach <jobId>` with no tty (stdin /dev/null) prints "Waking…"
//!   and exits 0 after reviving a parked/stopped engine — new pid, same
//!   jobId/sessionId. That is the wake primitive delivery self-heals with.
//!
//! Hidden claude subcommands are only recognized at argv[1], so every
//! invocation here calls the binary directly with the subcommand first. Spawn
//! env is washed of CLAUDE*/ANTHROPIC* vars: an inherited
//! `CLAUDE_CODE_CHILD_SESSION` marker makes the engine skip registration
//! entirely (invisible to `agents --json` and undeliverable).

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::adapters::base::SessionAdapter;
use crate::adapters::claude::ClaudeAdapter;
use crate::adapters::claude_sessions::{_config_dir, _pid_alive, _registry_dir};

const _AGENTS_TIMEOUT: f64 = 10.0; // observed ~270ms; the cap only bounds a hung CLI
const _SPAWN_TIMEOUT: f64 = 60.0;
const _WAKE_TIMEOUT: f64 = 20.0; // observed ~2-6s including a fresh supervisor start
const _WAKE_ENTRY_TIMEOUT: f64 = 5.0; // the wake is synchronous; the entry follows fast
const _ENTRY_POLL_INTERVAL: f64 = 0.3;
/// Worst-case extra submission budget when delivery must wake a parked engine
/// first: one ledger read, the tty-less attach that revives it, and the short
/// entry re-read. The hived folds this into its request budgets.
pub const WAKE_SUBMIT_BUDGET: f64 = _AGENTS_TIMEOUT + _WAKE_TIMEOUT + _WAKE_ENTRY_TIMEOUT;

/// An engine entry whose statusUpdatedAt stopped advancing this long ago is
/// not trusted as busy/waiting truth (wedged engine, clock issues); liveness
/// still holds — the pid check is what proves the process.
pub const STATUS_STALE_AFTER_SECONDS: f64 = 30.0 * 60.0;

/// Job ids observed are 8 lowercase hex chars (the sessionId prefix); accept a
/// small band around that so a format drift upstream does not break resolution.
pub fn looks_like_job_id(value: &str) -> bool {
    let n = value.chars().count();
    (6..=12).contains(&n) && value.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

fn str_of(value: Option<&Value>) -> String {
    // `str(data.get(k) or "")`: string as-is, number rendered, anything else "".
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn sleep_s(secs: f64) {
    #[cfg(test)]
    {
        if testhook::with(|h| h.no_sleep).unwrap_or(false) {
            return;
        }
    }
    if secs > 0.0 {
        thread::sleep(Duration::from_secs_f64(secs));
    }
}

fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// --------------------------------------------------------------------------
// pane <-> job records
// --------------------------------------------------------------------------
fn _control_dir() -> PathBuf {
    _config_dir().join("hive-control")
}

/// Per-pane record of the bg job hive bound to this pane.
pub fn pane_job_path(pane: &str) -> PathBuf {
    let slug = pane.replace('%', "");
    let slug = if slug.is_empty() { "default" } else { &slug };
    _control_dir().join(format!("hive-pane-{slug}.job"))
}

pub fn write_pane_job(
    pane: &str,
    job_id: &str,
    session_id: &str,
    cwd: &str,
) -> std::io::Result<()> {
    let path = pane_job_path(pane);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut doc = Map::new();
    doc.insert("jobId".to_string(), Value::String(job_id.to_string()));
    doc.insert(
        "sessionId".to_string(),
        Value::String(session_id.to_string()),
    );
    doc.insert("cwd".to_string(), Value::String(cwd.to_string()));
    fs::write(path, Value::Object(doc).to_string())
}

/// (job_id, session_id, cwd) recorded for *pane*, or None.
pub fn read_pane_job(pane: &str) -> Option<(String, String, String)> {
    let text = fs::read_to_string(pane_job_path(pane)).ok()?;
    let data: Value = serde_json::from_str(&text).ok()?;
    if !data.is_object() {
        return None;
    }
    let job_id = str_of(data.get("jobId"));
    if job_id.is_empty() {
        return None;
    }
    Some((
        job_id,
        str_of(data.get("sessionId")),
        str_of(data.get("cwd")),
    ))
}

pub fn clear_pane_job(pane: &str) {
    let _ = fs::remove_file(pane_job_path(pane));
}

pub fn job_id_for_pane(pane: &str) -> Option<String> {
    read_pane_job(pane).map(|record| record.0)
}

/// Inverse of [`pane_job_path`]: `hive-pane-19.job` -> `%19`.
fn _pane_from_record_name(name: &str) -> Option<String> {
    let slug = name.strip_prefix("hive-pane-")?.strip_suffix(".job")?;
    if slug.is_empty() || slug == "default" {
        return None;
    }
    Some(format!("%{slug}"))
}

/// Pane ids that currently have a job record on disk.
pub fn list_recorded_panes() -> Vec<String> {
    let root = _control_dir();
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut panes = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(pane) = _pane_from_record_name(name) {
            panes.push(pane);
        }
    }
    panes
}

/// Pane recorded for *job_id*, or None.
///
/// The reverse lookup behind tool-side identity: a `hive` invocation inside a
/// member's tool subprocess carries `CLAUDE_CODE_MESSAGING_SOCKET` naming the
/// engine's inbox, the engine's registry entry names the jobId, and this maps
/// it back to the tmux pane hive bound the job to.
pub fn pane_for_job(job_id: &str) -> Option<String> {
    if job_id.is_empty() {
        return None;
    }
    for pane in list_recorded_panes() {
        if let Some(record) = read_pane_job(&pane) {
            if record.0 == job_id {
                return Some(pane);
            }
        }
    }
    None
}

// --------------------------------------------------------------------------
// engine registry entries (sessions/<enginePid>.json, kind == "bg")
// --------------------------------------------------------------------------
#[derive(Debug, Clone, PartialEq)]
pub struct EngineSession {
    pub pid: i32,
    pub job_id: String,
    pub session_id: String,
    pub socket_path: String,
    pub cwd: String,
    pub status: String,
    pub waiting_for: String,
    pub status_updated_at: f64, // epoch seconds, 0.0 when absent
    pub name: String,           // the job's label, as the panel and ledger show it
}

fn _entry_to_engine(data: &Value) -> Option<EngineSession> {
    if data.get("kind").and_then(Value::as_str) != Some("bg") {
        return None;
    }
    let pid = data.get("pid").and_then(Value::as_i64)?;
    let job_id = str_of(data.get("jobId"));
    let sock = str_of(data.get("messagingSocketPath"));
    if job_id.is_empty() || sock.is_empty() {
        return None;
    }
    if !_pid_alive(pid as i32) || !Path::new(&sock).exists() {
        return None;
    }
    let updated = data
        .get("statusUpdatedAt")
        .and_then(Value::as_f64)
        .map(|raw| raw / 1000.0)
        .unwrap_or(0.0);
    Some(EngineSession {
        pid: pid as i32,
        job_id,
        session_id: str_of(data.get("sessionId")),
        socket_path: sock,
        cwd: str_of(data.get("cwd")),
        status: str_of(data.get("status")),
        waiting_for: str_of(data.get("waitingFor")),
        status_updated_at: updated,
        name: str_of(data.get("name")),
    })
}

/// The live engine's registry entry for *job_id*, or None.
///
/// The engine registers under its own (unstable) pid, so the jobId is found
/// by scanning the registry for the `kind:"bg"` entry naming it. None means
/// no live engine — asleep or dead; [`job_row`] tells them apart.
pub fn engine_session_for_job(job_id: &str) -> Option<EngineSession> {
    if job_id.is_empty() {
        return None;
    }
    let root = _registry_dir();
    let entries = fs::read_dir(&root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(data) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if !data.is_object() {
            continue;
        }
        if let Some(engine) = _entry_to_engine(&data) {
            if engine.job_id == job_id {
                return Some(engine);
            }
        }
    }
    None
}

/// The seam the Python tests monkeypatch as `m.engine_session_for_job`; every
/// in-module caller routes through it so a hooked lookup behaves the same.
fn hooked_engine_for_job(job_id: &str) -> Option<EngineSession> {
    #[cfg(test)]
    {
        if let Some(Some(v)) = testhook::with(|h| {
            if h.forbid_engine_lookup {
                panic!("an idle engine must not be polled");
            }
            h.engine_for_job.as_mut().map(|queue| {
                if queue.len() > 1 {
                    queue.pop_front().unwrap()
                } else {
                    queue.front().cloned().unwrap_or(None)
                }
            })
        }) {
            return v;
        }
    }
    engine_session_for_job(job_id)
}

/// The bg engine entry registered under *pid*, or None (viewer pids and
/// interactive sessions have no bg entry).
pub fn engine_session_for_pid(pid: u32) -> Option<EngineSession> {
    let text = fs::read_to_string(_registry_dir().join(format!("{pid}.json"))).ok()?;
    let data: Value = serde_json::from_str(&text).ok()?;
    if !data.is_object() {
        return None;
    }
    _entry_to_engine(&data)
}

/// True when *pane* records a job whose engine is live right now.
///
/// False also covers a parked (asleep) engine — asleep is not dead, but the
/// cheap per-tick probes must not pay the `agents --all` cost; callers that
/// need the third state use [`job_row`].
pub fn pane_engine_alive(pane: &str) -> bool {
    match job_id_for_pane(pane) {
        Some(job_id) if !job_id.is_empty() => hooked_engine_for_job(&job_id).is_some(),
        _ => false,
    }
}

/// Transcript session id of the pane's recorded job.
///
/// The live engine's registry entry is current truth (an in-session `/clear`
/// rotates it); the record's spawn-time snapshot answers for a parked engine
/// — wake preserves the sessionId, so the snapshot stays valid.
pub fn session_id_for_pane(pane: &str) -> Option<String> {
    let record = read_pane_job(pane)?;
    if let Some(engine) = hooked_engine_for_job(&record.0) {
        if !engine.session_id.is_empty() {
            return Some(engine.session_id);
        }
    }
    if record.1.is_empty() {
        None
    } else {
        Some(record.1)
    }
}

// --------------------------------------------------------------------------
// job ledger (claude agents --json --all) and lifecycle
// --------------------------------------------------------------------------

/// Environment for claude bg invocations.
///
/// CLAUDE*/ANTHROPIC* vars are washed: an inherited
/// `CLAUDE_CODE_CHILD_SESSION` makes the engine skip registration — invisible
/// and undeliverable. The config-tree override survives as
/// `CLAUDE_CONFIG_DIR` so a sandboxed lane's engine registers in the same
/// tree hive reads.
pub fn bg_env(extra: Option<&HashMap<String, String>>) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars()
        .filter(|(k, _)| {
            !(k.starts_with("CLAUDE")
                || k.starts_with("ANTHROPIC")
                // the spawner may be another member's engine: its identity must
                // not leak into this job (members get their own via extra)
                || k == "HIVE_TEAM"
                || k == "HIVE_MEMBER")
        })
        .collect();
    let config = _config_dir();
    let home_default = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude");
    if config != home_default {
        env.insert(
            "CLAUDE_CONFIG_DIR".to_string(),
            config.to_string_lossy().into_owned(),
        );
    }
    if let Some(extra) = extra {
        for (k, v) in extra {
            env.insert(k.clone(), v.clone());
        }
    }
    env
}

/// `subprocess.run(argv, capture_output=True, timeout=...)`: (returncode,
/// stdout, stderr), or None when the call itself failed or timed out.
fn run_capture(
    argv: &[String],
    timeout: f64,
    cwd: Option<&str>,
    env: &HashMap<String, String>,
) -> Option<(i32, String, String)> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().ok()?;
    let mut out_pipe = child.stdout.take()?;
    let mut err_pipe = child.stderr.take()?;
    let out_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = out_thread.join().unwrap_or_default();
                let stderr = err_thread.join().unwrap_or_default();
                return Some((
                    status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&stdout).into_owned(),
                    String::from_utf8_lossy(&stderr).into_owned(),
                ));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

/// All job rows from `claude agents --json --all`; None when the CLI call
/// itself failed (distinct from an empty ledger).
pub fn list_jobs(claude_bin: &str) -> Option<Vec<Map<String, Value>>> {
    let argv: Vec<String> =
        ["agents", "--json", "--all"]
            .iter()
            .fold(vec![claude_bin.to_string()], |mut acc, a| {
                acc.push(a.to_string());
                acc
            });
    let (code, stdout, _stderr) = run_capture(&argv, _AGENTS_TIMEOUT, None, &bg_env(None))?;
    if code != 0 {
        return None;
    }
    let rows: Value = serde_json::from_str(&_strip_ansi(&stdout)).ok()?;
    let rows = rows.as_array()?;
    Some(
        rows.iter()
            .filter_map(|row| row.as_object().cloned())
            .collect(),
    )
}

fn hooked_list_jobs(claude_bin: &str) -> Option<Vec<Map<String, Value>>> {
    #[cfg(test)]
    {
        if let Some(Some(v)) = testhook::with(|h| h.list_jobs_rows.clone()) {
            return v;
        }
    }
    list_jobs(claude_bin)
}

/// The ledger row for *job_id*, or None (unknown job, or CLI failure).
///
/// A row without `pid`/`status` is a parked or stopped engine — asleep, not
/// dead: `claude attach` wakes it with the same jobId/sessionId.
pub fn job_row(job_id: &str, claude_bin: &str) -> Option<Map<String, Value>> {
    if job_id.is_empty() {
        return None;
    }
    let rows = hooked_list_jobs(claude_bin)?;
    rows.into_iter().find(|row| str_of(row.get("id")) == job_id)
}

pub fn job_exists(job_id: &str, claude_bin: &str) -> bool {
    job_row(job_id, claude_bin).is_some()
}

/// `backgrounded\s*·\s*(\S+)` over the ANSI-stripped spawn stdout.
fn _spawn_announced(plain: &str) -> String {
    let chars: Vec<char> = plain.chars().collect();
    let key: Vec<char> = "backgrounded".chars().collect();
    let n = chars.len();
    let mut idx = 0;
    while idx + key.len() <= n {
        if chars[idx..idx + key.len()] == key[..] {
            let mut i = idx + key.len();
            while i < n && chars[i].is_whitespace() {
                i += 1;
            }
            if i < n && chars[i] == '\u{b7}' {
                i += 1;
                while i < n && chars[i].is_whitespace() {
                    i += 1;
                }
                let start = i;
                while i < n && !chars[i].is_whitespace() {
                    i += 1;
                }
                if i > start {
                    return chars[start..i].iter().collect();
                }
            }
        }
        idx += 1;
    }
    String::new()
}

/// Start a `claude --bg` job; return its jobId, or None on failure.
///
/// *extra_args* are forwarded verbatim (`--model`, `-r <sid> --fork-session`,
/// `--settings` …) and become the job's durable `respawnFlags`, so any
/// path-valued flag must be absolute. The prompt is the positional argument
/// (never `-p`, which `--bg` rejects). An empty *name* adds no `--name` (the
/// caller passed its own in *extra_args*).
pub fn spawn_job(
    cwd: &str,
    name: &str,
    prompt: &str,
    extra_args: &[String],
    extra_env: Option<&HashMap<String, String>>,
    claude_bin: &str,
) -> Option<String> {
    let mut argv = vec![claude_bin.to_string(), "--bg".to_string()];
    if !name.is_empty() {
        argv.push("--name".to_string());
        argv.push(name.to_string());
    }
    argv.extend(extra_args.iter().cloned());
    if !prompt.is_empty() {
        argv.push(prompt.to_string());
    }
    let cwd = if cwd.is_empty() { None } else { Some(cwd) };
    let (code, stdout, _stderr) = run_capture(&argv, _SPAWN_TIMEOUT, cwd, &bg_env(extra_env))?;
    if code != 0 {
        return None;
    }
    let announced = _spawn_announced(&_strip_ansi(&stdout));
    // The announcement is stdout, not a contract: an escape hive does not
    // strip (the FORCE_COLOR class) or a reworded line yields a token no
    // registry row can ever carry as its `jobId`, and the caller would poll
    // for it until the whole startup budget burned. Refuse it here instead.
    if looks_like_job_id(&announced) {
        Some(announced)
    } else {
        None
    }
}

/// Revive a parked/stopped engine without a terminal.
///
/// `claude attach <jobId>` with stdin at /dev/null prints "Waking…", spins
/// the engine back up (new pid, same jobId/sessionId) and exits 0. On a
/// removed job it fails; the caller reads the registry to see the result.
pub fn wake_job(job_id: &str, claude_bin: &str) -> bool {
    if job_id.is_empty() {
        return false;
    }
    let argv = vec![
        claude_bin.to_string(),
        "attach".to_string(),
        job_id.to_string(),
    ];
    match run_capture(&argv, _WAKE_TIMEOUT, None, &bg_env(None)) {
        Some((code, _out, _err)) => code == 0,
        None => false,
    }
}

fn hooked_wake_job(job_id: &str, claude_bin: &str) -> bool {
    #[cfg(test)]
    {
        if let Some(Some(v)) = testhook::with(|h| {
            if h.wake_result.is_some() {
                h.wakes.push(job_id.to_string());
            }
            h.wake_result
        }) {
            return v;
        }
    }
    wake_job(job_id, claude_bin)
}

/// Poll for the engine's registry entry (spawn readiness).
pub fn wait_engine_entry(job_id: &str, timeout: f64) -> Option<EngineSession> {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
    loop {
        if let Some(engine) = hooked_engine_for_job(job_id) {
            return Some(engine);
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep_s(_ENTRY_POLL_INTERVAL);
    }
}

/// The job's live engine entry, waking a parked engine when needed.
///
/// Returns None when no engine came up — the job is gone (removed) or the
/// wake failed; the caller decides whether that is a delivery error.
/// *timeout* None means the wake-entry default.
pub fn ensure_engine(
    job_id: &str,
    timeout: Option<f64>,
    claude_bin: &str,
) -> Option<EngineSession> {
    if let Some(engine) = hooked_engine_for_job(job_id) {
        return Some(engine);
    }
    if !hooked_wake_job(job_id, claude_bin) {
        return None;
    }
    wait_engine_entry(job_id, timeout.unwrap_or(_WAKE_ENTRY_TIMEOUT))
}

// --------------------------------------------------------------------------
// keyboard: piping keystrokes into the engine over `claude attach <jobId>`
// --------------------------------------------------------------------------
// `claude attach` reads stdin even when it is a pipe, so a jobId addresses the
// engine's keyboard the same way it addresses everything else — no tmux, no
// pane, no viewer. A pane viewer stays attached and unflickered while this
// second client types (real-machine verified, 2.1.240), and the attach itself
// wakes a parked engine, so the keyboard path self-heals the ~1h park for free.
const _CLEAR_LINE: &str = "\u{15}"; // C-u: drop whatever is in the composer (claude keeps it
                                    // on its own kill ring — Ctrl+Y pastes it back)
const _RESTORE_KILL: &str = "\u{19}"; // C-y: paste the kill ring back into the composer
const _SUBMIT: &str = "\r";
const _ESCAPE: &str = "\u{1b}"; // interrupts the running turn
                                // Only used when the job is on nobody's screen: claude's own pty host
                                // starts at this size, so it is the least surprising thing to wear.
const _DEFAULT_PTY_COLS: u16 = 200;
const _DEFAULT_PTY_ROWS: u16 = 50;

const _ENGINE_READY_TIMEOUT: f64 = 20.0; // our own attach is the wake; the entry follows it
const _CLIENT_READY_TIMEOUT: f64 = 15.0; // observed ~0.3s to the journal entry
const _TYPE_READY_TIMEOUT: f64 = 25.0; // total budget for "the client is forwarding stdin"
const _TYPE_RETRY_AFTER: f64 = 5.0; // re-type (C-u first, so it is idempotent) after this
const _SUBMIT_CONFIRM_TIMEOUT: f64 = 20.0; // the user turn is written the moment it lands
                                           // A slash command's `<command-name>` record is written when the command
                                           // *finishes* (a /compact can take a minute), so waiting for it would block the
                                           // caller on work it does not need to see. This window only has to be long
                                           // enough for the failure shape — the command submitted as plain text, which
                                           // writes its turn immediately.
const _SLASH_CONFIRM_TIMEOUT: f64 = 5.0;
const _INTERRUPT_CONFIRM_TIMEOUT: f64 = 12.0;
const _KEY_POLL_INTERVAL: f64 = 0.4;
const _ECHO_POLL_INTERVAL: f64 = 0.05; // in-memory read of our own attach stream
const _RENAME_CONFIRM_TIMEOUT: f64 = 5.0; // a control/rename lands in ~0.1s; this is slack
const _RENAME_POLL_INTERVAL: f64 = 0.1; // registry file reads are cheap
const _CONTROL_KEY_GAP: f64 = 0.25; // a control byte must not ride in the text's chunk
const _ATTACH_EXIT_TIMEOUT: f64 = 10.0;

const _ECHO_PREFIX_CHARS: usize = 40; // head/tail slice: unique enough, short enough to survive a wrap
const _PASTE_PLACEHOLDER: &str = "[Pastedtext#"; // squashed `[Pasted text #N]`
const _INTERRUPT_MARKER: &str = "[Request interrupted by user]";

const _BUF_CAP: usize = 262144;

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

tunable!(type_retry_after, type_retry_after, _TYPE_RETRY_AFTER);
tunable!(type_ready_timeout, type_ready_timeout, _TYPE_READY_TIMEOUT);
tunable!(
    slash_confirm_timeout,
    slash_confirm_timeout,
    _SLASH_CONFIRM_TIMEOUT
);
tunable!(
    submit_confirm_timeout,
    submit_confirm_timeout,
    _SUBMIT_CONFIRM_TIMEOUT
);
tunable!(
    interrupt_confirm_timeout,
    interrupt_confirm_timeout,
    _INTERRUPT_CONFIRM_TIMEOUT
);
tunable!(
    rename_confirm_timeout,
    rename_confirm_timeout,
    _RENAME_CONFIRM_TIMEOUT
);
tunable!(
    rename_poll_interval,
    rename_poll_interval,
    _RENAME_POLL_INTERVAL
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
fn _strip_ansi(text: &str) -> String {
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

/// `claude logs` replays the raw pty stream: the layout lives in cursor moves,
/// not in spaces, so whitespace and box drawing are noise for a substring test.
fn _squash(text: &str) -> String {
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
fn _composer_has_draft(job_id: &str) -> bool {
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
    _composer_has_draft(job_id)
}

/// What "the composer is showing *text*" can look like on the pty screen.
///
/// Three shapes, any of which counts: the head of the text, its tail (a long
/// paste scrolls the composer viewport to the cursor, so the head is off
/// screen), and the `[Pasted text #N]` placeholder the TUI folds a long paste
/// into, which carries none of the text at all.
fn _echo_needles(text: &str) -> Vec<String> {
    let squashed = _squash(text);
    if squashed.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = squashed.chars().collect();
    let head: String = chars.iter().take(_ECHO_PREFIX_CHARS).collect();
    let tail: String = chars[chars.len().saturating_sub(_ECHO_PREFIX_CHARS)..]
        .iter()
        .collect();
    let mut needles = Vec::new();
    for needle in [head, tail, _PASTE_PLACEHOLDER.to_string()] {
        if !needles.contains(&needle) {
            needles.push(needle);
        }
    }
    needles
}

/// The job's transcript file and its current size — the offset new records
/// are read from once the submit lands.
fn _transcript_cursor(engine: Option<&EngineSession>) -> (Option<PathBuf>, u64) {
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
    _transcript_cursor(engine)
}

/// Whatever the transcript gained after *offset*.
fn _transcript_since(path: Option<&Path>, offset: u64) -> String {
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

fn _user_text(record: &Value) -> Option<String> {
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

fn _is_slash_command(text: &str) -> bool {
    let stripped = text.trim();
    stripped.starts_with('/') && !stripped.contains('\n')
}

/// What the transcript says about the submit: `landed`, `corrupted` or
/// `none` (nothing yet — keep waiting).
///
/// A slash command lands as a `<command-name>` entry: the engine ran the
/// command instead of sending its literal text to the model. Anything else
/// lands as a user turn whose content equals what was typed *exactly*.
/// `corrupted` is the case exact matching exists for: a turn that ends with
/// the typed text but carries something in front of it is a leftover
/// composer draft that got submitted along with the delivery — the one thing
/// a substring match would wave through.
fn _submit_verdict(path: Option<&Path>, offset: u64, text: &str) -> &'static str {
    let chunk = _transcript_since(path, offset);
    if chunk.is_empty() {
        return "none";
    }
    let mut turns: Vec<String> = Vec::new();
    for line in chunk.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue; // a half-written tail line; the next poll sees it whole
        };
        if record.is_object() {
            if let Some(turn) = _user_text(&record) {
                turns.push(turn);
            }
        }
    }
    if _is_slash_command(text) {
        let command = text.trim().split_whitespace().next().unwrap_or("");
        if chunk.contains(&format!("<command-name>{command}</command-name>")) {
            return "landed";
        }
    } else if turns.iter().any(|turn| turn == text) {
        return "landed";
    }
    if turns
        .iter()
        .any(|turn| turn != text && turn.ends_with(text) && !turn.contains("<command-name>"))
    {
        return "corrupted";
    }
    "none"
}

// --------------------------------------------------------------------------
// the attach client
// --------------------------------------------------------------------------

struct DrainBuf {
    data: Vec<u8>,
    seen: usize,
}

/// A `claude attach` client on a pty, wearing the size already on screen.
///
/// The engine's pty follows whatever client is attached, so a client with no
/// tty drags it to a default the moment it connects and back when it leaves —
/// measured on a real engine: 180 columns, 120 while a tty-less pipe was
/// attached, 180 again after. Wearing the viewer's own size makes the
/// connection invisible instead.
pub(crate) struct RealClient {
    child: Child,
    pid: i32,
    // The attached TUI paints continuously; an undrained master fills its
    // buffer and blocks the engine's writes. The drained bytes are the
    // engine's own screen — kept (bounded) so the echo check reads them
    // in-memory instead of shelling out to `claude logs` per poll.
    fd: Arc<AtomicI32>,
    buf: Arc<Mutex<DrainBuf>>,
}

impl RealClient {
    fn new(child: Child, master_fd: i32) -> Self {
        let pid = child.id() as i32;
        let buf = Arc::new(Mutex::new(DrainBuf {
            data: Vec::new(),
            seen: 0,
        }));
        let drain_buf = Arc::clone(&buf);
        thread::spawn(move || {
            let mut chunk = [0u8; 65536];
            loop {
                let n = unsafe {
                    libc::read(
                        master_fd,
                        chunk.as_mut_ptr() as *mut libc::c_void,
                        chunk.len(),
                    )
                };
                if n <= 0 {
                    return;
                }
                let n = n as usize;
                let mut b = drain_buf.lock().unwrap_or_else(|e| e.into_inner());
                b.data.extend_from_slice(&chunk[..n]);
                b.seen += n;
                if b.data.len() > _BUF_CAP {
                    let excess = b.data.len() - _BUF_CAP;
                    b.data.drain(..excess);
                }
            }
        });
        RealClient {
            child,
            pid,
            fd: Arc::new(AtomicI32::new(master_fd)),
            buf,
        }
    }

    fn mark(&self) -> usize {
        self.buf.lock().unwrap_or_else(|e| e.into_inner()).seen
    }

    fn text_since(&self, mark: usize) -> String {
        let b = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        let kept = b.data.len();
        let start = kept.saturating_sub(b.seen.saturating_sub(mark));
        String::from_utf8_lossy(&b.data[start..]).into_owned()
    }

    fn write_str(&self, payload: &str) -> std::io::Result<()> {
        let fd = self.fd.load(Ordering::SeqCst);
        if fd < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "attach client closed",
            ));
        }
        let bytes = payload.as_bytes();
        let mut off = 0;
        while off < bytes.len() {
            let n = unsafe {
                libc::write(
                    fd,
                    bytes[off..].as_ptr() as *const libc::c_void,
                    bytes.len() - off,
                )
            };
            if n < 0 {
                return Err(std::io::Error::last_os_error());
            }
            off += n as usize;
        }
        Ok(())
    }

    fn close_stdin(&self) {
        let fd = self.fd.swap(-1, Ordering::SeqCst);
        if fd >= 0 {
            unsafe {
                libc::close(fd);
            }
        }
    }

    fn poll(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            _ => None,
        }
    }

    fn wait_timeout(&mut self, secs: f64) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs_f64(secs.max(0.0));
        loop {
            if let Some(code) = self.poll() {
                return Some(code);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

/// The keystroke path's view of an attach client; the fake variant is what
/// the Python tests' FakePipe stood in for.
pub(crate) enum Client {
    Real(RealClient),
    #[cfg(test)]
    Fake(testhook::FakePipe),
}

impl Client {
    fn pid(&self) -> i32 {
        match self {
            Client::Real(c) => c.pid,
            #[cfg(test)]
            Client::Fake(f) => f.pid(),
        }
    }

    fn mark(&self) -> usize {
        match self {
            Client::Real(c) => c.mark(),
            #[cfg(test)]
            Client::Fake(f) => f.mark(),
        }
    }

    fn text_since(&self, mark: usize) -> String {
        match self {
            Client::Real(c) => c.text_since(mark),
            #[cfg(test)]
            Client::Fake(f) => f.text_since(mark),
        }
    }

    fn write_str(&mut self, payload: &str) -> std::io::Result<()> {
        match self {
            Client::Real(c) => c.write_str(payload),
            #[cfg(test)]
            Client::Fake(f) => f.write_str(payload),
        }
    }

    fn close_stdin(&mut self) {
        match self {
            Client::Real(c) => c.close_stdin(),
            #[cfg(test)]
            Client::Fake(f) => f.close(),
        }
    }

    fn poll(&mut self) -> Option<i32> {
        match self {
            Client::Real(c) => c.poll(),
            #[cfg(test)]
            Client::Fake(f) => f.poll(),
        }
    }

    fn wait_timeout(&mut self, secs: f64) -> Option<i32> {
        match self {
            Client::Real(c) => c.wait_timeout(secs),
            #[cfg(test)]
            Client::Fake(f) => {
                let _ = secs;
                f.wait_timeout()
            }
        }
    }

    fn kill(&mut self) {
        match self {
            Client::Real(c) => c.kill(),
            #[cfg(test)]
            Client::Fake(f) => f.kill(),
        }
    }
}

/// (cols, rows) the engine is rendering at — its viewer pane's size.
///
/// The pane hive bound the job to is the client that set the current size, so
/// matching it means the attach changes nothing. With no pane on record (or
/// no tmux answer) the engine is not on anyone's screen and any size is
/// harmless; the fallback is the size claude's own pty host starts at.
fn _engine_screen_size(job_id: &str) -> (u16, u16) {
    if let Some(pane) = pane_for_job(job_id) {
        if let Some(raw) = crate::tmux::display_value(&pane, "#{pane_width}\t#{pane_height}") {
            let mut parts = raw.splitn(2, '\t');
            let cols = parts.next().unwrap_or("");
            let rows = parts.next().unwrap_or("");
            if !cols.is_empty()
                && !rows.is_empty()
                && cols.chars().all(|c| c.is_ascii_digit())
                && rows.chars().all(|c| c.is_ascii_digit())
            {
                if let (Ok(cols), Ok(rows)) = (cols.parse::<u16>(), rows.parse::<u16>()) {
                    if cols > 0 && rows > 0 {
                        return (cols, rows);
                    }
                }
            }
        }
    }
    (_DEFAULT_PTY_COLS, _DEFAULT_PTY_ROWS)
}

fn _attach_pipe(job_id: &str, claude_bin: &str) -> Option<Client> {
    #[cfg(test)]
    {
        if let Some(Some(pipe)) = testhook::with(|h| h.attach_pipe.clone()) {
            return Some(Client::Fake(pipe));
        }
    }
    let (cols, rows) = _engine_screen_size(job_id);
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    };
    if rc != 0 {
        return None;
    }
    let out_fd = unsafe { libc::dup(slave) };
    let err_fd = unsafe { libc::dup(slave) };
    if out_fd < 0 || err_fd < 0 {
        unsafe {
            libc::close(master);
            libc::close(slave);
            if out_fd >= 0 {
                libc::close(out_fd);
            }
            if err_fd >= 0 {
                libc::close(err_fd);
            }
        }
        return None;
    }
    let mut cmd = Command::new(claude_bin);
    cmd.arg("attach").arg(job_id).env_clear().envs(bg_env(None));
    unsafe {
        use std::os::unix::io::FromRawFd;
        use std::os::unix::process::CommandExt;
        cmd.stdin(Stdio::from_raw_fd(slave));
        cmd.stdout(Stdio::from_raw_fd(out_fd));
        cmd.stderr(Stdio::from_raw_fd(err_fd));
        cmd.pre_exec(|| {
            // the pty is the client's tty, not ours
            libc::setsid();
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(child) => Some(Client::Real(RealClient::new(child, master))),
        Err(_) => {
            unsafe {
                libc::close(master);
            }
            None
        }
    }
}

fn _feed(proc: &mut Client, payload: &str) -> bool {
    proc.write_str(payload).is_ok()
}

/// The engine the pipe is typing into — the attach itself wakes a parked one,
/// so this is also the wake wait. A client that exits first says the job is
/// gone; there is nothing left to wait for.
fn _wait_engine_behind(job_id: &str, proc: &mut Client) -> Option<EngineSession> {
    let deadline = Instant::now() + Duration::from_secs_f64(_ENGINE_READY_TIMEOUT);
    loop {
        if let Some(engine) = hooked_engine_for_job(job_id) {
            return Some(engine);
        }
        if proc.poll().is_some() || Instant::now() >= deadline {
            return None;
        }
        sleep_s(_ENTRY_POLL_INTERVAL);
    }
}

fn hooked_wait_engine_behind(job_id: &str, proc: &mut Client) -> Option<EngineSession> {
    #[cfg(test)]
    {
        if let Some(Some(v)) = testhook::with(|h| h.wait_engine_behind.clone()) {
            return v;
        }
    }
    _wait_engine_behind(job_id, proc)
}

/// Wait until the attach client has the session on screen.
///
/// Its own attach-journal entry says so (~0.3s), and that matters for the
/// control bytes: a `\x15` written into a client that is not in raw key mode
/// yet is inserted into the composer as a literal character instead of
/// clearing it — observed once on 2.1.240, and silent when it happens.
fn _wait_client_ready(proc: &mut Client) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f64(_CLIENT_READY_TIMEOUT);
    while Instant::now() < deadline {
        if proc.poll().is_some() {
            return false;
        }
        if crate::adapters::claude_view::attach_entry_for_pid(proc.pid()).is_some() {
            return true;
        }
        sleep_s(0.1);
    }
    false
}

fn hooked_wait_client_ready(proc: &mut Client) -> bool {
    #[cfg(test)]
    {
        if let Some(Some(v)) = testhook::with(|h| h.client_ready) {
            return v;
        }
    }
    _wait_client_ready(proc)
}

/// C-u, in a chunk of its own — see [`_wait_client_ready`].
fn _clear_composer(proc: &mut Client) -> bool {
    if !_feed(proc, _CLEAR_LINE) {
        return false;
    }
    sleep_s(_CONTROL_KEY_GAP);
    true
}

/// C-y: paste the draft the C-u killed back into the (now empty) composer.
///
/// Best-effort — a failed restore leaves what today's behavior always left,
/// the draft on claude's kill ring with the TUI's own Ctrl+Y hint on screen.
fn _restore_draft(proc: &mut Client) {
    sleep_s(_CONTROL_KEY_GAP);
    if _feed(proc, _RESTORE_KILL) {
        sleep_s(_CONTROL_KEY_GAP); // let the client forward it before EOF
    }
}

/// Let the attach client exit, and make sure it does.
///
/// A wedged client would otherwise outlive the caller holding the pipe open;
/// nothing downstream may block on it — reap off-thread so the caller
/// returns immediately.
fn _close_pipe(mut proc: Client) {
    proc.close_stdin();
    thread::spawn(move || {
        if proc.wait_timeout(_ATTACH_EXIT_TIMEOUT).is_none() {
            proc.kill();
            let _ = proc.wait_timeout(_ATTACH_EXIT_TIMEOUT);
        }
    });
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
    let Some(mut proc) = _attach_pipe(job_id, claude_bin) else {
        return KeyResult::failure(format!("could not run `{claude_bin} attach {job_id}`"));
    };
    let result = _type_inner(&mut proc, job_id, text);
    _close_pipe(proc);
    result
}

fn _type_inner(proc: &mut Client, job_id: &str, text: &str) -> KeyResult {
    let Some(engine) = hooked_wait_engine_behind(job_id, proc) else {
        return KeyResult::failure(format!("job {job_id} has no engine (removed?)"));
    };
    let (transcript, offset) = hooked_transcript_cursor(Some(&engine));
    if !hooked_wait_client_ready(proc) {
        return KeyResult::failure(format!("`attach {job_id}` never came up"));
    }

    let draft = hooked_composer_draft(job_id);
    let needles = _echo_needles(text);
    let ready = Duration::from_secs_f64(type_ready_timeout().max(0.0));
    let retry = type_retry_after().max(0.0);
    let start = Instant::now();
    let mut next_retype: Option<Instant> = None;
    let mut clears = 0u32;
    let mut echoed = false;
    let mut mark = proc.mark();
    while start.elapsed() < ready {
        if next_retype.map_or(true, |t| Instant::now() >= t) {
            mark = proc.mark(); // only output after this counts as our echo
            if !_clear_composer(proc) || !_feed(proc, text) {
                return KeyResult::failure("the attach client closed its stdin");
            }
            clears += 1;
            next_retype = Some(Instant::now() + Duration::from_secs_f64(retry));
        }
        let screen = _squash(&_strip_ansi(&proc.text_since(mark)));
        if needles.is_empty() || needles.iter().any(|n| screen.contains(n.as_str())) {
            echoed = true;
            break;
        }
        sleep_s(_ECHO_POLL_INTERVAL);
    }
    let restore = draft && clears == 1;
    if !echoed {
        return KeyResult::failure(format!(
            "job {job_id} never echoed the typed text back into its composer"
        ));
    }
    if !_feed(proc, _SUBMIT) {
        return KeyResult::failure("the attach client closed its stdin before Enter");
    }
    if transcript.is_none() {
        if restore {
            _restore_draft(proc);
        }
        return KeyResult::success("written", "no transcript to confirm against");
    }
    let slash = _is_slash_command(text);
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
        match _submit_verdict(transcript.as_deref(), offset, text) {
            "landed" => {
                if restore {
                    _restore_draft(proc);
                }
                return KeyResult::success("transcript", "");
            }
            "corrupted" => {
                return KeyResult::failure(format!(
                    "job {job_id} submitted the text with a leftover draft in front of it"
                ));
            }
            _ => {}
        }
        sleep_s(_KEY_POLL_INTERVAL);
    }
    if slash {
        // ponytail: a slash command's record comes late (or never — /cost and
        // other UI-only commands write none), so silence here is not evidence
        // of failure; the composer echo already proved the client was
        // forwarding, and a command swallowed as text would have shown up as
        // a turn by now. If a lost `/compact` ever needs catching, the
        // missing signal is "the composer emptied after Enter".
        if restore {
            _restore_draft(proc);
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
/// (`hive-<pane>`): every path that adopts an existing pane into a team —
/// init, spawn, resume — tags the pane after its CLI is already running, and
/// the mint cannot see a tag that does not exist yet. The rename is a
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
    let Some(mut proc) = _attach_pipe(job_id, claude_bin) else {
        return KeyResult::failure(format!("could not run `{claude_bin} attach {job_id}`"));
    };
    let result = _interrupt_inner(&mut proc, job_id);
    _close_pipe(proc);
    result
}

fn _interrupt_inner(proc: &mut Client, job_id: &str) -> KeyResult {
    let Some(engine) = hooked_wait_engine_behind(job_id, proc) else {
        return KeyResult::failure(format!("job {job_id} has no engine (removed?)"));
    };
    let (transcript, offset) = hooked_transcript_cursor(Some(&engine));
    let was_busy = engine.status == "busy";
    if !hooked_wait_client_ready(proc) {
        return KeyResult::failure(format!("`attach {job_id}` never came up"));
    }
    if !_feed(proc, _ESCAPE) {
        return KeyResult::failure("the attach client closed its stdin");
    }
    if !was_busy {
        // Nothing was running, so nothing can confirm: waiting out the
        // window could only relabel a success. cvim sends this before every
        // sendback, and the member is idle most of the time.
        sleep_s(_CONTROL_KEY_GAP); // let the client forward it before EOF
        return KeyResult::success("written", "the engine was not busy");
    }
    let confirm = Duration::from_secs_f64(interrupt_confirm_timeout().max(0.0));
    let start = Instant::now();
    while start.elapsed() < confirm {
        if _transcript_since(transcript.as_deref(), offset).contains(_INTERRUPT_MARKER) {
            return KeyResult::success("transcript", "");
        }
        if let Some(current) = hooked_engine_for_job(job_id) {
            if current.status != "busy" {
                return KeyResult::success("status", "");
            }
        }
        sleep_s(_KEY_POLL_INTERVAL);
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
    let _ = run_capture(&argv, _AGENTS_TIMEOUT, None, &bg_env(None));
}

// --------------------------------------------------------------------------
// runtime signal mapping (engine status -> hive runtime fields)
// --------------------------------------------------------------------------

/// Fold an engine entry's status into hive runtime fields.
///
/// `status` is the live truth (`state` in the ledger lags); `waiting`
/// carries `waitingFor`. A stale `statusUpdatedAt` demotes the status to
/// unknown instead of trusting a wedged engine's last word.
pub fn runtime_from_engine(engine: &EngineSession, now: Option<f64>) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert(
        "_runtimeSource".to_string(),
        Value::String("claude_bg".to_string()),
    );
    let current = now.unwrap_or_else(now_epoch);
    if engine.status_updated_at != 0.0
        && current - engine.status_updated_at > STATUS_STALE_AFTER_SECONDS
    {
        fields.insert("busy".to_string(), Value::Bool(false));
        fields.insert(
            "inputState".to_string(),
            Value::String("unknown".to_string()),
        );
        fields.insert(
            "inputReason".to_string(),
            Value::String("stale_status".to_string()),
        );
        return fields;
    }
    for (key, value) in
        crate::adapters::claude_sessions::runtime_from_status(&engine.status, &engine.waiting_for)
    {
        fields.insert(key, value);
    }
    fields
}

// --------------------------------------------------------------------------
// test seams (the Rust shape of the Python tests' monkeypatching)
// --------------------------------------------------------------------------
#[cfg(test)]
pub(crate) mod testhook {
    use super::EngineSession;
    use serde_json::{Map, Value};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// Stand-in for the `claude attach` client: records what was written.
    /// Each `text_since` poll drains the next scripted screen frame.
    #[derive(Clone, Default)]
    pub struct FakePipe {
        pub state: Arc<Mutex<FakeState>>,
    }

    #[derive(Default)]
    pub struct FakeState {
        pub writes: Vec<String>,
        pub stream: String,
        pub pending: VecDeque<String>,
        pub broken_after: Option<usize>,
        pub closed: bool,
        pub killed: bool,
        pub hang_wait: bool,
        pub poll: Option<i32>,
    }

    impl FakePipe {
        pub fn pid(&self) -> i32 {
            4242
        }

        pub fn mark(&self) -> usize {
            self.state.lock().unwrap().stream.chars().count()
        }

        pub fn text_since(&self, mark: usize) -> String {
            let mut st = self.state.lock().unwrap();
            if let Some(frame) = st.pending.pop_front() {
                st.stream.push_str(&frame);
            }
            st.stream.chars().skip(mark).collect()
        }

        pub fn write_str(&self, payload: &str) -> std::io::Result<()> {
            let mut st = self.state.lock().unwrap();
            if let Some(after) = st.broken_after {
                if st.writes.len() >= after {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "client gone",
                    ));
                }
            }
            st.writes.push(payload.to_string());
            Ok(())
        }

        pub fn close(&self) {
            self.state.lock().unwrap().closed = true;
        }

        pub fn poll(&self) -> Option<i32> {
            self.state.lock().unwrap().poll
        }

        pub fn wait_timeout(&self) -> Option<i32> {
            if self.state.lock().unwrap().hang_wait {
                None
            } else {
                Some(0)
            }
        }

        pub fn kill(&self) {
            self.state.lock().unwrap().killed = true;
        }
    }

    #[derive(Default)]
    pub struct Hook {
        pub attach_pipe: Option<FakePipe>,
        pub client_ready: Option<bool>,
        pub wait_engine_behind: Option<Option<EngineSession>>,
        /// Pop per call; the last value repeats (a constant monkeypatch is a
        /// one-element sequence, `iter([...])` a longer one).
        pub engine_for_job: Option<VecDeque<Option<EngineSession>>>,
        pub forbid_engine_lookup: bool,
        pub transcript_cursor: Option<(Option<PathBuf>, u64)>,
        pub composer_draft: Option<bool>,
        pub pane_for_job: Option<Option<String>>,
        /// Ok((job_id, certainty)) or Err(()) for "the probe blew up".
        pub view_probe: Option<Result<(String, String), ()>>,
        pub suspected_draft: Option<bool>,
        pub suspected_calls: Vec<(String, String)>,
        pub wake_result: Option<bool>,
        pub wakes: Vec<String>,
        pub rename_result: Option<bool>,
        pub renames: Vec<(String, String, String)>,
        pub list_jobs_rows: Option<Option<Vec<Map<String, Value>>>>,
        pub no_sleep: bool,
        pub type_retry_after: Option<f64>,
        pub type_ready_timeout: Option<f64>,
        pub slash_confirm_timeout: Option<f64>,
        pub submit_confirm_timeout: Option<f64>,
        pub interrupt_confirm_timeout: Option<f64>,
        pub rename_confirm_timeout: Option<f64>,
        pub rename_poll_interval: Option<f64>,
    }

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = RefCell::new(None);
    }

    pub fn with<T>(f: impl FnOnce(&mut Hook) -> T) -> Option<T> {
        HOOK.with(|h| h.borrow_mut().as_mut().map(f))
    }

    pub struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            HOOK.with(|h| *h.borrow_mut() = None);
        }
    }

    pub fn install(hook: Hook) -> Guard {
        HOOK.with(|h| *h.borrow_mut() = Some(hook));
        Guard
    }
}

#[cfg(test)]
mod tests {
    use super::testhook::{FakePipe, Hook};
    use super::*;
    use crate::adapters::claude_sessions::test_env;
    use serde_json::json;
    use std::collections::VecDeque;

    // --- fixtures -----------------------------------------------------------

    /// An isolated claude config tree (the Python `_claude_home` fixture).
    struct Home {
        config: PathBuf,
        dir: tempfile::TempDir,
        _env: test_env::EnvGuard,
    }

    fn claude_home() -> Home {
        let env_guard = test_env::EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("claude-home");
        std::env::set_var("CLAUDE_HOME", &config);
        Home {
            config,
            dir,
            _env: env_guard,
        }
    }

    /// Set an env var for the test's lifetime, restoring the old value.
    struct VarGuard(&'static str, Option<String>);

    impl VarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            std::env::set_var(key, value);
            VarGuard(key, old)
        }
    }

    impl Drop for VarGuard {
        fn drop(&mut self) {
            match &self.1 {
                Some(v) => std::env::set_var(self.0, v),
                None => std::env::remove_var(self.0),
            }
        }
    }

    fn write_registry_entry(home: &Home, file_pid: i64, fields: &Value) {
        let dir = home.config.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{file_pid}.json")), fields.to_string()).unwrap();
    }

    fn bg_entry(pid: i64, job_id: &str, sock: &str, status: &str) -> Value {
        json!({
            "pid": pid,
            "kind": "bg",
            "jobId": job_id,
            "sessionId": format!("{job_id}-ffff-4aaa-8bbb-000000000000"),
            "messagingSocketPath": sock,
            "status": status,
            "statusUpdatedAt": 1_700_000_000_000u64,
        })
    }

    fn me() -> i64 {
        std::process::id() as i64
    }

    fn fake_bin(dir: &Path, script: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("claude");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// A fake claude binary that emits exactly the bytes in stdout.bin.
    fn stdout_bin(dir: &Path, stdout: &[u8], exit_code: i32) -> String {
        fs::write(dir.join("stdout.bin"), stdout).unwrap();
        fake_bin(
            dir,
            &format!(
                "#!/bin/sh\ncat \"{}\"\nexit {exit_code}\n",
                dir.join("stdout.bin").display()
            ),
        )
    }

    fn fake_engine(job_id: &str, status: &str) -> EngineSession {
        EngineSession {
            pid: 999,
            job_id: job_id.to_string(),
            session_id: "sid-1".to_string(),
            socket_path: "/tmp/sock".to_string(),
            cwd: "/repo".to_string(),
            status: status.to_string(),
            waiting_for: String::new(),
            status_updated_at: 0.0,
            name: String::new(),
        }
    }

    /// Attach *pipe*, feed the screen from *screens*, transcript from a file.
    ///
    /// *baseline* is what the screen shows before anything is typed — the
    /// pipe reads it first and only counts an echo that was not already
    /// there. *draft* is what the dim-aware composer parser reports before
    /// the C-u.
    fn wire(
        hook: &mut Hook,
        pipe: &FakePipe,
        screens: &[&str],
        transcript: Option<PathBuf>,
        engine: Option<EngineSession>,
        baseline: &str,
        draft: bool,
    ) {
        hook.attach_pipe = Some(pipe.clone());
        hook.client_ready = Some(true);
        hook.wait_engine_behind = Some(Some(
            engine.unwrap_or_else(|| fake_engine("cafe1234", "idle")),
        ));
        hook.transcript_cursor = Some((transcript, 0));
        hook.composer_draft = Some(draft);
        hook.no_sleep = true;
        let mut st = pipe.state.lock().unwrap();
        st.stream = baseline.to_string();
        st.pending = screens.iter().map(|s| s.to_string()).collect();
    }

    fn transcript(dir: &Path, records: &[Value]) -> PathBuf {
        let path = dir.join("session.jsonl");
        let mut text = String::new();
        for record in records {
            text.push_str(&record.to_string());
            text.push('\n');
        }
        fs::write(&path, text).unwrap();
        path
    }

    fn user(text: &str) -> Value {
        json!({"type": "user", "message": {"role": "user", "content": text}})
    }

    fn writes(pipe: &FakePipe) -> Vec<String> {
        pipe.state.lock().unwrap().writes.clone()
    }

    // --- pane <-> job records ----------------------------------------------

    #[test]
    fn test_pane_job_record_roundtrip_and_reverse_lookup() {
        let _home = claude_home();

        write_pane_job("%19", "cafe1234", "sess-19", "/w/a").unwrap();
        write_pane_job("%7", "beef5678", "sess-7", "/w/b").unwrap();

        assert_eq!(
            read_pane_job("%19"),
            Some(("cafe1234".into(), "sess-19".into(), "/w/a".into()))
        );
        assert_eq!(job_id_for_pane("%7").as_deref(), Some("beef5678"));
        let mut panes = list_recorded_panes();
        panes.sort();
        assert_eq!(panes, vec!["%19", "%7"]);
        assert_eq!(pane_for_job("cafe1234").as_deref(), Some("%19"));
        assert_eq!(pane_for_job("missing"), None);
        assert_eq!(pane_for_job(""), None);

        clear_pane_job("%19");
        assert_eq!(read_pane_job("%19"), None);
        assert_eq!(pane_for_job("cafe1234"), None);
    }

    #[test]
    fn test_read_pane_job_rejects_garbage() {
        let _home = claude_home();
        let path = pane_job_path("%3");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not json").unwrap();
        assert_eq!(read_pane_job("%3"), None);
        fs::write(&path, json!({"cwd": "/w"}).to_string()).unwrap(); // no jobId
        assert_eq!(read_pane_job("%3"), None);
    }

    #[test]
    fn test_looks_like_job_id() {
        assert!(looks_like_job_id("7fcc705f"));
        assert!(!looks_like_job_id("74e0fe8d-3278-436a-98f1-7dd32c817571"));
        assert!(!looks_like_job_id("worker"));
        assert!(!looks_like_job_id(""));
    }

    // --- engine registry entries -------------------------------------------

    #[test]
    fn test_engine_session_for_job_finds_live_bg_entry() {
        let home = claude_home();
        let sock = home.dir.path().join("engine.sock");
        fs::write(&sock, "").unwrap();
        let pid = me();
        write_registry_entry(
            &home,
            pid,
            &bg_entry(pid, "cafe1234", sock.to_str().unwrap(), "busy"),
        );
        // an interactive entry never answers a job lookup
        write_registry_entry(
            &home,
            424242,
            &json!({
                "pid": pid,
                "kind": "interactive",
                "name": "x",
                "messagingSocketPath": sock.to_str().unwrap(),
            }),
        );

        let engine = engine_session_for_job("cafe1234").unwrap();
        assert_eq!(engine.pid as i64, pid);
        assert_eq!(engine.status, "busy");
        assert_eq!(engine.socket_path, sock.to_str().unwrap());
        assert!(engine.session_id.starts_with("cafe1234"));
        assert!(engine_session_for_job("other000").is_none());
    }

    #[test]
    fn test_engine_entry_requires_live_pid_and_socket() {
        let home = claude_home();
        let sock = home.dir.path().join("engine.sock");
        fs::write(&sock, "").unwrap();
        let dead: i64 = 4_000_000;
        write_registry_entry(
            &home,
            dead,
            &bg_entry(dead, "dead0001", sock.to_str().unwrap(), "idle"),
        );
        let pid = me();
        let gone = home.dir.path().join("gone.sock");
        write_registry_entry(
            &home,
            pid,
            &bg_entry(pid, "nosock01", gone.to_str().unwrap(), "idle"),
        );

        assert!(engine_session_for_job("dead0001").is_none());
        assert!(engine_session_for_job("nosock01").is_none());
        assert!(!pane_engine_alive("%1"));
    }

    #[test]
    fn test_session_id_for_pane_prefers_live_engine_over_record() {
        let home = claude_home();
        let sock = home.dir.path().join("engine.sock");
        fs::write(&sock, "").unwrap();
        let pid = me();
        write_pane_job("%5", "cafe1234", "sess-old", "/w").unwrap();
        write_registry_entry(
            &home,
            pid,
            &bg_entry(pid, "cafe1234", sock.to_str().unwrap(), "idle"),
        );

        // live engine's sessionId (follows /clear) wins over the record snapshot
        assert!(session_id_for_pane("%5").unwrap().starts_with("cafe1234"));

        fs::remove_file(home.config.join("sessions").join(format!("{pid}.json"))).unwrap();
        // parked engine: fall back to the record's spawn-time snapshot
        assert_eq!(session_id_for_pane("%5").as_deref(), Some("sess-old"));
    }

    // --- ledger / lifecycle -------------------------------------------------

    #[test]
    fn test_job_row_separates_asleep_from_gone() {
        let rows: Vec<Map<String, Value>> = vec![
            json!({"id": "cafe1234", "kind": "background", "state": "stopped", "sessionId": "s-1"})
                .as_object()
                .cloned()
                .unwrap(),
            json!({"pid": 1, "kind": "interactive", "name": "x"})
                .as_object()
                .cloned()
                .unwrap(),
        ];
        let mut hook = Hook::default();
        hook.list_jobs_rows = Some(Some(rows));
        let _g = testhook::install(hook);

        assert_eq!(
            job_row("cafe1234", "claude").unwrap().get("state"),
            Some(&Value::String("stopped".into())) // asleep, not dead
        );
        assert!(job_row("gone0001", "claude").is_none());
        assert!(job_exists("cafe1234", "claude"));

        testhook::with(|h| h.list_jobs_rows = Some(None)); // CLI failure
        assert!(job_row("cafe1234", "claude").is_none());
    }

    #[test]
    fn test_spawn_job_parses_the_backgrounded_announcement() {
        let home = claude_home();
        let _child = VarGuard::set("CLAUDE_CODE_CHILD_SESSION", "1");
        let _model = VarGuard::set("ANTHROPIC_MODEL", "x");
        let out = home.dir.path();
        fs::write(
            out.join("stdout.bin"),
            "backgrounded · 7fcc705f · probe-mouse\n  claude agents  list sessions\n",
        )
        .unwrap();
        let bin = fake_bin(
            out,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{out}/argv\"\npwd > \"{out}/cwd\"\nenv > \"{out}/env\"\ncat \"{out}/stdout.bin\"\nprintf 'Starting background service…\\n' >&2\n",
                out = out.display()
            ),
        );
        let workdir = out.join("w");
        fs::create_dir(&workdir).unwrap();
        let mut extra = HashMap::new();
        extra.insert("K".to_string(), "V".to_string());

        let job_id = spawn_job(
            workdir.to_str().unwrap(),
            "t.w1",
            "/hive",
            &["--model".to_string(), "opus".to_string()],
            Some(&extra),
            &bin,
        );

        assert_eq!(job_id.as_deref(), Some("7fcc705f"));
        let argv = fs::read_to_string(out.join("argv")).unwrap();
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            vec!["--bg", "--name", "t.w1", "--model", "opus", "/hive"]
        );
        let cwd = fs::read_to_string(out.join("cwd")).unwrap();
        assert_eq!(
            fs::canonicalize(cwd.trim()).unwrap(),
            fs::canonicalize(&workdir).unwrap()
        );
        // env washed: an inherited child-session marker would make the engine
        // skip registration entirely; the config-tree override survives
        let envdump = fs::read_to_string(out.join("env")).unwrap();
        assert!(!envdump
            .lines()
            .any(|l| l.starts_with("CLAUDE_CODE_CHILD_SESSION=")));
        assert!(!envdump.lines().any(|l| l.starts_with("ANTHROPIC_MODEL=")));
        assert!(envdump
            .lines()
            .any(|l| l == format!("CLAUDE_CONFIG_DIR={}", home.config.display())));
        assert!(envdump.lines().any(|l| l == "K=V"));
    }

    #[test]
    fn test_spawn_job_returns_none_on_failure() {
        let home = claude_home();
        let bin = stdout_bin(home.dir.path(), b"", 1);
        assert_eq!(
            spawn_job(
                home.dir.path().to_str().unwrap(),
                "t.w1",
                "",
                &[],
                None,
                &bin
            ),
            None
        );
    }

    #[test]
    fn test_spawn_job_refuses_an_announcement_that_is_not_a_job_id() {
        // a token no registry row can carry as its `jobId` is not an address:
        // the caller would poll for it until the whole startup budget burned
        let cases: [&[u8]; 3] = [
            b"backgrounded \xc2\xb7 \x1b]8;;x\x07 \xc2\xb7 probe\n", // an escape the strip missed
            b"backgrounded \xc2\xb7 not-a-job-id \xc2\xb7 probe\n", // reworded / renamed announcement
            b"started probe in the background\n",                   // no announcement at all
        ];
        for stdout in cases {
            let home = claude_home();
            let bin = stdout_bin(home.dir.path(), stdout, 0);
            assert_eq!(
                spawn_job(
                    home.dir.path().to_str().unwrap(),
                    "t.w1",
                    "",
                    &[],
                    None,
                    &bin
                ),
                None
            );
        }
    }

    #[test]
    fn test_ensure_engine_wakes_a_parked_job_once() {
        let mut hook = Hook::default();
        hook.engine_for_job = Some(VecDeque::from(vec![
            None,
            Some(fake_engine("cafe1234", "idle")),
        ]));
        hook.wake_result = Some(true);
        hook.no_sleep = true;
        let _g = testhook::install(hook);

        let engine = ensure_engine("cafe1234", Some(0.0), "claude").unwrap();
        assert_eq!(engine.job_id, "cafe1234");
        assert_eq!(
            testhook::with(|h| h.wakes.clone()).unwrap(),
            vec!["cafe1234"]
        );
    }

    #[test]
    fn test_ensure_engine_gives_up_when_wake_fails() {
        let mut hook = Hook::default();
        hook.engine_for_job = Some(VecDeque::from(vec![None]));
        hook.wake_result = Some(false);
        hook.no_sleep = true;
        let _g = testhook::install(hook);

        assert!(ensure_engine("cafe1234", Some(0.0), "claude").is_none());
    }

    // --- runtime mapping ----------------------------------------------------

    fn runtime_engine(status: &str, waiting_for: &str, updated_at: Option<f64>) -> EngineSession {
        EngineSession {
            pid: 1,
            job_id: "cafe1234".to_string(),
            session_id: "s".to_string(),
            socket_path: "/s".to_string(),
            cwd: String::new(),
            status: status.to_string(),
            waiting_for: waiting_for.to_string(),
            status_updated_at: updated_at.unwrap_or_else(now_epoch),
            name: String::new(),
        }
    }

    #[test]
    fn test_runtime_from_engine_maps_status_vocabulary() {
        let busy = runtime_from_engine(&runtime_engine("busy", "", None), None);
        assert_eq!(busy.get("busy"), Some(&Value::Bool(true)));
        assert_eq!(busy.get("inputState"), Some(&Value::String("ready".into())));

        let idle = runtime_from_engine(&runtime_engine("idle", "", None), None);
        assert_eq!(idle.get("busy"), Some(&Value::Bool(false)));
        assert_eq!(idle.get("inputState"), Some(&Value::String("ready".into())));

        let waiting = runtime_from_engine(&runtime_engine("waiting", "input needed", None), None);
        assert_eq!(waiting.get("busy"), Some(&Value::Bool(false)));
        assert_eq!(
            waiting.get("inputState"),
            Some(&Value::String("waiting_user".into()))
        );
        assert_eq!(
            waiting.get("inputReason"),
            Some(&Value::String("registry:input needed".into()))
        );

        let unknown = runtime_from_engine(&runtime_engine("", "", None), None);
        assert_eq!(
            unknown.get("inputState"),
            Some(&Value::String("unknown".into()))
        );
        assert_eq!(
            unknown.get("inputReason"),
            Some(&Value::String("no_registry_status".into()))
        );
    }

    #[test]
    fn test_runtime_from_engine_demotes_stale_status() {
        let stale = runtime_from_engine(
            &runtime_engine("busy", "", Some(1.0)),
            Some(STATUS_STALE_AFTER_SECONDS + 100.0),
        );
        assert_eq!(stale.get("busy"), Some(&Value::Bool(false)));
        assert_eq!(
            stale.get("inputState"),
            Some(&Value::String("unknown".into()))
        );
        assert_eq!(
            stale.get("inputReason"),
            Some(&Value::String("stale_status".into()))
        );
    }

    // --- argv shape ---------------------------------------------------------

    #[test]
    fn test_attach_puts_the_subcommand_first() {
        // `claude attach <job>` — subcommand before the job id, always.
        let home = claude_home();
        let out = home.dir.path();
        let bin = fake_bin(
            out,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{out}/argv.tmp\"\nmv \"{out}/argv.tmp\" \"{out}/argv\"\n",
                out = out.display()
            ),
        );

        let client = _attach_pipe("cafe1234", &bin).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while !out.join("argv").exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let argv = fs::read_to_string(out.join("argv")).unwrap();
        assert_eq!(argv.lines().collect::<Vec<_>>(), vec!["attach", "cafe1234"]);
        _close_pipe(client);
    }

    #[test]
    fn test_pipe_env_is_washed_of_claude_vars() {
        let _home = claude_home();
        let _child = VarGuard::set("CLAUDE_CODE_CHILD_SESSION", "1");
        let _key = VarGuard::set("ANTHROPIC_API_KEY", "secret");
        let env = bg_env(None);
        assert!(!env.contains_key("CLAUDE_CODE_CHILD_SESSION"));
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
    }

    // --- typing -------------------------------------------------------------

    #[test]
    fn test_typing_clears_the_composer_in_its_own_chunk_then_submits() {
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), &[user("hello there")]);
        let mut hook = Hook::default();
        wire(
            &mut hook,
            &pipe,
            &["> hello there"],
            Some(path),
            None,
            "> ",
            false,
        );
        let _g = testhook::install(hook);

        let result = type_into_job("cafe1234", "hello there", "claude");

        assert!(result.ok);
        assert_eq!(result.confirmed, "transcript");
        // C-u alone, then the text, then Enter — a control byte must never
        // ride in the text's chunk (it gets inserted literally when it does).
        assert_eq!(writes(&pipe), vec!["\u{15}", "hello there", "\r"]);
        assert!(pipe.state.lock().unwrap().closed);
    }

    #[test]
    fn test_a_lost_keystroke_is_retyped_and_the_retype_cannot_double() {
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), &[user("ping")]);
        // First screens have no echo: the client was not forwarding yet.
        let mut hook = Hook::default();
        wire(
            &mut hook,
            &pipe,
            &["> ", "> ", "> ping"],
            Some(path),
            None,
            "> ",
            false,
        );
        hook.type_retry_after = Some(0.0);
        let _g = testhook::install(hook);

        assert!(type_into_job("cafe1234", "ping", "claude").ok);
        // Every retype re-clears first, so the composer holds one copy, not two.
        let written = writes(&pipe);
        assert_eq!(
            written.iter().filter(|w| *w == "ping").count(),
            written.iter().filter(|w| *w == "\u{15}").count()
        );
        assert_eq!(written.last().map(String::as_str), Some("\r"));
    }

    #[test]
    fn test_no_echo_within_the_budget_refuses_instead_of_submitting() {
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let mut hook = Hook::default();
        wire(
            &mut hook,
            &pipe,
            &["> something else"],
            Some(dir.path().join("none.jsonl")),
            None,
            "> ",
            false,
        );
        hook.type_ready_timeout = Some(0.0);
        let _g = testhook::install(hook);

        let result = type_into_job("cafe1234", "ping", "claude");

        assert!(!result.ok);
        assert!(!writes(&pipe).iter().any(|w| w == "\r"));
    }

    #[test]
    fn test_the_echo_survives_the_composer_wrapping_the_text() {
        // The attach stream is a raw pty replay: the layout is cursor moves
        // and box drawing, so the echo is matched with both squashed out.
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let text = "a long sendback that the composer wraps over two lines";
        let path = transcript(dir.path(), &[user(text)]);
        let wrapped =
            "╭─────────╮\n│ a long sendback that the │\n│ composer wraps over two lines │\n╰──╯";
        let mut hook = Hook::default();
        wire(&mut hook, &pipe, &[wrapped], Some(path), None, "> ", false);
        let _g = testhook::install(hook);

        assert!(type_into_job("cafe1234", text, "claude").ok);
    }

    #[test]
    fn test_text_already_on_the_screen_is_not_taken_for_the_echo() {
        // The attach stream starts at attach time (no history replay), and
        // the mark is taken at type time: a stale identical copy that was
        // already on screen before the type proves nothing — with no new
        // echo, no submit.
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let stale = "> ping\n(the previous delivery, still in the scrollback)";
        let mut hook = Hook::default();
        wire(
            &mut hook,
            &pipe,
            &[],
            Some(dir.path().join("none.jsonl")),
            None,
            stale,
            false,
        );
        hook.type_ready_timeout = Some(0.01);
        let _g = testhook::install(hook);

        let result = type_into_job("cafe1234", "ping", "claude");

        assert!(!result.ok);
        let written = writes(&pipe);
        assert!(written.iter().any(|w| w == "ping"));
        assert!(!written.iter().any(|w| w == "\r"));
    }

    #[test]
    fn test_a_second_copy_of_the_same_text_is_the_echo() {
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let stale = "> ping\n(the previous delivery, still in the scrollback)";
        let path = transcript(dir.path(), &[user("ping")]);
        let frame = format!("{stale}\n> ping");
        let mut hook = Hook::default();
        wire(&mut hook, &pipe, &[&frame], Some(path), None, stale, false);
        let _g = testhook::install(hook);

        assert!(type_into_job("cafe1234", "ping", "claude").ok);
    }

    #[test]
    fn test_a_long_sendback_echoes_by_its_tail() {
        // The composer scrolls to the cursor, so a long paste shows its end
        // and the head never reaches the screen.
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let text = format!(
            "head of the sendback\n{}the very last line of it",
            "filler line\n".repeat(40)
        );
        let path = transcript(dir.path(), &[user(&text)]);
        let viewport = format!(
            "{}│ the very last line of it │",
            "│ filler line │\n".repeat(5)
        );
        let mut hook = Hook::default();
        wire(
            &mut hook,
            &pipe,
            &[&viewport],
            Some(path),
            None,
            "> ",
            false,
        );
        let _g = testhook::install(hook);

        assert!(type_into_job("cafe1234", &text, "claude").ok);
    }

    #[test]
    fn test_a_pasted_text_placeholder_counts_as_the_echo() {
        // A long paste is folded into `[Pasted text #N]`: none of the text is
        // on screen, and the placeholder is the only thing the client can echo.
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let text = "a sendback long enough for the TUI to fold it away\n".repeat(20);
        let path = transcript(dir.path(), &[user(&text)]);
        let earlier = "> [Pasted text #1 +3 lines]"; // an older paste, still in the replay
        let frame = format!("{earlier}\n> [Pasted text #2 +20 lines]");
        let mut hook = Hook::default();
        wire(
            &mut hook,
            &pipe,
            &[&frame],
            Some(path),
            None,
            earlier,
            false,
        );
        let _g = testhook::install(hook);

        assert!(type_into_job("cafe1234", &text, "claude").ok);
    }

    #[test]
    fn test_a_removed_job_fails_as_soon_as_the_client_gives_up() {
        // `attach <gone>` exits at once; waiting out the wake budget for an
        // engine that will never register just delays the error.
        let pipe = FakePipe::default();
        pipe.state.lock().unwrap().poll = Some(1);
        let mut hook = Hook::default();
        hook.attach_pipe = Some(pipe.clone());
        hook.engine_for_job = Some(VecDeque::from(vec![None]));
        hook.no_sleep = true;
        let _g = testhook::install(hook);

        let result = type_into_job("deadbeef", "ping", "claude");

        assert!(!result.ok);
        assert!(result.why.contains("no engine"));
    }

    #[test]
    fn test_a_broken_pipe_is_a_failure_not_a_crash() {
        let pipe = FakePipe::default();
        pipe.state.lock().unwrap().broken_after = Some(0);
        let dir = tempfile::tempdir().unwrap();
        let mut hook = Hook::default();
        wire(
            &mut hook,
            &pipe,
            &["> "],
            Some(dir.path().join("none.jsonl")),
            None,
            "> ",
            false,
        );
        let _g = testhook::install(hook);

        let result = type_into_job("cafe1234", "ping", "claude");

        assert!(!result.ok);
        assert!(result.why.contains("stdin"));
    }

    // --- submit confirmation ------------------------------------------------

    #[test]
    fn test_a_turn_that_swallowed_a_leftover_draft_is_not_confirmed() {
        // The transcript turn must equal what was typed. A composer that
        // still held a draft produces a longer turn — the one thing a
        // substring match would wave through.
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), &[user("DRAFTJUNK/compact")]);
        let mut hook = Hook::default();
        wire(
            &mut hook,
            &pipe,
            &["> DRAFTJUNK/compact"],
            Some(path),
            None,
            "> ",
            false,
        );
        let _g = testhook::install(hook);

        let result = type_into_job("cafe1234", "/compact", "claude");

        assert!(!result.ok);
        assert!(result.why.contains("leftover draft"));
    }

    #[test]
    fn test_a_slash_command_is_confirmed_by_its_command_record() {
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), &[user("<command-name>/compact</command-name>")]);
        let mut hook = Hook::default();
        wire(
            &mut hook,
            &pipe,
            &["> /compact"],
            Some(path),
            None,
            "> ",
            false,
        );
        let _g = testhook::install(hook);

        let result = type_into_job("cafe1234", "/compact", "claude");

        assert!(result.ok);
        assert_eq!(result.confirmed, "transcript");
    }

    #[test]
    fn test_a_ui_only_slash_command_degrades_to_written() {
        // `/cost` and friends draw a panel and write nothing — silence there
        // is not evidence the keystrokes were lost.
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), &[]);
        let mut hook = Hook::default();
        wire(
            &mut hook,
            &pipe,
            &["> /cost"],
            Some(path),
            None,
            "> ",
            false,
        );
        hook.slash_confirm_timeout = Some(0.0);
        let _g = testhook::install(hook);

        let result = type_into_job("cafe1234", "/cost", "claude");

        assert!(result.ok);
        assert_eq!(result.confirmed, "written");
    }

    #[test]
    fn test_plain_text_without_a_turn_is_a_failure() {
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), &[]);
        let mut hook = Hook::default();
        wire(&mut hook, &pipe, &["> ping"], Some(path), None, "> ", false);
        hook.submit_confirm_timeout = Some(0.0);
        let _g = testhook::install(hook);

        assert!(!type_into_job("cafe1234", "ping", "claude").ok);
    }

    // --- interrupt ----------------------------------------------------------

    #[test]
    fn test_interrupt_writes_one_escape_and_confirms_on_the_marker() {
        // Escape is never repeated: a second one lands on claude's own
        // 'edit previous message' chord.
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(
            dir.path(),
            &[json!({"type": "system", "content": "[Request interrupted by user]"})],
        );
        let mut hook = Hook::default();
        wire(
            &mut hook,
            &pipe,
            &[""],
            Some(path),
            Some(fake_engine("cafe1234", "busy")),
            "> ",
            false,
        );
        let _g = testhook::install(hook);

        let result = interrupt_job("cafe1234", "claude");

        assert!(result.ok);
        assert_eq!(result.confirmed, "transcript");
        assert_eq!(writes(&pipe), vec!["\u{1b}"]);
    }

    #[test]
    fn test_interrupt_of_an_idle_engine_returns_at_once() {
        // Nothing is running, so nothing can confirm: sitting out the window
        // could only relabel a success — and cvim sends this before every
        // sendback.
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), &[]);
        let mut hook = Hook::default();
        wire(&mut hook, &pipe, &[""], Some(path), None, "> ", false);
        hook.forbid_engine_lookup = true; // an idle engine must not be polled
        let _g = testhook::install(hook);

        let result = interrupt_job("cafe1234", "claude");

        assert!(result.ok);
        assert_eq!(result.confirmed, "written");
    }

    #[test]
    fn test_interrupt_of_a_busy_engine_that_stays_busy_fails() {
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), &[]);
        let mut hook = Hook::default();
        wire(
            &mut hook,
            &pipe,
            &[""],
            Some(path),
            Some(fake_engine("cafe1234", "busy")),
            "> ",
            false,
        );
        hook.engine_for_job = Some(VecDeque::from(vec![Some(fake_engine("cafe1234", "busy"))]));
        hook.interrupt_confirm_timeout = Some(0.0);
        let _g = testhook::install(hook);

        assert!(!interrupt_job("cafe1234", "claude").ok);
    }

    #[test]
    fn test_interrupt_confirms_when_the_engine_leaves_busy() {
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), &[]);
        let mut hook = Hook::default();
        wire(
            &mut hook,
            &pipe,
            &[""],
            Some(path),
            Some(fake_engine("cafe1234", "busy")),
            "> ",
            false,
        );
        hook.engine_for_job = Some(VecDeque::from(vec![Some(fake_engine("cafe1234", "idle"))]));
        let _g = testhook::install(hook);

        let result = interrupt_job("cafe1234", "claude");

        assert!(result.ok);
        assert_eq!(result.confirmed, "status");
    }

    // --- a wedged client may not outlive the call ---------------------------

    #[test]
    fn test_a_client_that_will_not_exit_is_killed() {
        let pipe = FakePipe::default();
        pipe.state.lock().unwrap().hang_wait = true;
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), &[user("ping")]);
        let mut hook = Hook::default();
        wire(&mut hook, &pipe, &["> ping"], Some(path), None, "> ", false);
        let _g = testhook::install(hook);

        assert!(type_into_job("cafe1234", "ping", "claude").ok);
        // the reap runs off-thread; give it a moment
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pipe.state.lock().unwrap().killed && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(pipe.state.lock().unwrap().killed);
    }

    // --- draft save/restore -------------------------------------------------

    #[test]
    fn test_a_killed_draft_is_pasted_back_after_the_submit() {
        // C-u parks the draft on claude's kill ring; a confirmed submit
        // pastes it back (C-y) so the human's half-typed thought survives
        // the command.
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), &[user("hello there")]);
        let mut hook = Hook::default();
        wire(
            &mut hook,
            &pipe,
            &["> hello there"],
            Some(path),
            None,
            "> ",
            true,
        );
        let _g = testhook::install(hook);

        let result = type_into_job("cafe1234", "hello there", "claude");

        assert!(result.ok);
        assert_eq!(writes(&pipe), vec!["\u{15}", "hello there", "\r", "\u{19}"]);
    }

    #[test]
    fn test_an_empty_composer_never_gets_a_stale_ring_pasted() {
        // The kill ring survives a C-u that killed nothing; pasting it back
        // would resurrect unrelated content (real-machine verified).
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), &[user("hello there")]);
        let mut hook = Hook::default();
        wire(
            &mut hook,
            &pipe,
            &["> hello there"],
            Some(path),
            None,
            "> ",
            false,
        );
        let _g = testhook::install(hook);

        assert!(type_into_job("cafe1234", "hello there", "claude").ok);
        assert!(!writes(&pipe).iter().any(|w| w == "\u{19}"));
    }

    #[test]
    fn test_a_retype_forfeits_the_restore() {
        // The second C-u overwrites the single-slot ring with our own failed
        // text — pasting that back would fabricate a draft the human never
        // typed.
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), &[user("ping")]);
        let mut hook = Hook::default();
        wire(
            &mut hook,
            &pipe,
            &["> ", "> ", "> ping"],
            Some(path),
            None,
            "> ",
            true,
        );
        hook.type_retry_after = Some(0.0);
        let _g = testhook::install(hook);

        assert!(type_into_job("cafe1234", "ping", "claude").ok);
        assert!(!writes(&pipe).iter().any(|w| w == "\u{19}"));
    }

    #[test]
    fn test_a_slash_command_restores_the_draft_too() {
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), &[]);
        let mut hook = Hook::default();
        wire(&mut hook, &pipe, &["> /cost"], Some(path), None, "> ", true);
        hook.slash_confirm_timeout = Some(0.0);
        let _g = testhook::install(hook);

        let result = type_into_job("cafe1234", "/cost", "claude");

        assert!(result.ok);
        assert_eq!(result.confirmed, "written");
        assert_eq!(writes(&pipe).last().map(String::as_str), Some("\u{19}"));
    }

    #[test]
    fn test_a_failed_submit_does_not_touch_the_ring() {
        // On corruption the composer state is unknown — pasting on top of it
        // could double the mess; the loud failure is the whole point.
        let pipe = FakePipe::default();
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), &[user("DRAFT-hello there")]);
        let mut hook = Hook::default();
        wire(
            &mut hook,
            &pipe,
            &["> hello there"],
            Some(path),
            None,
            "> ",
            true,
        );
        let _g = testhook::install(hook);

        let result = type_into_job("cafe1234", "hello there", "claude");

        assert!(!result.ok);
        assert!(!writes(&pipe).iter().any(|w| w == "\u{19}"));
    }

    #[test]
    fn test_the_draft_gate_reads_the_pane_only_when_it_shows_this_job() {
        // The logs replay is an incremental paint stream and cannot answer
        // "what is in the composer"; the member's own pane render can — but
        // only while it is actually showing this member.
        let mut hook = Hook::default();
        hook.pane_for_job = Some(Some("%7".to_string()));
        hook.view_probe = Some(Ok(("cafe1234".to_string(), "certain".to_string())));
        hook.suspected_draft = Some(true);
        let _g = testhook::install(hook);

        assert!(_composer_has_draft("cafe1234"));
        assert_eq!(
            testhook::with(|h| h.suspected_calls.clone()).unwrap(),
            vec![("%7".to_string(), "claude".to_string())]
        );
    }

    #[test]
    fn test_the_draft_gate_is_closed_when_the_viewer_shows_someone_else() {
        let mut hook = Hook::default();
        hook.pane_for_job = Some(Some("%7".to_string()));
        hook.view_probe = Some(Ok(("other999".to_string(), "certain".to_string())));
        hook.suspected_draft = Some(true); // must not capture
        let _g = testhook::install(hook);

        assert!(!_composer_has_draft("cafe1234"));
        assert!(testhook::with(|h| h.suspected_calls.clone())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_the_draft_gate_is_closed_without_a_pane() {
        let mut hook = Hook::default();
        hook.pane_for_job = Some(None);
        let _g = testhook::install(hook);
        assert!(!_composer_has_draft("cafe1234"));
    }

    #[test]
    fn test_a_probe_failure_closes_the_draft_gate() {
        let mut hook = Hook::default();
        hook.pane_for_job = Some(Some("%7".to_string()));
        hook.view_probe = Some(Err(())); // tmux gone
        let _g = testhook::install(hook);
        assert!(!_composer_has_draft("cafe1234"));
    }

    // --- job naming ---------------------------------------------------------

    fn named_engine(name: &str) -> EngineSession {
        EngineSession {
            pid: 1,
            job_id: "cafe1234".to_string(),
            session_id: "s".to_string(),
            socket_path: "/tmp/s".to_string(),
            cwd: "/repo".to_string(),
            status: "idle".to_string(),
            waiting_for: String::new(),
            status_updated_at: 0.0,
            name: name.to_string(),
        }
    }

    #[test]
    fn test_a_wrongly_named_job_is_renamed_with_a_control_frame() {
        let mut hook = Hook::default();
        // pre-check, then confirm poll
        hook.engine_for_job = Some(VecDeque::from(vec![
            Some(named_engine("hive-183")),
            Some(named_engine("honey.worker")),
        ]));
        hook.rename_result = Some(true);
        hook.no_sleep = true;
        let _g = testhook::install(hook);

        assert!(ensure_job_named("cafe1234", "honey.worker"));
        assert_eq!(
            testhook::with(|h| h.renames.clone()).unwrap(),
            vec![(
                "/tmp/s".to_string(),
                "honey.worker".to_string(),
                "s".to_string()
            )]
        );
    }

    #[test]
    fn test_a_correctly_named_job_sends_no_frame() {
        let mut hook = Hook::default();
        hook.engine_for_job = Some(VecDeque::from(vec![Some(named_engine("honey.worker"))]));
        hook.rename_result = Some(true); // any frame would be recorded
        let _g = testhook::install(hook);

        assert!(ensure_job_named("cafe1234", "honey.worker"));
        assert!(testhook::with(|h| h.renames.clone()).unwrap().is_empty());
    }

    #[test]
    fn test_a_refused_rename_frame_reports_failure() {
        let mut hook = Hook::default();
        hook.engine_for_job = Some(VecDeque::from(vec![Some(named_engine("hive-183"))]));
        hook.rename_result = Some(false);
        let _g = testhook::install(hook);

        assert!(!ensure_job_named("cafe1234", "honey.worker"));
    }

    #[test]
    fn test_a_rename_the_registry_never_confirms_reports_failure() {
        let mut hook = Hook::default();
        hook.engine_for_job = Some(VecDeque::from(vec![Some(named_engine("hive-183"))]));
        hook.rename_result = Some(true);
        hook.rename_confirm_timeout = Some(0.2);
        hook.rename_poll_interval = Some(0.05);
        let _g = testhook::install(hook);

        assert!(!ensure_job_named("cafe1234", "honey.worker"));
    }

    #[test]
    fn test_naming_an_engineless_job_reports_failure() {
        let mut hook = Hook::default();
        hook.engine_for_job = Some(VecDeque::from(vec![None]));
        let _g = testhook::install(hook);
        assert!(!ensure_job_named("cafe1234", "honey.worker"));
    }

    #[test]
    fn test_the_registry_name_is_read_into_the_engine_session() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("s.sock");
        fs::write(&sock, "").unwrap();
        let engine = _entry_to_engine(&json!({
            "kind": "bg",
            "pid": 1,
            "jobId": "cafe1234",
            "messagingSocketPath": sock.to_str().unwrap(),
            "name": "honey.worker",
        }))
        .unwrap();
        assert_eq!(engine.name, "honey.worker");
    }

    #[test]
    fn test_bg_env_keeps_color_forcing_for_the_renderer() {
        // Color is the engine's to keep — a cold-spawned engine renders its
        // TUI with this env for its whole life. Safety against colored output
        // lives at the parse sites (ANSI strip), never in the env.
        let _home = claude_home();
        let _force = VarGuard::set("FORCE_COLOR", "3");
        let env = bg_env(None);
        assert_eq!(env.get("FORCE_COLOR").map(String::as_str), Some("3"));
        assert!(!env.contains_key("NO_COLOR"));
    }

    #[test]
    fn test_list_jobs_parses_colored_json() {
        let home = claude_home();
        let bin = stdout_bin(
            home.dir.path(),
            b"\x1b[32m[{\"jobId\": \"abcd1234\"}]\x1b[39m",
            0,
        );
        let rows = list_jobs(&bin).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get("jobId"),
            Some(&Value::String("abcd1234".into()))
        );
    }

    #[test]
    fn test_spawn_job_parses_colored_output() {
        // Regression: an ANSI-wrapped jobId polled a job that does not exist,
        // so every engine-parented spawn timed out as 'never registered'.
        let home = claude_home();
        let bin = stdout_bin(
            home.dir.path(),
            b"opus backgrounded \xc2\xb7 \x1b[36mce5de22a\x1b[39m\n",
            0,
        );
        assert_eq!(
            spawn_job(
                home.dir.path().to_str().unwrap(),
                "x",
                "hi",
                &[],
                None,
                &bin
            )
            .as_deref(),
            Some("ce5de22a")
        );
    }
}
