//! Team-scoped hived: message transport, runtime signals, notify watcher.
//!
//! Delivery has exactly one state: the native transport (claude inbox /
//! codex daemon / grok leader) either accepted the message or refused it.
//! There is no tracked in-between and no confirmation oracle — acceptance
//! means the target's own runtime owns it from there.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::agent::{Agent, DeliveryError};
use crate::runtime_snapshot::{RuntimeSnapshot, RuntimeSnapshotStore};
use crate::runtime_state::{format_hive_envelope, project_thread_event};
use crate::team::Team;
use crate::{bus, devlog};

#[allow(dead_code)]
pub const ACTIVE_SLEEP: f64 = 0.5;
pub const IDLE_NOTIFY_TICK_SECONDS: f64 = 1.0;
pub const IDLE_NOTIFY_THRESHOLD_SECONDS: f64 = 5.0;
pub const IDLE_NOTIFY_MESSAGE: &str = "Window idle 5s+ (all agents stopped). Return to review.";
pub const IDLE_NOTIFY_MISSING_PRUNE_TICKS: i64 = 5;
pub const NOTIFY_DEBUG_HEARTBEAT_SECONDS: f64 = 30.0;
pub const HIVED_CODE_CHECK_SECONDS: f64 = 5.0;
pub const HIVED_OWNER_CHECK_SECONDS: f64 = 5.0;
const _HIVED_REEXEC_LOCK_ENV: &str = "HIVE_HIVED_REEXEC_LOCK_FD";
pub const SOCKET_READY_TIMEOUT: f64 = 2.0;
pub const SOCKET_RETRY_INTERVAL: f64 = 0.1;
// The CLI's socket budget must be strictly longer than the work it asks the
// hived to perform: worst-case native transport submission (claude inbox
// connect+write / codex daemon RPC / grok leader prompt+ack) plus slack for
// scheduling and payload plumbing.
// A send blocks on nothing else — it returns queued the moment the transport
// accepts; confirmation is asynchronous (background tracker / query-time).
pub const REQUEST_SLACK: f64 = 5.0;
pub const HIVED_API_VERSION: i64 = 5;
pub const BUSY_OUTPUT_THRESHOLD_SECONDS: f64 = 3.0;
// A probed session id only speaks for the session it saw: nothing tells the
// hived that the human typed `/new` in an unmanaged pane, so the snapshot
// ages out and the adapter re-probes instead of pinning a dead id forever.
const _SESSION_SNAPSHOT_FRESHNESS_S: f64 = 600.0;
const _TRANSCRIPT_PATH_CACHE_TTL: f64 = 60.0;
const _CLAUDE_JOBS_CACHE_TTL: f64 = 30.0;
const _GROK_REAP_GRACE_SECONDS: f64 = 120.0;
// One send_keys attempt per pane per cooldown window, so a slow-starting
// codex is not typed at twice while the process check cannot see it yet.
const _CODEX_REATTACH_COOLDOWN_SECONDS: f64 = 60.0;
pub const FLOW_MAILBOX_AGENT: &str = "flow.run";

// waitingFor values that do not gate a send: a /status-style dialog open in
// an attached viewer parks the status on "waiting", but the inbox still
// queues normally and the message shows the moment the dialog closes.
const _SEND_GATE_WAIVED_REASONS: [&str; 1] = ["registry:dialog open"];

// Near-zero process clock (runtime_snapshot's timestamps share the shape).
// Python's monotonic is system uptime, so its "last seen at 0.0" defaults
// mean "long ago" — the Rust ports of those stamps seed NEG_INFINITY instead.
fn monotonic() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

fn _native_submit_timeout() -> f64 {
    // claude's worst case is a delivery that has to wake a parked engine
    // first (ledger check + tty-less attach + entry poll) before the inbox
    // write itself.
    let claude = crate::adapters::claude_sessions::SUBMIT_TIMEOUT
        + crate::adapters::claude_bg::WAKE_SUBMIT_BUDGET;
    claude
        .max(crate::adapters::codex_app_server::SUBMIT_TIMEOUT)
        .max(crate::adapters::grok_leader::SUBMIT_TIMEOUT)
}

pub fn _send_request_timeout() -> f64 {
    _native_submit_timeout() + REQUEST_SLACK
}

// --------------------------------------------------------------------------
// module state (Python module globals; nextest gives one process per test)
// --------------------------------------------------------------------------

/// Public `busy` monitor duck type (Python passes the monitor object around).
pub trait OutputMonitor: Send + Sync {
    fn is_busy(&self, pane_id: &str, threshold_seconds: f64) -> bool;
    fn last_output_age(&self, pane_id: &str) -> Option<f64>;
    fn start(&self) {}
    fn stop(&self) {}
}

impl OutputMonitor for crate::tmux::ControlModeOutputMonitor {
    fn is_busy(&self, pane_id: &str, threshold_seconds: f64) -> bool {
        crate::tmux::ControlModeOutputMonitor::is_busy(self, pane_id, threshold_seconds)
    }
    fn last_output_age(&self, pane_id: &str) -> Option<f64> {
        crate::tmux::ControlModeOutputMonitor::last_output_age(self, pane_id)
    }
    fn start(&self) {
        crate::tmux::ControlModeOutputMonitor::start(self)
    }
    fn stop(&self) {
        crate::tmux::ControlModeOutputMonitor::stop(self)
    }
}

#[allow(clippy::type_complexity)]
fn output_busy_monitor() -> &'static Mutex<Option<Arc<dyn OutputMonitor>>> {
    static CELL: OnceLock<Mutex<Option<Arc<dyn OutputMonitor>>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

pub fn _set_output_busy_monitor(monitor: Option<Arc<dyn OutputMonitor>>) {
    *output_busy_monitor()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = monitor;
}

fn _get_output_busy_monitor() -> Option<Arc<dyn OutputMonitor>> {
    output_busy_monitor()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[allow(clippy::type_complexity)]
fn transcript_path_cache() -> &'static Mutex<HashMap<String, (String, f64, String)>> {
    static CELL: OnceLock<Mutex<HashMap<String, (String, f64, String)>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(HashMap::new()))
}

fn runtime_snapshots() -> &'static Mutex<RuntimeSnapshotStore> {
    static CELL: OnceLock<Mutex<RuntimeSnapshotStore>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(RuntimeSnapshotStore::default()))
}

#[allow(clippy::type_complexity)]
fn claude_jobs_cache() -> &'static Mutex<Option<(f64, Option<HashMap<String, Map<String, Value>>>)>>
{
    static CELL: OnceLock<Mutex<Option<(f64, Option<HashMap<String, Map<String, Value>>>)>>> =
        OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

fn codex_reattach_at() -> &'static Mutex<HashMap<String, f64>> {
    static CELL: OnceLock<Mutex<HashMap<String, f64>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(HashMap::new()))
}

static _SHUTDOWN: AtomicBool = AtomicBool::new(false);
static _INFLIGHT_REQUESTS: AtomicI64 = AtomicI64::new(0);

pub fn _requests_in_flight() -> bool {
    _INFLIGHT_REQUESTS.load(Ordering::SeqCst) > 0
}

// --------------------------------------------------------------------------
// build identity
// --------------------------------------------------------------------------

/// The Rust build's code identity is the binary on disk (the Python port
/// hashed every .py under src/hive; the compiled binary is the same truth).
pub fn _compute_build_hash() -> String {
    let inner = || -> std::io::Result<String> {
        let exe = std::env::current_exe()?;
        let bytes = fs::read(&exe)?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        Ok(hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    };
    inner().unwrap_or_else(|_| "unknown".to_string())
}

pub fn hived_build_hash() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(_compute_build_hash)
}

pub fn _hived_reexec_argv(
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
) -> Vec<String> {
    vec![
        hooked_current_exe(),
        "--hived".to_string(),
        workspace.to_string(),
        team.to_string(),
        tmux_window.to_string(),
        tmux_window_id.to_string(),
    ]
}

/// Per-loop reexec bookkeeping (the Python `code_reexec_state` dict).
#[derive(Debug)]
pub struct ReexecState {
    pub last_code_check_at: f64,
    pub candidate_hash: Option<String>,
}

impl Default for ReexecState {
    fn default() -> Self {
        ReexecState {
            // Python `state.get("last_code_check_at", 0.0)` against a large
            // uptime clock: the first check always runs.
            last_code_check_at: f64::NEG_INFINITY,
            candidate_hash: None,
        }
    }
}

/// Return a stable changed build hash that should trigger hived reexec.
pub fn _stale_disk_build_hash_for_reexec(state: &mut ReexecState, now: f64) -> Option<String> {
    if now - state.last_code_check_at < HIVED_CODE_CHECK_SECONDS {
        return None;
    }
    state.last_code_check_at = now;

    let disk_hash = hooked_compute_build_hash();
    if disk_hash == "unknown" || disk_hash == hived_build_hash() {
        state.candidate_hash = None;
        return None;
    }

    if state.candidate_hash.as_deref() == Some(disk_hash.as_str()) {
        return Some(disk_hash);
    }
    state.candidate_hash = Some(disk_hash);
    None
}

pub fn _release_reexec_lock_fd_impl(lock_fd: Option<i32>) {
    let Some(fd) = lock_fd else { return };
    unsafe {
        libc::flock(fd, libc::LOCK_UN);
        libc::close(fd);
    }
}

pub fn _try_acquire_reexec_lock_impl(workspace: &str) -> Option<i32> {
    let lock_path = _lock_path(workspace);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).ok()?;
    }
    let cpath = CString::new(lock_path.as_os_str().as_bytes()).ok()?;
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644) };
    if fd < 0 {
        return None;
    }
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        _release_reexec_lock_fd_impl(Some(fd));
        return None;
    }
    // Python os.set_inheritable(fd, True): clear FD_CLOEXEC.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        _release_reexec_lock_fd_impl(Some(fd));
        return None;
    }
    Some(fd)
}

pub fn _take_reexec_lock_fd_from_env() -> Option<i32> {
    let raw_fd = std::env::var(_HIVED_REEXEC_LOCK_ENV).unwrap_or_default();
    std::env::remove_var(_HIVED_REEXEC_LOCK_ENV);
    if raw_fd.is_empty() {
        return None;
    }
    raw_fd.parse::<i32>().ok()
}

/// What a (hooked) execv attempt reports back.
#[allow(dead_code)]
pub enum ExecOutcome {
    /// Test-only: the process would have been replaced; unreachable live.
    Replaced,
    Failed(std::io::Error),
}

fn _execv_impl(argv: &[String]) -> ExecOutcome {
    let cstrings: Vec<CString> = argv
        .iter()
        .filter_map(|a| CString::new(a.as_str()).ok())
        .collect();
    if cstrings.len() != argv.len() || cstrings.is_empty() {
        return ExecOutcome::Failed(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "bad argv",
        ));
    }
    let mut ptrs: Vec<*const libc::c_char> = cstrings.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    unsafe { libc::execv(cstrings[0].as_ptr(), ptrs.as_ptr()) };
    ExecOutcome::Failed(std::io::Error::last_os_error())
}

/// Replace this process with the on-disk build.
///
/// Returns None when nothing was torn down (another hived holds the reexec
/// lock) — the caller keeps serving on its own socket. When ``execv`` itself
/// fails, the old build has to keep serving rather than leave the window with
/// a dead hived and no socket: the listener is rebound, the output monitor
/// restarted, and the replacement socket returned for the caller to serve on.
pub fn _reexec_hived(
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
    server: &dyn HivedServerApi,
    busy_monitor: Option<&Arc<dyn OutputMonitor>>,
    on_reexec: Option<&dyn Fn()>,
) -> Option<Box<dyn HivedServerApi>> {
    let lock_fd = hooked_try_acquire_reexec_lock(workspace)?;

    let previous_lock_env = std::env::var(_HIVED_REEXEC_LOCK_ENV).ok();
    std::env::set_var(_HIVED_REEXEC_LOCK_ENV, lock_fd.to_string());
    if let Some(monitor) = busy_monitor {
        monitor.stop();
    }
    _set_output_busy_monitor(None);
    server.close();
    hooked_cleanup_socket(workspace);
    if let Some(cb) = on_reexec {
        cb();
    }
    let argv = _hived_reexec_argv(workspace, team, tmux_window, tmux_window_id);
    let outcome = hooked_execv(&argv);
    // Python's `finally`: runs whether execv returned or "raised".
    match previous_lock_env {
        None => std::env::remove_var(_HIVED_REEXEC_LOCK_ENV),
        Some(previous) => std::env::set_var(_HIVED_REEXEC_LOCK_ENV, previous),
    }
    hooked_release_reexec_lock_fd(Some(lock_fd));
    match outcome {
        ExecOutcome::Replaced => return None,
        ExecOutcome::Failed(exc) => {
            eprintln!(
                "hived: reexec failed ({exc}); staying on build {}",
                &hived_build_hash()[..hived_build_hash().len().min(12)]
            );
        }
    }

    // Only reached when execv failed. Rebinding is the recovery; if it too
    // fails the loop must die through its own teardown (Python's raised
    // OSError) — signal shutdown so the next serve tick retires it.
    let replacement = match hooked_open_server_socket(workspace) {
        Ok(replacement) => replacement,
        Err(_) => {
            _SHUTDOWN.store(true, Ordering::SeqCst);
            return None;
        }
    };
    if let Some(monitor) = busy_monitor {
        monitor.start();
        _set_output_busy_monitor(Some(Arc::clone(monitor)));
    }
    Some(replacement)
}

// --------------------------------------------------------------------------
// small helpers
// --------------------------------------------------------------------------

pub fn _now_iso() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::gmtime_r(&secs, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        tm.tm_year as i64 + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}

fn getpid() -> i64 {
    std::process::id() as i64
}

fn _hived_metadata(started_at: &str) -> Map<String, Value> {
    let mut meta = Map::new();
    meta.insert("pid".to_string(), Value::from(getpid()));
    meta.insert("started_at".to_string(), Value::from(started_at));
    meta.insert("code_hash".to_string(), Value::from(hived_build_hash()));
    meta
}

/// Python str(float) for registry createdAt round-trips.
fn py_float_str(value: f64) -> String {
    if value == value.trunc() && value.is_finite() {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

fn map_get_str(map: &Map<String, Value>, key: &str) -> String {
    match map.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => match other {
            Value::Bool(b) => {
                if *b {
                    "True".to_string()
                } else {
                    "False".to_string()
                }
            }
            _ => other.to_string(),
        },
    }
}

// --------------------------------------------------------------------------
// paths / owner file
// --------------------------------------------------------------------------

pub fn _run_dir_impl(workspace: &str) -> PathBuf {
    devlog::run_dir(Path::new(workspace))
}

pub fn _socket_path(workspace: &str) -> PathBuf {
    hooked_run_dir(workspace).join("hived.sock")
}

pub fn _lock_path(workspace: &str) -> PathBuf {
    hooked_run_dir(workspace).join("hived.lock")
}

pub fn _owner_path(workspace: &str) -> PathBuf {
    hooked_run_dir(workspace).join("hived.owner.json")
}

pub fn _write_hived_owner_impl(workspace: &str, pid: i64, started_at: &str, token: &str) {
    let path = _owner_path(workspace);
    let tmp = path.with_file_name(format!(
        "{}.{pid}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("")
    ));
    let mut payload = Map::new();
    payload.insert("pid".to_string(), Value::from(pid));
    payload.insert("startedAt".to_string(), Value::from(started_at));
    payload.insert("token".to_string(), Value::from(token));
    let write = || -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&tmp, serde_json::to_string(&payload).unwrap_or_default())?;
        fs::rename(&tmp, &path)?;
        Ok(())
    };
    if write().is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

pub fn _read_hived_owner(workspace: &str) -> Option<Map<String, Value>> {
    let text = fs::read_to_string(_owner_path(workspace)).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

fn owner_pid(owner: &Map<String, Value>) -> Option<i64> {
    match owner.get("pid") {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

pub fn _owner_matches_current_process(
    owner: Option<&Map<String, Value>>,
    owner_token: &str,
) -> bool {
    let Some(owner) = owner else { return true };
    if owner.is_empty() {
        return true;
    }
    // Python int(owner.get("pid", 0)): a missing pid is 0, an unparseable
    // one raises → True (treated as matching, i.e. not a foreign owner).
    let pid = match owner.get("pid") {
        None => 0,
        Some(_) => match owner_pid(owner) {
            Some(pid) => pid,
            None => return true,
        },
    };
    pid == getpid() && owner.get("token").and_then(Value::as_str) == Some(owner_token)
}

pub fn _foreign_owner_pid(workspace: &str, owner_token: &str) -> Option<i64> {
    let owner = _read_hived_owner(workspace);
    if _owner_matches_current_process(owner.as_ref(), owner_token) {
        return None;
    }
    Some(owner.as_ref().and_then(owner_pid).unwrap_or(0))
}

pub fn _cleanup_owner_if_current(workspace: &str, owner_token: &str) {
    let owner = _read_hived_owner(workspace);
    let Some(owner) = owner else { return };
    if owner.is_empty() || !_owner_matches_current_process(Some(&owner), owner_token) {
        return;
    }
    let _ = fs::remove_file(_owner_path(workspace));
}

pub fn _cleanup_socket_if_owner(workspace: &str, owner_token: &str) {
    let owner = _read_hived_owner(workspace);
    if let Some(owner) = owner.as_ref() {
        if !owner.is_empty() && !_owner_matches_current_process(Some(owner), owner_token) {
            return;
        }
    }
    hooked_cleanup_socket(workspace);
    _cleanup_owner_if_current(workspace, owner_token);
}

pub fn _cleanup_socket_impl(workspace: &str) {
    let _ = fs::remove_file(_socket_path(workspace));
}

// --------------------------------------------------------------------------
// client side: request helpers
// --------------------------------------------------------------------------

pub fn _request_hived(
    workspace: &str,
    payload: &Map<String, Value>,
    timeout: f64,
) -> Option<Map<String, Value>> {
    let path = _socket_path(workspace);
    if !path.exists() {
        return None;
    }
    let inner = || -> std::io::Result<Vec<u8>> {
        let mut client = UnixStream::connect(&path)?;
        let dur = Some(Duration::from_secs_f64(timeout.max(0.001)));
        client.set_read_timeout(dur)?;
        client.set_write_timeout(dur)?;
        let mut body = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
        body.push('\n');
        client.write_all(body.as_bytes())?;
        client.shutdown(std::net::Shutdown::Write)?;
        let mut chunks = Vec::new();
        let mut buf = [0u8; 65536];
        loop {
            let n = client.read(&mut buf)?;
            if n == 0 {
                break;
            }
            chunks.extend_from_slice(&buf[..n]);
        }
        Ok(chunks)
    };
    let chunks = inner().ok()?;
    if chunks.is_empty() {
        return None;
    }
    match serde_json::from_slice::<Value>(&chunks) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

fn action_payload(action: &str) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("action".to_string(), Value::from(action));
    payload
}

pub fn request_ping_impl(workspace: &str) -> Option<Map<String, Value>> {
    _request_hived(workspace, &action_payload("ping"), SOCKET_RETRY_INTERVAL)
}

pub fn _socket_alive(workspace: &str) -> bool {
    let response = hooked_request_ping(workspace);
    match response {
        Some(map) => {
            map.get("ok") == Some(&Value::Bool(true))
                && map.get("apiVersion") == Some(&Value::from(HIVED_API_VERSION))
        }
        None => false,
    }
}

/// Ask the hived to bring its shared-daemon codex client online now.
///
/// Called at spawn time so the client holds the broadcast stream before the
/// member's first turn. Best-effort: returns None when the hived is down,
/// and the lazy connect on the next runtime tick covers that case.
pub fn request_connect_codex(workspace: &str) -> Option<Map<String, Value>> {
    _request_hived(workspace, &action_payload("connect-codex"), 3.0)
}

/// Ask the hived to bring a per-pane grok 2nd client online now.
///
/// Called at spawn time so the stdio client has loaded the pane's session
/// before its first turn: ``session/load`` replays past updates, and a replay
/// is not evidence — only a live-attached client sees the first real turn.
/// Best-effort: returns None when the hived is down, and the lazy connect on
/// the next runtime tick covers that case.
pub fn request_connect_grok(workspace: &str, pane: &str) -> Option<Map<String, Value>> {
    let mut payload = action_payload("connect-grok");
    payload.insert("pane".to_string(), Value::from(pane));
    _request_hived(workspace, &payload, 3.0)
}

pub fn _hived_identity_matches(response: Option<&Map<String, Value>>, team: &str) -> bool {
    // Hived identity is (workspace socket, team) — never the window.
    //
    // The window is display: it can die, move, or be recreated by attach
    // without the team changing, so a window mismatch must not bounce a
    // healthy hived (and with it every live delivery client it holds).
    match response {
        Some(map) => {
            map.get("ok") == Some(&Value::Bool(true))
                && map.get("apiVersion") == Some(&Value::from(HIVED_API_VERSION))
                && map.get("buildHash").and_then(Value::as_str) == Some(hived_build_hash())
                && map.get("team").and_then(Value::as_str) == Some(team)
        }
        None => false,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn request_send(
    workspace: &str,
    team: &str,
    sender_agent: &str,
    sender_pane: &str,
    target_agent: &str,
    body: &str,
    artifact: &str,
    reply_to: &str,
) -> Option<Map<String, Value>> {
    let timeout = _send_request_timeout();
    let mut payload = action_payload("send");
    payload.insert("team".to_string(), Value::from(team));
    payload.insert("senderAgent".to_string(), Value::from(sender_agent));
    payload.insert("senderPane".to_string(), Value::from(sender_pane));
    payload.insert("targetAgent".to_string(), Value::from(target_agent));
    payload.insert("body".to_string(), Value::from(body));
    payload.insert("artifact".to_string(), Value::from(artifact));
    payload.insert("replyTo".to_string(), Value::from(reply_to));
    _request_hived(workspace, &payload, timeout)
}

pub fn request_doctor(
    workspace: &str,
    team: &str,
    target_agent: &str,
    verbose: bool,
) -> Option<Map<String, Value>> {
    let mut payload = action_payload("doctor");
    payload.insert("team".to_string(), Value::from(team));
    payload.insert("agent".to_string(), Value::from(target_agent));
    payload.insert("verbose".to_string(), Value::from(verbose));
    _request_hived(workspace, &payload, SOCKET_READY_TIMEOUT)
}

pub fn request_team_runtime(workspace: &str, team: &str) -> Option<Map<String, Value>> {
    let mut payload = action_payload("team-runtime");
    payload.insert("team".to_string(), Value::from(team));
    _request_hived(workspace, &payload, SOCKET_READY_TIMEOUT)
}

pub fn request_runtime_snapshot(workspace: &str, pane_id: &str) -> Option<Map<String, Value>> {
    let mut payload = action_payload("runtime-snapshot");
    payload.insert("pane".to_string(), Value::from(pane_id));
    _request_hived(workspace, &payload, SOCKET_READY_TIMEOUT)
}

pub fn request_thread(workspace: &str, message_id: &str) -> Option<Map<String, Value>> {
    let mut payload = action_payload("thread");
    payload.insert("msgId".to_string(), Value::from(message_id));
    _request_hived(workspace, &payload, SOCKET_READY_TIMEOUT)
}

// --------------------------------------------------------------------------
// server socket
// --------------------------------------------------------------------------

/// The serve loop's view of its listener (fake servers in tests implement it
/// the way the Python tests pass duck-typed `_Server` objects).
pub trait HivedServerApi: Send + Sync {
    fn close(&self);
    fn accept_timeout(&self, timeout: f64) -> Option<UnixStream>;
}

pub struct ServerSocket {
    listener: Mutex<Option<UnixListener>>,
}

impl HivedServerApi for ServerSocket {
    fn close(&self) {
        *self.listener.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn accept_timeout(&self, timeout: f64) -> Option<UnixStream> {
        let guard = self.listener.lock().unwrap_or_else(|e| e.into_inner());
        let listener = guard.as_ref()?;
        let mut pfd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = (timeout * 1000.0).ceil().max(0.0) as i32;
        let ret = unsafe { libc::poll(&mut pfd, 1, ms) };
        if ret <= 0 {
            return None;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                Some(stream)
            }
            Err(_) => None,
        }
    }
}

pub fn _open_server_socket(workspace: &str) -> Result<ServerSocket> {
    fs::create_dir_all(hooked_run_dir(workspace))?;
    _cleanup_socket_impl(workspace);
    let listener = UnixListener::bind(_socket_path(workspace))?;
    listener.set_nonblocking(true)?;
    Ok(ServerSocket {
        listener: Mutex::new(Some(listener)),
    })
}

// --------------------------------------------------------------------------
// request dispatch
// --------------------------------------------------------------------------

fn err_response(error: impl std::fmt::Display) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("ok".to_string(), Value::Bool(false));
    map.insert("error".to_string(), Value::from(error.to_string()));
    map
}

pub fn _handle_request(
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
    hived_started_at: &str,
    request: &Map<String, Value>,
) -> (Map<String, Value>, bool) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.handle_request.clone()).flatten() {
        return f(request);
    }
    let hived = _hived_metadata(hived_started_at);
    let action = request.get("action").and_then(Value::as_str).unwrap_or("");
    let team_in_request = || {
        let requested = map_get_str(request, "team");
        if requested.is_empty() {
            team.to_string()
        } else {
            requested
        }
    };
    match action {
        "ping" => {
            let mut response = Map::new();
            response.insert("ok".to_string(), Value::Bool(true));
            response.insert("apiVersion".to_string(), Value::from(HIVED_API_VERSION));
            response.insert("buildHash".to_string(), Value::from(hived_build_hash()));
            response.insert("team".to_string(), Value::from(team));
            response.insert("tmuxWindow".to_string(), Value::from(tmux_window));
            response.insert("tmuxWindowId".to_string(), Value::from(tmux_window_id));
            response.insert("hived".to_string(), Value::Object(hived));
            (response, true)
        }
        "send" => {
            let response = _send_payload(
                workspace,
                &team_in_request(),
                &map_get_str(request, "senderAgent"),
                &map_get_str(request, "senderPane"),
                &map_get_str(request, "targetAgent"),
                &map_get_str(request, "body"),
                &map_get_str(request, "artifact"),
                &map_get_str(request, "replyTo"),
            )
            .unwrap_or_else(err_response);
            (response, true)
        }
        "doctor" => {
            let response = _doctor_payload(
                workspace,
                &team_in_request(),
                &map_get_str(request, "agent"),
                request
                    .get("verbose")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                Some(&hived),
            )
            .unwrap_or_else(err_response);
            (response, true)
        }
        "team-runtime" => {
            let response = _team_runtime_payload(&team_in_request()).unwrap_or_else(err_response);
            (response, true)
        }
        "runtime-snapshot" => {
            let response = _runtime_snapshot_payload(&map_get_str(request, "pane"));
            (response, true)
        }
        "thread" => {
            let response = _thread_payload(workspace, &map_get_str(request, "msgId"))
                .unwrap_or_else(err_response);
            (response, true)
        }
        "connect-codex" => {
            let mut response = Map::new();
            response.insert("ok".to_string(), Value::Bool(true));
            response.insert("connected".to_string(), Value::Bool(hooked_cas_connect()));
            (response, true)
        }
        "connect-grok" => {
            let pane = map_get_str(request, "pane");
            let connected = !pane.is_empty() && hooked_gl_connect_pane(&pane);
            let mut response = Map::new();
            response.insert("ok".to_string(), Value::Bool(true));
            response.insert("connected".to_string(), Value::Bool(connected));
            (response, true)
        }
        "shutdown" => {
            let mut response = Map::new();
            response.insert("ok".to_string(), Value::Bool(true));
            (response, false)
        }
        _ => (err_response("unknown action"), true),
    }
}

fn _serve_connection(
    conn: UnixStream,
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
    hived_started_at: &str,
    read_timeout: f64,
) {
    _INFLIGHT_REQUESTS.fetch_add(1, Ordering::SeqCst);
    let _ = conn.set_read_timeout(Some(Duration::from_secs_f64(read_timeout.max(0.001))));
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        match (&conn).read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(_) => {
                raw.clear();
                break;
            }
        }
    }
    let request = match serde_json::from_slice::<Value>(&raw) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let (response, keep_running) = _handle_request(
        workspace,
        team,
        tmux_window,
        tmux_window_id,
        hived_started_at,
        &request,
    );
    let mut body = serde_json::to_string(&Value::Object(response)).unwrap_or_default();
    body.push('\n');
    let _ = (&conn).write_all(body.as_bytes());
    // Answer first, then retire: the reply must be on the wire before the
    // loop tears the socket down.
    if !keep_running {
        _SHUTDOWN.store(true, Ordering::SeqCst);
    }
    _INFLIGHT_REQUESTS.fetch_sub(1, Ordering::SeqCst);
}

/// Accept for up to ``timeout`` seconds, handling each request off-loop.
///
/// Handlers run on their own thread because their budgets differ by an order
/// of magnitude: a delivery may hold the native transport for
/// ``_send_request_timeout()`` while ``hive team`` / ``hive doctor`` give up
/// after ``SOCKET_READY_TIMEOUT`` and report a missing hived. Serving them
/// in accept order made one slow send fake the hived's death for every
/// short read behind it.
pub fn _serve_requests(
    server: &dyn HivedServerApi,
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
    hived_started_at: &str,
    timeout: f64,
) -> bool {
    let end = monotonic() + timeout;
    while !_SHUTDOWN.load(Ordering::SeqCst) {
        let remaining = end - monotonic();
        if remaining <= 0.0 {
            break;
        }
        let Some(conn) = server.accept_timeout(remaining) else {
            break;
        };
        let workspace = workspace.to_string();
        let team = team.to_string();
        let tmux_window = tmux_window.to_string();
        let tmux_window_id = tmux_window_id.to_string();
        let hived_started_at = hived_started_at.to_string();
        let _ = thread::Builder::new()
            .name("hived-request".to_string())
            .spawn(move || {
                _serve_connection(
                    conn,
                    &workspace,
                    &team,
                    &tmux_window,
                    &tmux_window_id,
                    &hived_started_at,
                    timeout,
                );
            });
    }
    !_SHUTDOWN.load(Ordering::SeqCst)
}

// --------------------------------------------------------------------------
// busy / transcript machinery
// --------------------------------------------------------------------------

pub fn _fresh_snapshot_session_id_impl(pane_id: &str, now: Option<f64>) -> String {
    let store = runtime_snapshots()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(snapshot) = store.get(pane_id) {
        if !snapshot.sessionId.value.is_empty() && snapshot.sessionId.is_fresh(now) {
            return snapshot.sessionId.value.clone();
        }
    }
    String::new()
}

/// Resolve the agent transcript jsonl path for a pane, with TTL cache.
///
/// Returns the absolute path string, or None when the pane has no
/// associated transcript (non-agent pane, no resolved session, etc.).
/// The cache is keyed by pane_id with a coarse TTL so the underlying
/// rglob in ``adapter.find_session_file`` does not fire on every tick.
///
/// When ``force=true`` the cache is bypassed and re-populated. Callers use
/// this to recover from a session switch (e.g. ``/new``) where the cached
/// path points at the previous session's jsonl that no longer advances.
pub fn _resolve_transcript_path_cached_impl(pane_id: &str, force: bool) -> Option<String> {
    let now = monotonic();
    let snapshot_exists = runtime_snapshots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(pane_id)
        .is_some();
    let fresh_snapshot_session_id = hooked_fresh_snapshot_session_id(pane_id, Some(now));
    if !force {
        let cache = transcript_path_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = cache.get(pane_id) {
            if now < cached.1
                && (!snapshot_exists
                    || (!fresh_snapshot_session_id.is_empty()
                        && cached.2 == fresh_snapshot_session_id))
            {
                return if cached.0.is_empty() {
                    None
                } else {
                    Some(cached.0.clone())
                };
            }
        }
    }

    let mut path_str = String::new();
    let mut sid = String::new();
    if !pane_id.is_empty() && hooked_is_pane_alive(pane_id) {
        if let Some(profile) = hooked_detect_profile_for_pane(pane_id) {
            if let Some(adapter) = hooked_adapters_get(profile.name) {
                sid = fresh_snapshot_session_id;
                if sid.is_empty() {
                    sid = adapter
                        .resolve_current_session_id(pane_id)
                        .unwrap_or_default();
                }
                if !sid.is_empty() {
                    let cwd_hint = hooked_display_value(pane_id, "#{pane_current_path}");
                    if let Some(transcript) = adapter.find_session_file(&sid, cwd_hint.as_deref()) {
                        path_str = transcript.to_string_lossy().to_string();
                    }
                }
            }
        }
    }

    transcript_path_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            pane_id.to_string(),
            (path_str.clone(), now + _TRANSCRIPT_PATH_CACHE_TTL, sid),
        );
    if path_str.is_empty() {
        None
    } else {
        Some(path_str)
    }
}

pub fn _check_mtime_within(path: &str, threshold_seconds: f64) -> Option<bool> {
    let metadata = fs::metadata(path).ok()?;
    let mtime = metadata.modified().ok()?;
    let age = std::time::SystemTime::now()
        .duration_since(mtime)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    Some(age <= threshold_seconds)
}

/// Three-state phantom-redraw gate based on transcript jsonl mtime.
///
/// Returns:
///     Some(true)  — jsonl mtime advanced within threshold (real activity)
///     Some(false) — jsonl mtime is older than threshold (phantom redraw)
///     None        — path could not be determined or stat failed; caller
///                   falls back to the underlying control-mode signal.
pub fn _transcript_progressed_recently_impl(pane_id: &str, threshold_seconds: f64) -> Option<bool> {
    let path = hooked_resolve_transcript_path_cached(pane_id, false)?;
    let progressed = _check_mtime_within(&path, threshold_seconds);
    if progressed != Some(false) {
        return progressed;
    }
    // Stale: cached path may be from a previous session. Re-resolve once.
    let fresh = hooked_resolve_transcript_path_cached(pane_id, true);
    match fresh {
        None => Some(false),
        Some(fresh) if fresh == path => Some(false),
        Some(fresh) => _check_mtime_within(&fresh, threshold_seconds),
    }
}

/// Busy flag from claude's own session registry, or None.
///
/// A bg member pane answers from its job's engine entry; an interactive
/// claude on the pane tty answers from its own registry entry (real TUI
/// sessions report ``status``; headless/desktop ones do not and stay None).
pub fn _claude_registry_busy(pane_id: &str) -> Option<bool> {
    if let Some(job_id) = hooked_cb_job_id_for_pane(pane_id) {
        let engine = hooked_cb_engine_session_for_job(&job_id)?;
        return Some(engine.status == "busy");
    }
    let reported = hooked_cs_session_status(hooked_claude_pid_for_pane(pane_id))?;
    Some(reported.0 == "busy")
}

/// Busy flag from the pane's native runtime source (codex shared
/// app-server via the pane's thread record, grok per-pane leader, claude's
/// own session registry).
///
/// None when no native source holds live state for the pane, which is the
/// signal to fall back to the heuristic monitor source.
pub fn _native_daemon_busy_impl(pane_id: &str) -> Option<bool> {
    if pane_id.is_empty() {
        return None;
    }
    if let Some(rt) = hooked_cas_runtime_for_pane(pane_id) {
        return Some(rt.busy);
    }
    if let Some(rt) = hooked_gl_runtime_for_pane(pane_id) {
        return Some(rt.busy);
    }
    _claude_registry_busy(pane_id)
}

/// Public ``busy`` signal: true when the agent is in mid-turn.
pub fn _pane_is_truly_busy(pane_id: &str, monitor: Option<&dyn OutputMonitor>) -> bool {
    if pane_id.is_empty() {
        return false;
    }

    if let Some(app_busy) = hooked_native_daemon_busy(pane_id) {
        return app_busy;
    }

    let monitor_busy = monitor
        .map(|m| m.is_busy(pane_id, BUSY_OUTPUT_THRESHOLD_SECONDS))
        .unwrap_or(false);
    if monitor_busy {
        let progressed =
            hooked_transcript_progressed_recently(pane_id, BUSY_OUTPUT_THRESHOLD_SECONDS);
        if progressed != Some(false) {
            return true;
        }
    }

    false
}

pub fn _busy_output_payload_impl(pane_id: &str) -> Map<String, Value> {
    let monitor = _get_output_busy_monitor();
    let mut map = Map::new();
    map.insert(
        "busy".to_string(),
        Value::Bool(_pane_is_truly_busy(pane_id, monitor.as_deref())),
    );
    map
}

/// idle-notify variant of `_pane_is_truly_busy` with the inactive_age clamp.
pub fn _is_output_busy(
    pane_id: &str,
    monitor: Option<&dyn OutputMonitor>,
    inactive_age: Option<f64>,
) -> bool {
    if pane_id.is_empty() {
        return false;
    }

    if let Some(app_busy) = hooked_native_daemon_busy(pane_id) {
        return app_busy;
    }

    if let Some(m) = monitor {
        if m.is_busy(pane_id, BUSY_OUTPUT_THRESHOLD_SECONDS) {
            let progressed =
                hooked_transcript_progressed_recently(pane_id, BUSY_OUTPUT_THRESHOLD_SECONDS);
            if progressed != Some(false) {
                let Some(inactive_age) = inactive_age else {
                    return true;
                };
                if let Some(output_age) = m.last_output_age(pane_id) {
                    if output_age < inactive_age {
                        return true;
                    }
                }
            }
        }
    }

    false
}

pub fn _most_recent_output_pane(panes: &[String], monitor: Option<&dyn OutputMonitor>) -> String {
    let Some(monitor) = monitor else {
        return String::new();
    };
    let mut candidates: Vec<(f64, String)> = Vec::new();
    for pane_id in panes {
        if let Some(age) = monitor.last_output_age(pane_id) {
            candidates.push((age, pane_id.clone()));
        }
    }
    candidates
        .into_iter()
        .min_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        })
        .map(|(_, pane)| pane)
        .unwrap_or_default()
}

fn _idle_notify_target_pane(
    panes: &[String],
    record: &IdleRecord,
    busy_monitor: Option<&dyn OutputMonitor>,
) -> String {
    if let Some(recorded) = record.last_busy_pane.as_ref() {
        if !recorded.is_empty() && panes.iter().any(|p| p == recorded) {
            return recorded.clone();
        }
    }
    let recent = _most_recent_output_pane(panes, busy_monitor);
    if !recent.is_empty() {
        return recent;
    }
    panes.first().cloned().unwrap_or_default()
}

// --------------------------------------------------------------------------
// per-CLI runtime payloads
// --------------------------------------------------------------------------

/// Native codex runtime from the shared daemon, or None if unmanaged.
pub fn _codex_app_server_runtime_impl(pane_id: &str) -> Option<Map<String, Value>> {
    let rt = hooked_cas_runtime_for_pane(pane_id)?;
    let input_state = if rt.input_state.is_empty() {
        "ready".to_string()
    } else {
        rt.input_state.clone()
    };
    let mut fields = Map::new();
    fields.insert("busy".to_string(), Value::Bool(rt.busy));
    fields.insert("turnPhase".to_string(), Value::from(rt.turn_phase.clone()));
    fields.insert("inputState".to_string(), Value::from(input_state.clone()));
    fields.insert(
        "inputReason".to_string(),
        Value::from(if input_state != "waiting_user" {
            ""
        } else {
            "app_server_active_flag"
        }),
    );
    fields.insert(
        "_runtimeSource".to_string(),
        Value::from("codex_app_server"),
    );
    Some(fields)
}

/// Native grok runtime from the pane's leader, or None if no daemon.
pub fn _grok_leader_runtime(pane_id: &str) -> Option<Map<String, Value>> {
    let rt = hooked_gl_runtime_for_pane(pane_id)?;
    let input_state = if rt.input_state.is_empty() {
        "ready".to_string()
    } else {
        rt.input_state.clone()
    };
    let mut fields = Map::new();
    fields.insert("busy".to_string(), Value::Bool(rt.busy));
    fields.insert("turnPhase".to_string(), Value::from(rt.turn_phase.clone()));
    fields.insert("inputState".to_string(), Value::from(input_state.clone()));
    fields.insert(
        "inputReason".to_string(),
        Value::from(if input_state != "waiting_user" {
            ""
        } else {
            "leader_permission_request"
        }),
    );
    fields.insert("_runtimeSource".to_string(), Value::from("grok-leader"));
    Some(fields)
}

/// Native claude runtime from the pane's bg job, or None if unmanaged.
pub fn _claude_bg_runtime_impl(pane_id: &str) -> Option<Map<String, Value>> {
    let (job_id, record_session, _cwd) = hooked_cb_read_pane_job(pane_id)?;
    Some(_claude_job_runtime(&job_id, &record_session))
}

/// Native claude runtime keyed by the job itself (pane optional).
///
/// Liveness is three-tier: a live engine entry (alive — its ``status`` is
/// the truth); a ledger row without a live engine (asleep — the supervisor
/// parks idle jobs after ~1h, delivery wakes them, so asleep is not dead
/// and is never reaped); no ledger row (gone). The ledger costs a CLI call
/// (~270ms), so it is consulted only when the engine entry is missing,
/// behind a short cache.
pub fn _claude_job_runtime(job_id: &str, record_session: &str) -> Map<String, Value> {
    if let Some(engine) = hooked_cb_engine_session_for_job(job_id) {
        let mut fields = crate::adapters::claude_bg::runtime_from_engine(&engine, None);
        fields.insert("cliAlive".to_string(), Value::Bool(true));
        let sid = if !engine.session_id.is_empty() {
            engine.session_id.clone()
        } else if !record_session.is_empty() {
            record_session.to_string()
        } else {
            "unresolved".to_string()
        };
        fields.insert("sessionId".to_string(), Value::from(sid));
        return fields;
    }
    let mut fields = Map::new();
    fields.insert("_runtimeSource".to_string(), Value::from("claude_bg"));
    fields.insert("busy".to_string(), Value::Bool(false));
    let fallback_sid = |value: &str| {
        if !value.is_empty() {
            value.to_string()
        } else if !record_session.is_empty() {
            record_session.to_string()
        } else {
            "unresolved".to_string()
        }
    };
    let Some(rows) = _claude_jobs_cached() else {
        fields.insert("cliAlive".to_string(), Value::Bool(true));
        fields.insert("inputState".to_string(), Value::from("unknown"));
        fields.insert("inputReason".to_string(), Value::from("ledger_unavailable"));
        fields.insert("sessionId".to_string(), Value::from(fallback_sid("")));
        return fields;
    };
    let Some(row) = rows.get(job_id) else {
        fields.insert("cliAlive".to_string(), Value::Bool(false));
        fields.insert("inputState".to_string(), Value::from("offline"));
        fields.insert("inputReason".to_string(), Value::from("engine_gone"));
        fields.insert("sessionId".to_string(), Value::from(fallback_sid("")));
        return fields;
    };
    // Asleep: parked engine. It still accepts input — delivery wakes it — so
    // it reads as an idle, reachable member, never as a dead one.
    let row_sid = row
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    fields.insert("cliAlive".to_string(), Value::Bool(true));
    fields.insert("_engineState".to_string(), Value::from("asleep"));
    fields.insert("inputState".to_string(), Value::from("ready"));
    fields.insert("inputReason".to_string(), Value::from(""));
    fields.insert("sessionId".to_string(), Value::from(fallback_sid(row_sid)));
    fields
}

/// What the pane's attach viewer is actually showing (the human can switch
/// it to any other bg session).
pub fn _claude_view_fields(pane_id: &str) -> Map<String, Value> {
    let view = hooked_cv_view_for_pane(pane_id, None);
    let mut fields = Map::new();
    fields.insert("_viewKind".to_string(), Value::from(view.kind));
    fields.insert("_viewCertainty".to_string(), Value::from(view.certainty));
    fields.insert("_viewedJob".to_string(), Value::from(view.job_id));
    fields.insert("_viewedMember".to_string(), Value::from(view.member));
    fields
}

/// Job ledger rows keyed by jobId, or None when the CLI call failed.
///
/// Cached briefly: the ledger is only read when an engine entry is missing
/// (rare state), and a ~270ms node start must not run per tick per pane.
pub fn _claude_jobs_cached() -> Option<HashMap<String, Map<String, Value>>> {
    let now = monotonic();
    {
        let cache = claude_jobs_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((expiry, indexed)) = cache.as_ref() {
            if now < *expiry {
                return indexed.clone();
            }
        }
    }
    let rows = hooked_cb_list_jobs();
    let indexed = rows.map(|rows| {
        let mut map = HashMap::new();
        for row in rows {
            let id = row.get("id").and_then(Value::as_str).unwrap_or_default();
            if !id.is_empty() {
                map.insert(id.to_string(), row);
            }
        }
        map
    });
    *claude_jobs_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some((now + _CLAUDE_JOBS_CACHE_TTL, indexed.clone()));
    indexed
}

pub fn _agent_runtime_payload(
    pane_id: &str,
    runtime_snapshot: Option<&RuntimeSnapshot>,
) -> Map<String, Value> {
    let mut runtime = Map::new();
    let alive = hooked_is_pane_alive(pane_id);
    runtime.insert("alive".to_string(), Value::Bool(alive));
    for (key, value) in hooked_busy_output_payload(pane_id) {
        runtime.insert(key, value);
    }
    if !alive {
        runtime.insert("cliAlive".to_string(), Value::Bool(false));
        runtime.insert("busy".to_string(), Value::Bool(false));
        runtime.insert("inputState".to_string(), Value::from("offline"));
        runtime.insert("inputReason".to_string(), Value::from("pane_dead"));
        return runtime;
    }

    // Liveness is runtime evidence only: a retained shell keeps the pane, a
    // stale title, the @hive-cli tag and a surviving thread/job record alive,
    // and none of that alone makes it an agent runtime. For claude the
    // evidence is the bg job's registry/ledger state — the engine never
    // lives on the pane tty, so the process table only proves the viewer.
    let profile = hooked_detect_cli_process_for_pane(pane_id);
    runtime.insert("cliAlive".to_string(), Value::Bool(profile.is_some()));
    runtime.insert(
        "_cli".to_string(),
        Value::from(profile.map(|p| p.name).unwrap_or("unknown")),
    );
    if profile.is_none() || profile.map(|p| p.name) == Some("claude") {
        if let Some(bg_runtime) = hooked_claude_bg_runtime(pane_id) {
            runtime.insert("_cli".to_string(), Value::from("claude"));
            let resolved_model = hooked_resolve_model_for_pane(pane_id, "claude", "");
            if !resolved_model.is_empty() {
                runtime.insert("model".to_string(), Value::from(resolved_model));
            }
            for (key, value) in bg_runtime {
                runtime.insert(key, value);
            }
            for (key, value) in _claude_view_fields(pane_id) {
                runtime.insert(key, value);
            }
            return runtime;
        }
    }
    let Some(profile) = profile else {
        runtime.insert("busy".to_string(), Value::Bool(false)); // shell output is not agent activity
        runtime.insert("inputState".to_string(), Value::from("offline"));
        runtime.insert("inputReason".to_string(), Value::from("cli_exited"));
        return runtime;
    };

    let resolved_model = hooked_resolve_model_for_pane(pane_id, profile.name, "");
    if !resolved_model.is_empty() {
        runtime.insert("model".to_string(), Value::from(resolved_model));
    }

    let Some(adapter) = hooked_adapters_get(profile.name) else {
        runtime.insert("inputState".to_string(), Value::from("unknown"));
        runtime.insert("inputReason".to_string(), Value::from("no_session"));
        return runtime;
    };

    // A hive-managed codex has a recorded thread on the shared app-server
    // daemon: read native runtime signals (busy / turn) over the socket
    // instead of reverse-engineering them from the transcript, and its
    // session id IS the recorded threadId — no probing. An unmanaged codex
    // (no record) falls through to the transcript path below.
    if profile.name == "codex" {
        if let Some(app_runtime) = hooked_codex_app_server_runtime(pane_id) {
            for (key, value) in app_runtime {
                runtime.insert(key, value);
            }
            runtime.insert(
                "sessionId".to_string(),
                Value::from(
                    hooked_cas_session_id_for_pane(pane_id)
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "unresolved".to_string()),
                ),
            );
            return runtime;
        }
    }

    // hive-spawned grok is the same shape over its per-pane leader daemon,
    // and its session id needs no probing: hive minted it at spawn time and
    // wrote it beside the socket. Unlike codex it never falls through to the
    // transcript path — that gate only knows claude/codex record shapes and
    // reads a pending grok permission request as clear — so with no leader
    // state the honest answer is unknown.
    if profile.name == "grok" {
        let leader_runtime = _grok_leader_runtime(pane_id);
        runtime.insert(
            "sessionId".to_string(),
            Value::from(
                hooked_gl_session_id_for_pane(pane_id)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "unresolved".to_string()),
            ),
        );
        match leader_runtime {
            Some(fields) => {
                for (key, value) in fields {
                    runtime.insert(key, value);
                }
            }
            None => {
                runtime.insert("inputState".to_string(), Value::from("unknown"));
                runtime.insert("inputReason".to_string(), Value::from("no_leader_runtime"));
            }
        }
        return runtime;
    }

    let session_id;
    let snapshot_fresh = runtime_snapshot
        .map(|s| !s.sessionId.value.is_empty() && s.sessionId.is_fresh(None))
        .unwrap_or(false);
    if snapshot_fresh {
        let snapshot = runtime_snapshot.unwrap();
        for (key, value) in snapshot.to_runtime_fields(None) {
            runtime.insert(key, value);
        }
        session_id = snapshot.sessionId.value.clone();
    } else {
        session_id = adapter
            .resolve_current_session_id(pane_id)
            .unwrap_or_default();
        let source = if session_id.is_empty() { "" } else { "adapter" };
        runtime.insert(
            "sessionId".to_string(),
            Value::from(if session_id.is_empty() {
                "unresolved".to_string()
            } else {
                session_id.clone()
            }),
        );
        if !session_id.is_empty() {
            let snapshot = runtime_snapshots()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .update_session_id(
                    pane_id,
                    &session_id,
                    source,
                    None,
                    Some(_SESSION_SNAPSHOT_FRESHNESS_S),
                );
            for (key, value) in snapshot.to_runtime_fields(None) {
                runtime.insert(key, value);
            }
        }
    }

    // An interactive claude reports its own state in the session registry —
    // the same fields the bg engine path maps. It is the authority when it
    // speaks: the transcript gate can only see an AskUserQuestion record, so
    // it reads every other wait (and a stale ask) wrong, and the send gate
    // refuses on that verdict.
    if profile.name == "claude" {
        if let Some((status, waiting_for)) =
            hooked_cs_session_status(hooked_claude_pid_for_pane(pane_id))
        {
            for (key, value) in
                crate::adapters::claude_sessions::runtime_from_status(&status, &waiting_for)
            {
                runtime.insert(key, value);
            }
            runtime.insert("_runtimeSource".to_string(), Value::from("claude_registry"));
            return runtime;
        }
    }

    if session_id.is_empty() {
        runtime.insert("inputState".to_string(), Value::from("unknown"));
        runtime.insert("inputReason".to_string(), Value::from("no_session"));
        return runtime;
    }

    let cwd_hint = hooked_display_value(pane_id, "#{pane_current_path}");
    let transcript = adapter.find_session_file(&session_id, cwd_hint.as_deref());
    runtime.insert(
        "_transcript".to_string(),
        match transcript.as_ref() {
            Some(path) => Value::from(path.to_string_lossy().to_string()),
            None => Value::Null,
        },
    );
    let Some(transcript) = transcript else {
        runtime.insert("inputState".to_string(), Value::from("unknown"));
        runtime.insert("inputReason".to_string(), Value::from("transcript_missing"));
        return runtime;
    };

    let exists = transcript.exists();
    runtime.insert("_transcriptExists".to_string(), Value::Bool(exists));
    if !exists {
        runtime.insert("inputState".to_string(), Value::from("unknown"));
        runtime.insert("inputReason".to_string(), Value::from("transcript_missing"));
        return runtime;
    }

    runtime.insert(
        "_transcriptSize".to_string(),
        Value::from(fs::metadata(&transcript).map(|m| m.len()).unwrap_or(0)),
    );
    let gate = hooked_check_input_gate(&transcript);
    runtime.insert("_gate".to_string(), Value::from(gate.status));
    runtime.insert("_gateReason".to_string(), Value::from(gate.reason.clone()));
    if gate.status == "waiting" {
        runtime.insert("inputState".to_string(), Value::from("waiting_user"));
        runtime.insert("inputReason".to_string(), Value::from("ask_pending"));
    } else if gate.status == "clear" {
        runtime.insert("inputState".to_string(), Value::from("ready"));
        runtime.insert("inputReason".to_string(), Value::from(""));
    } else {
        runtime.insert("inputState".to_string(), Value::from("unknown"));
        runtime.insert(
            "inputReason".to_string(),
            Value::from(if gate.reason.is_empty() {
                "read_error".to_string()
            } else {
                gate.reason
            }),
        );
    }
    runtime
}

/// Runtime for a registry member with no pane: the engine IS the member.
///
/// ``alive`` mirrors engine liveness (there is no pane to be alive), and
/// ``headless`` marks the row so consumers can tell a closed display from a
/// dead member.
pub fn _headless_member_runtime(agent: &Agent) -> Map<String, Value> {
    let mut runtime = Map::new();
    runtime.insert("alive".to_string(), Value::Bool(false));
    runtime.insert("headless".to_string(), Value::Bool(true));
    runtime.insert("busy".to_string(), Value::Bool(false));
    let sid = agent.session_id.clone().unwrap_or_default();
    let cli = agent.cli.as_str();
    if cli == "claude" && !sid.is_empty() {
        let mut job_rt = _claude_job_runtime(&sid, "");
        if job_rt.get("cliAlive") != Some(&Value::Bool(true)) {
            let live = hooked_cs_list_sessions()
                .into_iter()
                .find(|s| s.session_id == sid);
            if let Some(live) = live {
                // A joined interactive session: its registry status is the
                // runtime, its channel liveness is the pulse.
                let status = hooked_cs_session_status(Some(live.pid));
                let mut session_rt = Map::new();
                session_rt.insert("cliAlive".to_string(), Value::Bool(true));
                session_rt.insert("sessionId".to_string(), Value::from(sid.clone()));
                session_rt.insert("_runtimeSource".to_string(), Value::from("claude_session"));
                match status {
                    Some((status, waiting_for)) => {
                        for (key, value) in crate::adapters::claude_sessions::runtime_from_status(
                            &status,
                            &waiting_for,
                        ) {
                            session_rt.insert(key, value);
                        }
                    }
                    None => {
                        session_rt.insert("busy".to_string(), Value::Bool(false));
                        session_rt.insert("inputState".to_string(), Value::from("ready"));
                        session_rt.insert("inputReason".to_string(), Value::from(""));
                    }
                }
                job_rt = session_rt;
            }
        }
        for (key, value) in job_rt {
            runtime.insert(key, value);
        }
    } else if cli == "codex" && !sid.is_empty() {
        match hooked_cas_runtime_for_thread(&sid) {
            None => {
                runtime.insert("cliAlive".to_string(), Value::Bool(false));
                runtime.insert("inputState".to_string(), Value::from("unknown"));
                runtime.insert("inputReason".to_string(), Value::from("no_daemon_runtime"));
            }
            Some(rt) => {
                let input_state = if rt.input_state.is_empty() {
                    "ready".to_string()
                } else {
                    rt.input_state.clone()
                };
                runtime.insert("cliAlive".to_string(), Value::Bool(true));
                runtime.insert("busy".to_string(), Value::Bool(rt.busy));
                runtime.insert("turnPhase".to_string(), Value::from(rt.turn_phase.clone()));
                runtime.insert("inputState".to_string(), Value::from(input_state.clone()));
                runtime.insert(
                    "inputReason".to_string(),
                    Value::from(if input_state != "waiting_user" {
                        ""
                    } else {
                        "app_server_active_flag"
                    }),
                );
                runtime.insert(
                    "_runtimeSource".to_string(),
                    Value::from("codex_app_server"),
                );
            }
        }
        runtime.insert("sessionId".to_string(), Value::from(sid));
    } else if cli == "grok" {
        let key = crate::adapters::grok_leader::member_key(&agent.team_name, &agent.name);
        match hooked_gl_runtime_for_key(&key) {
            None => {
                runtime.insert("cliAlive".to_string(), Value::Bool(false));
                runtime.insert("inputState".to_string(), Value::from("unknown"));
                runtime.insert("inputReason".to_string(), Value::from("no_leader_runtime"));
            }
            Some(rt) => {
                let input_state = if rt.input_state.is_empty() {
                    "ready".to_string()
                } else {
                    rt.input_state.clone()
                };
                runtime.insert("cliAlive".to_string(), Value::Bool(true));
                runtime.insert("busy".to_string(), Value::Bool(rt.busy));
                runtime.insert("turnPhase".to_string(), Value::from(rt.turn_phase.clone()));
                runtime.insert("inputState".to_string(), Value::from(input_state.clone()));
                runtime.insert(
                    "inputReason".to_string(),
                    Value::from(if input_state != "waiting_user" {
                        ""
                    } else {
                        "leader_permission_request"
                    }),
                );
                runtime.insert("_runtimeSource".to_string(), Value::from("grok-leader"));
            }
        }
        let record = hooked_gl_read_session_key(&key);
        let record_sid = record.map(|(sid, _)| sid).unwrap_or_default();
        let final_sid = if !record_sid.is_empty() {
            record_sid
        } else if !sid.is_empty() {
            sid
        } else {
            "unresolved".to_string()
        };
        runtime.insert("sessionId".to_string(), Value::from(final_sid));
    } else {
        runtime.insert("cliAlive".to_string(), Value::Bool(false));
        runtime.insert("inputState".to_string(), Value::from("unknown"));
        runtime.insert("inputReason".to_string(), Value::from("no_engine_identity"));
    }
    let alive = runtime.get("cliAlive") == Some(&Value::Bool(true));
    runtime.insert("alive".to_string(), Value::Bool(alive));
    runtime
}

pub fn _member_runtime_payload_impl(pane_id: &str, role: &str) -> Map<String, Value> {
    if role != "agent" {
        let mut payload = Map::new();
        payload.insert(
            "alive".to_string(),
            Value::Bool(hooked_is_pane_alive(pane_id)),
        );
        for (key, value) in hooked_busy_output_payload(pane_id) {
            payload.insert(key, value);
        }
        return payload;
    }
    let snapshot = runtime_snapshots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(pane_id)
        .cloned();
    _agent_runtime_payload(pane_id, snapshot.as_ref())
}

pub fn _team_runtime_payload(team_name: &str) -> Result<Map<String, Value>> {
    let team = hooked_team_load(team_name)?;
    let mut members = Map::new();
    let mut needs_answer: Vec<String> = Vec::new();

    if let Some(lead) = team.lead_agent() {
        let role = hooked_member_role_for_pane(&lead.pane_id);
        let runtime = hooked_member_runtime_payload(&lead.pane_id, role);
        if runtime.get("inputState").and_then(Value::as_str) == Some("waiting_user") {
            needs_answer.push(lead.name.clone());
        }
        members.insert(lead.name.clone(), Value::Object(runtime));
    }

    let mut sorted_agents: Vec<&Agent> = team.agents.iter().collect();
    sorted_agents.sort_by(|a, b| a.name.cmp(&b.name));
    for agent in sorted_agents {
        let runtime = if !agent.pane_id.is_empty() {
            hooked_member_runtime_payload(&agent.pane_id, "agent")
        } else {
            _headless_member_runtime(agent)
        };
        if runtime.get("inputState").and_then(Value::as_str) == Some("waiting_user") {
            needs_answer.push(agent.name.clone());
        }
        members.insert(agent.name.clone(), Value::Object(runtime));
    }

    let mut payload = Map::new();
    payload.insert("ok".to_string(), Value::Bool(true));
    payload.insert("team".to_string(), Value::from(team_name));
    payload.insert("members".to_string(), Value::Object(members));
    if !needs_answer.is_empty() {
        payload.insert(
            "needsAnswer".to_string(),
            Value::Array(needs_answer.into_iter().map(Value::from).collect()),
        );
    }
    Ok(payload)
}

pub fn _runtime_snapshot_payload(pane_id: &str) -> Map<String, Value> {
    if pane_id.is_empty() {
        return err_response("pane required");
    }
    let snapshot = runtime_snapshots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(pane_id)
        .cloned();
    let mut payload = Map::new();
    payload.insert("ok".to_string(), Value::Bool(true));
    payload.insert("pane".to_string(), Value::from(pane_id));
    payload.insert(
        "snapshot".to_string(),
        match snapshot {
            Some(s) => Value::Object(s.to_runtime_fields(None)),
            None => Value::Null,
        },
    );
    payload
}

pub fn _team_member_bindings_impl(team_name: &str) -> Result<Vec<(String, Map<String, Value>)>> {
    let team = hooked_team_load(team_name)?;
    let mut members: Vec<(String, Map<String, Value>)> = Vec::new();
    let mut upsert = |name: String, row: Map<String, Value>| match members
        .iter_mut()
        .find(|(n, _)| *n == name)
    {
        Some(slot) => slot.1 = row,
        None => members.push((name, row)),
    };

    if let Some(lead) = team.lead_agent() {
        let mut row = Map::new();
        row.insert("name".to_string(), Value::from(lead.name.clone()));
        row.insert(
            "role".to_string(),
            Value::from(hooked_member_role_for_pane(&lead.pane_id)),
        );
        row.insert("pane".to_string(), Value::from(lead.pane_id.clone()));
        row.insert("cli".to_string(), Value::from(lead.cli.clone()));
        upsert(lead.name.clone(), row);
    }

    let mut sorted_agents: Vec<&Agent> = team.agents.iter().collect();
    sorted_agents.sort_by(|a, b| a.name.cmp(&b.name));
    for agent in sorted_agents {
        let mut row = Map::new();
        row.insert("name".to_string(), Value::from(agent.name.clone()));
        row.insert("role".to_string(), Value::from("agent"));
        row.insert("pane".to_string(), Value::from(agent.pane_id.clone()));
        row.insert("cli".to_string(), Value::from(agent.cli.clone()));
        upsert(agent.name.clone(), row);
    }

    Ok(members)
}

pub fn _idle_notify_agent_panes_impl(team_name: &str) -> Vec<String> {
    let bindings = hooked_team_member_bindings(team_name).unwrap_or_default();
    let mut panes: Vec<String> = Vec::new();
    for (_, member) in bindings {
        if member.get("role").and_then(Value::as_str) != Some("agent") {
            continue;
        }
        let pane_id = map_get_str(&member, "pane");
        if !pane_id.is_empty()
            && !panes.contains(&pane_id)
            && hooked_is_pane_alive(&pane_id)
            && hooked_detect_cli_process_for_pane(&pane_id).is_some()
        {
            panes.push(pane_id);
        }
    }
    panes
}

// --------------------------------------------------------------------------
// idle notify state machine
// --------------------------------------------------------------------------

/// One window's idle-notify record (the Python per-window state dict).
#[derive(Debug, Clone, PartialEq)]
pub struct IdleRecord {
    pub last_busy_ts: f64,
    pub notified: bool,
    pub seen_since_fire: bool,
    pub missing_ticks: i64,
    pub last_busy_pane: Option<String>,
}

impl IdleRecord {
    pub fn new(last_busy_ts: f64, notified: bool, seen_since_fire: bool) -> IdleRecord {
        IdleRecord {
            last_busy_ts,
            notified,
            seen_since_fire,
            missing_ticks: 0,
            last_busy_pane: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WinDebug {
    pub busy_observed: bool,
    pub observed_token: Option<String>,
}

/// The Python `debug_state` dict ("__init__" sentinels become None).
#[derive(Debug)]
pub struct NotifyDebugState {
    pub tick_seq: u64,
    pub windows: HashMap<String, WinDebug>,
    pub active_window: Option<String>,
    pub inactive_at: HashMap<String, f64>,
    pub windows_keys: Option<Vec<String>>,
    pub last_heartbeat: f64,
}

impl Default for NotifyDebugState {
    fn default() -> Self {
        NotifyDebugState {
            tick_seq: 0,
            windows: HashMap::new(),
            active_window: None,
            inactive_at: HashMap::new(),
            windows_keys: None,
            // Python `debug_state.get("last_heartbeat", 0.0)` against a
            // large uptime clock: the first tick emits a heartbeat.
            last_heartbeat: f64::NEG_INFINITY,
        }
    }
}

fn record_state_value(record: &IdleRecord) -> Value {
    let mut map = Map::new();
    map.insert("notified".to_string(), Value::Bool(record.notified));
    map.insert(
        "seen_since_fire".to_string(),
        Value::Bool(record.seen_since_fire),
    );
    map.insert("last_busy_ts".to_string(), Value::from(record.last_busy_ts));
    Value::Object(map)
}

fn notify_state_value(notified: bool, seen_since_fire: bool) -> Value {
    let mut map = Map::new();
    map.insert("notified".to_string(), Value::Bool(notified));
    map.insert("seen_since_fire".to_string(), Value::Bool(seen_since_fire));
    Value::Object(map)
}

#[allow(clippy::too_many_arguments)]
pub fn _idle_notify_tick(
    team_name: &str,
    session_name: &str,
    idle_notify: &mut HashMap<String, IdleRecord>,
    busy_monitor: Option<&dyn OutputMonitor>,
    now: f64,
    workspace: &str,
    debug_state: Option<&mut NotifyDebugState>,
    members: Option<&[(String, Map<String, Value>)]>,
) {
    let mut local_debug = NotifyDebugState::default();
    let debug_state = match debug_state {
        Some(state) => state,
        None => &mut local_debug,
    };
    debug_state.tick_seq += 1;

    let active_window = hooked_get_most_recent_client_window(session_name).unwrap_or_default();

    let agent_panes: Vec<String> = match members {
        Some(members) => {
            let mut panes: Vec<String> = Vec::new();
            for (_, member) in members {
                if member.get("role").and_then(Value::as_str) != Some("agent") {
                    continue;
                }
                let pane_id = map_get_str(member, "pane");
                if !pane_id.is_empty()
                    && !panes.contains(&pane_id)
                    && hooked_is_pane_alive(&pane_id)
                    && hooked_detect_cli_process_for_pane(&pane_id).is_some()
                {
                    panes.push(pane_id);
                }
            }
            panes
        }
        None => hooked_idle_notify_agent_panes(team_name),
    };
    let mut windows: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for pane_id in &agent_panes {
        let window_target = hooked_get_pane_window_target(pane_id).unwrap_or_default();
        if window_target.is_empty() {
            continue;
        }
        windows
            .entry(window_target)
            .or_default()
            .push(pane_id.clone());
    }

    let prev_active_initialized = debug_state.active_window.is_some();
    let prev_active = debug_state.active_window.clone().unwrap_or_default();
    if !prev_active_initialized || prev_active != active_window {
        hooked_notify_debug_emit(
            workspace,
            "active.changed",
            &[
                ("team", Value::from(team_name)),
                (
                    "old",
                    if prev_active_initialized {
                        Value::from(prev_active.clone())
                    } else {
                        Value::Null
                    },
                ),
                (
                    "new",
                    if active_window.is_empty() {
                        Value::Null
                    } else {
                        Value::from(active_window.clone())
                    },
                ),
            ],
        );
        // Stamp the moment the previous active window became inactive so the
        // busy check can ignore output that the user already saw while it was
        // active. The newly-active window has no inactive boundary.
        if prev_active_initialized && !prev_active.is_empty() {
            debug_state.inactive_at.insert(prev_active.clone(), now);
        }
        if !active_window.is_empty() {
            debug_state.inactive_at.remove(&active_window);
        }
        debug_state.active_window = Some(active_window.clone());
    }

    let new_keys: Vec<String> = windows.keys().cloned().collect();
    if debug_state.windows_keys.as_ref() != Some(&new_keys) {
        hooked_notify_debug_emit(
            workspace,
            "windows.changed",
            &[
                ("team", Value::from(team_name)),
                (
                    "old",
                    match debug_state.windows_keys.as_ref() {
                        Some(keys) => Value::Array(keys.iter().cloned().map(Value::from).collect()),
                        None => Value::Null,
                    },
                ),
                (
                    "new",
                    Value::Array(new_keys.iter().cloned().map(Value::from).collect()),
                ),
            ],
        );
        debug_state.windows_keys = Some(new_keys.clone());
    }

    let token_key = crate::notify_ui::NOTIFY_TOKEN_OPTION.trim_start_matches('@');
    if windows.contains_key(&active_window) {
        let token = hooked_get_window_option(&active_window, token_key).unwrap_or_default();
        if !token.is_empty() {
            let mut sorted_panes = windows[&active_window].clone();
            sorted_panes.sort();
            hooked_notify_debug_emit(
                workspace,
                "active.clear_attempt",
                &[
                    ("team", Value::from(team_name)),
                    ("window", Value::from(active_window.clone())),
                    ("token", Value::from(token.clone())),
                    (
                        "panes",
                        Value::Array(sorted_panes.iter().cloned().map(Value::from).collect()),
                    ),
                ],
            );
            hooked_clear_stale_notify(
                &active_window,
                &sorted_panes,
                &token,
                false,
                "hived.active_window",
                workspace,
            );
        }
    }

    if !hooked_is_plugin_enabled("notify") {
        if !idle_notify.is_empty() {
            hooked_notify_debug_emit(
                workspace,
                "plugin.disabled",
                &[
                    ("team", Value::from(team_name)),
                    ("records_cleared", Value::from(idle_notify.len())),
                ],
            );
        }
        idle_notify.clear();
        return;
    }

    let known_windows: Vec<String> = idle_notify.keys().cloned().collect();
    for window_target in known_windows {
        if windows.contains_key(&window_target) {
            if let Some(record) = idle_notify.get_mut(&window_target) {
                record.missing_ticks = 0;
            }
            continue;
        }
        let Some(record) = idle_notify.get_mut(&window_target) else {
            continue;
        };
        record.missing_ticks += 1;
        if record.missing_ticks >= IDLE_NOTIFY_MISSING_PRUNE_TICKS {
            let mut last_state = Map::new();
            last_state.insert("notified".to_string(), Value::Bool(record.notified));
            last_state.insert(
                "seen_since_fire".to_string(),
                Value::Bool(record.seen_since_fire),
            );
            last_state.insert("last_busy_ts".to_string(), Value::from(record.last_busy_ts));
            let missing_ticks = record.missing_ticks;
            hooked_notify_debug_emit(
                workspace,
                "record.prune",
                &[
                    ("team", Value::from(team_name)),
                    ("window", Value::from(window_target.clone())),
                    ("missing_ticks", Value::from(missing_ticks)),
                    ("last_state", Value::Object(last_state)),
                ],
            );
            idle_notify.remove(&window_target);
            debug_state.windows.remove(&window_target);
            debug_state.inactive_at.remove(&window_target);
        }
    }

    for (window_target, window_panes) in &windows {
        let mut panes = window_panes.clone();
        panes.sort();
        let record_existed = idle_notify.contains_key(window_target);
        let record = idle_notify
            .entry(window_target.clone())
            .or_insert_with(|| IdleRecord::new(now, true, true));
        let win_dbg = debug_state
            .windows
            .entry(window_target.clone())
            .or_default();
        if !record_existed {
            let mut initial = Map::new();
            initial.insert("last_busy_ts".to_string(), Value::from(record.last_busy_ts));
            initial.insert("notified".to_string(), Value::Bool(record.notified));
            initial.insert(
                "seen_since_fire".to_string(),
                Value::Bool(record.seen_since_fire),
            );
            hooked_notify_debug_emit(
                workspace,
                "record.create",
                &[
                    ("team", Value::from(team_name)),
                    ("window", Value::from(window_target.clone())),
                    (
                        "panes",
                        Value::Array(panes.iter().cloned().map(Value::from).collect()),
                    ),
                    ("initial", Value::Object(initial)),
                ],
            );
        }
        record.missing_ticks = 0;

        if *window_target == active_window {
            let state_before = record_state_value(record);
            let was_seen = record.seen_since_fire;
            let was_notified = record.notified;
            record.last_busy_ts = now;
            record.notified = true;
            record.seen_since_fire = true;
            if !was_seen || !was_notified {
                hooked_notify_debug_emit(
                    workspace,
                    "active.block",
                    &[
                        ("team", Value::from(team_name)),
                        ("window", Value::from(window_target.clone())),
                        ("state_before", state_before),
                    ],
                );
            }
            continue;
        }

        let token = hooked_get_window_option(window_target, token_key).unwrap_or_default();
        if !token.is_empty() {
            if win_dbg.observed_token.as_deref() != Some(token.as_str()) {
                hooked_notify_debug_emit(
                    workspace,
                    "token.present",
                    &[
                        ("team", Value::from(team_name)),
                        ("window", Value::from(window_target.clone())),
                        ("token", Value::from(token.clone())),
                        (
                            "state_before",
                            notify_state_value(record.notified, record.seen_since_fire),
                        ),
                    ],
                );
                win_dbg.observed_token = Some(token.clone());
            }
            record.notified = true;
            record.seen_since_fire = false;
            continue;
        }

        if let Some(prev_token) = win_dbg.observed_token.take() {
            hooked_notify_debug_emit(
                workspace,
                "token.cleared_externally",
                &[
                    ("team", Value::from(team_name)),
                    ("window", Value::from(window_target.clone())),
                    ("prev_token", Value::from(prev_token)),
                    ("state_before", record_state_value(record)),
                ],
            );
        }

        let inactive_age = debug_state
            .inactive_at
            .get(window_target)
            .map(|inactive_at_ts| now - inactive_at_ts);
        let busy_panes: Vec<String> = panes
            .iter()
            .filter(|p| _is_output_busy(p, busy_monitor, inactive_age))
            .cloned()
            .collect();
        let prev_busy = win_dbg.busy_observed;
        let is_busy = !busy_panes.is_empty();
        if is_busy {
            record.last_busy_ts = now;
            let recent = _most_recent_output_pane(&busy_panes, busy_monitor);
            record.last_busy_pane = Some(if recent.is_empty() {
                busy_panes[busy_panes.len() - 1].clone()
            } else {
                recent
            });
            if prev_busy != is_busy {
                hooked_notify_debug_emit(
                    workspace,
                    "busy.transition",
                    &[
                        ("team", Value::from(team_name)),
                        ("window", Value::from(window_target.clone())),
                        ("busy", Value::Bool(true)),
                        (
                            "busy_panes",
                            Value::Array(busy_panes.iter().cloned().map(Value::from).collect()),
                        ),
                        (
                            "last_busy_pane",
                            record
                                .last_busy_pane
                                .clone()
                                .map(Value::from)
                                .unwrap_or(Value::Null),
                        ),
                    ],
                );
            }
            if record.seen_since_fire {
                if record.notified {
                    hooked_notify_debug_emit(
                        workspace,
                        "busy.rearm",
                        &[
                            ("team", Value::from(team_name)),
                            ("window", Value::from(window_target.clone())),
                            ("seen_since_fire", Value::Bool(true)),
                        ],
                    );
                }
                record.notified = false;
            }
            win_dbg.busy_observed = true;
            continue;
        }

        if prev_busy != is_busy {
            hooked_notify_debug_emit(
                workspace,
                "busy.transition",
                &[
                    ("team", Value::from(team_name)),
                    ("window", Value::from(window_target.clone())),
                    ("busy", Value::Bool(false)),
                    ("last_busy_ts", Value::from(record.last_busy_ts)),
                ],
            );
        }
        win_dbg.busy_observed = false;

        if now - record.last_busy_ts >= IDLE_NOTIFY_THRESHOLD_SECONDS && !record.notified {
            let target_pane = _idle_notify_target_pane(&panes, record, busy_monitor);
            hooked_notify_debug_emit(
                workspace,
                "fire.attempt",
                &[
                    ("team", Value::from(team_name)),
                    ("window", Value::from(window_target.clone())),
                    ("target_pane", Value::from(target_pane.clone())),
                    ("idle_seconds", Value::from(now - record.last_busy_ts)),
                    (
                        "state_before",
                        notify_state_value(record.notified, record.seen_since_fire),
                    ),
                ],
            );
            let (suppressed, surface) =
                hooked_notify_ui_notify(IDLE_NOTIFY_MESSAGE, &target_pane, workspace);
            record.notified = true;
            record.seen_since_fire = suppressed;
            let new_token = hooked_get_window_option(window_target, token_key).unwrap_or_default();
            win_dbg.observed_token = if new_token.is_empty() {
                None
            } else {
                Some(new_token.clone())
            };
            hooked_notify_debug_emit(
                workspace,
                "fire.result",
                &[
                    ("team", Value::from(team_name)),
                    ("window", Value::from(window_target.clone())),
                    ("target_pane", Value::from(target_pane)),
                    ("surface", surface.map(Value::from).unwrap_or(Value::Null)),
                    ("suppressed", Value::Bool(suppressed)),
                    (
                        "token_after",
                        if new_token.is_empty() {
                            Value::Null
                        } else {
                            Value::from(new_token)
                        },
                    ),
                    (
                        "state_after",
                        notify_state_value(record.notified, record.seen_since_fire),
                    ),
                ],
            );
        }
    }

    if now - debug_state.last_heartbeat >= NOTIFY_DEBUG_HEARTBEAT_SECONDS {
        hooked_notify_debug_emit(
            workspace,
            "tick.summary",
            &[
                ("team", Value::from(team_name)),
                ("tick_seq", Value::from(debug_state.tick_seq)),
                (
                    "active_window",
                    if active_window.is_empty() {
                        Value::Null
                    } else {
                        Value::from(active_window.clone())
                    },
                ),
                (
                    "windows",
                    Value::Array(new_keys.into_iter().map(Value::from).collect()),
                ),
                ("records", Value::from(idle_notify.len())),
            ],
        );
        debug_state.last_heartbeat = now;
    }
}

// --------------------------------------------------------------------------
// thread / send / doctor payloads
// --------------------------------------------------------------------------

pub fn _thread_payload(workspace: &str, message_id: &str) -> Result<Map<String, Value>> {
    let events = bus::read_events_with_ns(workspace)?;
    let mut send_events: HashMap<String, (i64, Map<String, Value>)> = HashMap::new();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();

    for (seq, event) in events {
        let event_map = match serde_json::to_value(&event) {
            Ok(Value::Object(map)) => map,
            _ => continue,
        };
        let event_msg_id = event.msg_id.clone();
        if event_msg_id.is_empty() {
            continue;
        }
        if event.intent == "send" {
            let parent = event.in_reply_to.clone();
            send_events.insert(event_msg_id.clone(), (seq, event_map));
            if !parent.is_empty() {
                children.entry(parent).or_default().push(event_msg_id);
            }
        }
    }

    if !send_events.contains_key(message_id) {
        return Ok(err_response(format!(
            "no send event found with msgId '{message_id}'"
        )));
    }

    let mut root_id = message_id.to_string();
    let mut seen: HashSet<String> = HashSet::new();
    loop {
        let (_, event) = &send_events[&root_id];
        let parent = map_get_str(event, "inReplyTo");
        if parent.is_empty() || !send_events.contains_key(&parent) || seen.contains(&parent) {
            break;
        }
        seen.insert(root_id.clone());
        root_id = parent;
    }

    let mut depth_map: HashMap<String, i64> = HashMap::new();
    let mut thread_ids: HashSet<String> = HashSet::new();

    fn walk(
        current_id: &str,
        depth: i64,
        thread_ids: &mut HashSet<String>,
        depth_map: &mut HashMap<String, i64>,
        children: &HashMap<String, Vec<String>>,
        send_events: &HashMap<String, (i64, Map<String, Value>)>,
    ) {
        if thread_ids.contains(current_id) {
            return;
        }
        thread_ids.insert(current_id.to_string());
        depth_map.insert(current_id.to_string(), depth);
        let mut child_ids = children.get(current_id).cloned().unwrap_or_default();
        child_ids.sort_by_key(|item| send_events[item].0);
        for child_id in child_ids {
            walk(
                &child_id,
                depth + 1,
                thread_ids,
                depth_map,
                children,
                send_events,
            );
        }
    }

    walk(
        &root_id,
        0,
        &mut thread_ids,
        &mut depth_map,
        &children,
        &send_events,
    );

    let mut sorted_ids: Vec<String> = thread_ids.into_iter().collect();
    sorted_ids.sort_by_key(|item| send_events[item].0);
    let mut items: Vec<Value> = Vec::new();
    for thread_msg_id in sorted_ids {
        let (_, event) = &send_events[&thread_msg_id];
        let mut item = project_thread_event(event);
        item.insert(
            "depth".to_string(),
            Value::from(depth_map.get(&thread_msg_id).copied().unwrap_or(0)),
        );
        if thread_msg_id == message_id {
            item.insert("focus".to_string(), Value::Bool(true));
        }
        items.push(Value::Object(item));
    }

    let mut payload = Map::new();
    payload.insert("ok".to_string(), Value::Bool(true));
    payload.insert("rootMsgId".to_string(), Value::from(root_id));
    payload.insert("focusMsgId".to_string(), Value::from(message_id));
    payload.insert("messages".to_string(), Value::Array(items));
    Ok(payload)
}

pub fn _resolve_live_agent_impl(team_name: &str, agent_name: &str) -> Result<(Team, Agent)> {
    let team = hooked_team_load(team_name)?;
    let agent = team.get(agent_name)?;
    if !hooked_agent_is_alive(&agent) {
        bail!("agent '{agent_name}' is not alive");
    }
    Ok((team, agent))
}

/// Raise when the target agent is waiting on its human.
///
/// Reads the member's runtime (native daemon / registry state for
/// codex, grok and claude; transcript gate for unmanaged panes) instead of
/// re-deriving it — one judgement for every CLI, and no silent skip when a
/// transcript cannot be resolved.
pub fn _check_send_gate_impl(target: &Agent) -> Result<()> {
    let runtime = if !target.pane_id.is_empty() {
        hooked_member_runtime_payload(&target.pane_id, "agent")
    } else {
        _headless_member_runtime(target)
    };
    if runtime.get("inputState").and_then(Value::as_str) != Some("waiting_user") {
        return Ok(());
    }
    let reason = map_get_str(&runtime, "inputReason");
    if _SEND_GATE_WAIVED_REASONS.contains(&reason.as_str()) {
        return Ok(());
    }
    bail!("target agent is waiting for a user answer; answer it in the target pane")
}

#[allow(clippy::too_many_arguments)]
pub fn _send_payload(
    workspace: &str,
    team_name: &str,
    sender_agent: &str,
    _sender_pane: &str,
    target_agent: &str,
    body: &str,
    artifact: &str,
    reply_to: &str,
) -> Result<Map<String, Value>> {
    if target_agent == FLOW_MAILBOX_AGENT {
        // The flow runner's mailbox: it owns no pane and no transport —
        // the durable bus row IS the delivery, and the runner polls for
        // it. Members answer a flow dispatch with an ordinary
        // `hive send flow`, which lands here.
        let event = bus::write_send_event(
            workspace,
            sender_agent,
            target_agent,
            body.trim(),
            artifact,
            None,
            reply_to,
        )?;
        let mut payload = Map::new();
        payload.insert("ok".to_string(), Value::Bool(true));
        payload.insert("to".to_string(), Value::from(target_agent));
        payload.insert("msgId".to_string(), Value::from(event.msg_id));
        payload.insert("mailbox".to_string(), Value::Bool(true));
        return Ok(payload);
    }

    let (_team, target) = hooked_resolve_live_agent(team_name, target_agent)?;
    let normalized_body = body.trim();

    // Side effect only: errors if target is waiting for a user answer.
    hooked_check_send_gate(&target)?;

    let event = bus::write_send_event(
        workspace,
        sender_agent,
        target_agent,
        normalized_body,
        artifact,
        None,
        reply_to,
    )?;
    let message_id = event.msg_id;
    let envelope = format_hive_envelope(
        sender_agent,
        target_agent,
        body,
        artifact,
        &message_id,
        reply_to,
    );

    let mut payload = Map::new();
    payload.insert("ok".to_string(), Value::Bool(true));
    payload.insert("to".to_string(), Value::from(target_agent));
    payload.insert("msgId".to_string(), Value::from(message_id.clone()));
    // Fire-and-forget past this point: the transport verdict is the only
    // delivery state. The daemon/channel either accepted the message (its
    // own contract queues and processes it) or refused it — there is no
    // tracked in-between, no confirmation oracle, and nothing to poll. A
    // claude member mid-turn queues the message itself (`priority: next`
    // folds it in at the next tool boundary) — no hived hold on top.
    if let Err(exc) = hooked_agent_send(&target, &envelope) {
        let mut refused = Map::new();
        refused.insert("ok".to_string(), Value::Bool(false));
        refused.insert(
            "error".to_string(),
            Value::from(format!("transport refused {target_agent}: {exc}")),
        );
        refused.insert("msgId".to_string(), Value::from(message_id));
        return Ok(refused);
    }

    if !artifact.is_empty() {
        payload.insert("artifact".to_string(), Value::from(artifact));
    }
    Ok(payload)
}

pub fn _doctor_payload(
    workspace: &str,
    team_name: &str,
    target_agent: &str,
    verbose: bool,
    hived: Option<&Map<String, Value>>,
) -> Result<Map<String, Value>> {
    let team = hooked_team_load(team_name)?;
    let target = team.get(target_agent)?;

    let alive = hooked_agent_is_alive(&target);
    let mut diag = Map::new();
    diag.insert("ok".to_string(), Value::Bool(true));
    diag.insert("agent".to_string(), Value::from(target_agent));
    diag.insert("team".to_string(), Value::from(team.name.clone()));
    if let Some(hived) = hived {
        if !hived.is_empty() {
            diag.insert("hived".to_string(), Value::Object(hived.clone()));
        }
    }
    let runtime = hooked_member_runtime_payload(&target.pane_id, "agent");
    diag.insert(
        "alive".to_string(),
        Value::Bool(
            runtime
                .get("alive")
                .and_then(Value::as_bool)
                .unwrap_or(alive),
        ),
    );
    if let Some(cli_alive) = runtime.get("cliAlive") {
        diag.insert(
            "cliAlive".to_string(),
            Value::Bool(cli_alive.as_bool().unwrap_or(false)),
        );
    }
    for key in ["model", "sessionId", "inputState"] {
        let value = map_get_str(&runtime, key);
        if !value.is_empty() {
            diag.insert(key.to_string(), Value::from(value));
        }
    }
    if let Some(busy) = runtime.get("busy") {
        diag.insert(
            "busy".to_string(),
            Value::Bool(busy.as_bool().unwrap_or(false)),
        );
    }
    let turn_phase = map_get_str(&runtime, "turnPhase");
    if !turn_phase.is_empty() {
        diag.insert("turnPhase".to_string(), Value::from(turn_phase));
    }
    if verbose {
        diag.insert("pane".to_string(), Value::from(target.pane_id.clone()));
        diag.insert("teamMembers".to_string(), Value::from(team.agents.len()));
        let cli = map_get_str(&runtime, "_cli");
        if !cli.is_empty() {
            diag.insert("cli".to_string(), Value::from(cli.clone()));
        }
        if cli == "codex" {
            let mut codex = Map::new();
            codex.insert(
                "socket".to_string(),
                Value::from(
                    hooked_cas_shared_socket_path()
                        .to_string_lossy()
                        .to_string(),
                ),
            );
            codex.insert("alive".to_string(), Value::Bool(hooked_cas_daemon_alive()));
            codex.insert(
                "threadId".to_string(),
                match hooked_cas_thread_id_for_pane(&target.pane_id) {
                    Some(tid) => Value::from(tid),
                    None => Value::Null,
                },
            );
            diag.insert("codexDaemon".to_string(), Value::Object(codex));
        }
        if cli == "claude" {
            let job_id = hooked_cb_job_id_for_pane(&target.pane_id).unwrap_or_default();
            if !job_id.is_empty() {
                let mut job = Map::new();
                job.insert("jobId".to_string(), Value::from(job_id.clone()));
                job.insert(
                    "engineAlive".to_string(),
                    Value::Bool(hooked_cb_engine_session_for_job(&job_id).is_some()),
                );
                diag.insert("claudeJob".to_string(), Value::Object(job));
            }
            if runtime.contains_key("_viewKind") {
                // What the pane's viewer is showing right now — the member's
                // own job, another session, or the panel list.
                let mut view = Map::new();
                view.insert(
                    "kind".to_string(),
                    Value::from(map_get_str(&runtime, "_viewKind")),
                );
                view.insert(
                    "certainty".to_string(),
                    Value::from(map_get_str(&runtime, "_viewCertainty")),
                );
                view.insert(
                    "jobId".to_string(),
                    Value::from(map_get_str(&runtime, "_viewedJob")),
                );
                view.insert(
                    "member".to_string(),
                    Value::from(map_get_str(&runtime, "_viewedMember")),
                );
                view.insert(
                    "onMember".to_string(),
                    Value::Bool(
                        !job_id.is_empty() && map_get_str(&runtime, "_viewedJob") == job_id,
                    ),
                );
                diag.insert("claudeView".to_string(), Value::Object(view));
            }
        }
        if let Some(engine_state) = runtime.get("_engineState") {
            diag.insert("engineState".to_string(), engine_state.clone());
        }
        if let Some(input_reason) = runtime.get("inputReason") {
            diag.insert("inputReason".to_string(), input_reason.clone());
        }
        if let Some(transcript) = runtime.get("_transcript") {
            diag.insert("transcript".to_string(), transcript.clone());
        }
        if let Some(exists) = runtime.get("_transcriptExists") {
            diag.insert("transcriptExists".to_string(), exists.clone());
        }
        if let Some(size) = runtime.get("_transcriptSize") {
            diag.insert("transcriptSize".to_string(), size.clone());
        }
        if let Some(gate_reason) = runtime.get("_gateReason") {
            diag.insert("gateReason".to_string(), gate_reason.clone());
        }
        let phase_observed = map_get_str(&runtime, "phaseObservedAt");
        if !phase_observed.is_empty() {
            diag.insert("phaseObservedAt".to_string(), Value::from(phase_observed));
        }
        if let Some(evidence) = runtime.get("_safetyEvidence") {
            diag.insert("safetyEvidence".to_string(), evidence.clone());
        }
        diag.insert("workspace".to_string(), Value::from(workspace));
        diag.insert(
            "runDir".to_string(),
            Value::from(
                devlog::run_dir(Path::new(workspace))
                    .to_string_lossy()
                    .to_string(),
            ),
        );
        diag.insert(
            "logs".to_string(),
            Value::Object(devlog::log_paths(Path::new(workspace))),
        );
        diag.insert(
            "eventCount".to_string(),
            Value::from(bus::count_events(workspace)?),
        );
    }
    Ok(diag)
}

// --------------------------------------------------------------------------
// supervisors
// --------------------------------------------------------------------------

/// Reap grok leader daemons that nothing owns any more.
///
/// Two lifecycles, told apart by key shape:
///
/// - ``m-<team>.<member>`` — registry-driven: the engine belongs to a team
///   member, so a dead pane means nothing (the display closed). Reap only
///   when the team's registry file is *valid and lists no such member*
///   (kill/delete removed it), or the file is *missing entirely* (the team
///   was deleted/archived). An unreadable entry is never grounds to kill a
///   daemon, and a young pidfile gets a grace window so a spawn's
///   registration in flight cannot be raced.
/// - ``p<slug>`` — a raw ``hive grok`` pane outside any team keeps the old
///   pane lifecycle: pane gone, daemon reaped.
///
/// Killing a leader takes its attached TUI down with it, so every reap is
/// logged; ``is_pane_alive`` only reports dead panes from a successful tmux
/// listing, never from a transient tmux failure.
pub fn _cleanup_dead_daemons(workspace: &str) {
    for key in hooked_gl_list_daemon_keys() {
        let binding = crate::adapters::grok_leader::member_from_key(&key);
        match binding {
            None => {
                let slug = &key[1.min(key.len())..];
                if slug.is_empty() || !slug.chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                let pane = format!("%{slug}");
                if hooked_is_pane_alive(&pane) {
                    continue;
                }
            }
            Some((team, member)) => {
                let Some(path) = crate::registry::entry_path(&team) else {
                    continue;
                };
                if path.is_file() {
                    let Some(entry) = crate::registry::load(&team) else {
                        continue; // unreadable is not proof of absence
                    };
                    let listed = entry
                        .get("members")
                        .and_then(Value::as_array)
                        .map(|members| {
                            members.iter().any(|m| {
                                m.get("name").and_then(Value::as_str) == Some(member.as_str())
                            })
                        })
                        .unwrap_or(false);
                    if listed {
                        continue;
                    }
                }
                // Missing registry file, or a valid roster without this
                // member: the engine is an orphan — but never a newborn one.
                let pidfile = hooked_gl_socket_path_for_key(&key).with_extension("pid");
                let Ok(metadata) = fs::metadata(&pidfile) else {
                    continue; // no pidfile yet: daemon mid-start
                };
                let Ok(mtime) = metadata.modified() else {
                    continue;
                };
                let age = std::time::SystemTime::now()
                    .duration_since(mtime)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                if age < _GROK_REAP_GRACE_SECONDS {
                    continue;
                }
            }
        }
        hooked_notify_debug_emit(
            workspace,
            "daemon.reap",
            &[("key", Value::from(key.clone()))],
        );
        // Drop the pool's client BEFORE killing the daemon: a grok stdio
        // client that outlives its leader auto-spawns a replacement on the
        // same socket, resurrecting an orphan mid-reap.
        hooked_gl_pool_drop_key(&key);
        hooked_gl_kill_daemon_key(&key);
    }
}

/// Keep this team's codex members riding the shared daemon.
pub fn _codex_supervisor_tick(workspace: &str, team: &str) {
    let panes = hooked_list_panes_all();
    let live_panes: HashSet<String> = panes.iter().map(|p| p.pane_id.clone()).collect();
    if !panes.is_empty() {
        for pane in hooked_cas_list_recorded_panes() {
            if !live_panes.contains(&pane) {
                hooked_cas_clear_pane_thread(&pane);
                codex_reattach_at()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&pane);
            }
        }
    }

    let Ok(t) = hooked_team_load(team) else {
        return;
    };
    let members: Vec<&Agent> = t
        .agents
        .iter()
        .filter(|a| a.cli == "codex" && live_panes.contains(&a.pane_id))
        .collect();
    if members.is_empty() {
        return;
    }

    if !hooked_cas_daemon_alive() {
        hooked_cas_drop_client();
        let respawned = hooked_cas_spawn_daemon();
        hooked_notify_debug_emit(
            workspace,
            "codex.daemon.respawn",
            &[("ok", Value::Bool(respawned))],
        );
        if !respawned {
            return;
        }
    }

    let now = monotonic();
    for agent in members {
        let Some(thread_id) = hooked_cas_thread_id_for_pane(&agent.pane_id) else {
            continue;
        };
        if thread_id.is_empty() {
            continue;
        }
        if hooked_detect_cli_process_for_pane(&agent.pane_id).is_some() {
            continue; // CLI (codex or another agent) is on the TTY — leave it
        }
        let last = codex_reattach_at()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&agent.pane_id)
            .copied()
            .unwrap_or(f64::NEG_INFINITY);
        if now - last < _CODEX_REATTACH_COOLDOWN_SECONDS {
            continue;
        }
        let command =
            hooked_display_value(&agent.pane_id, "#{pane_current_command}").unwrap_or_default();
        if !crate::agent_cli::is_shell_command(&command) {
            continue; // not at a shell prompt (vim, ssh, …): never type into it
        }
        codex_reattach_at()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(agent.pane_id.clone(), now);
        hooked_notify_debug_emit(
            workspace,
            "codex.member.reattach",
            &[
                ("pane", Value::from(agent.pane_id.clone())),
                ("agent", Value::from(agent.name.clone())),
                ("thread", Value::from(thread_id.clone())),
            ],
        );
        hooked_send_keys(&agent.pane_id, &format!("hive codex resume {thread_id}"));
    }
}

/// Prune claude pane job records whose pane died; park the orphans.
///
/// Records are machine-level (like codex's thread records), so staleness
/// must never rebind a recycled pane id to a foreign job. A record whose
/// pane is gone also means nobody is watching that engine any more:
/// ``claude stop`` parks it — the job stays in the ledger and ``hive
/// resume`` can still wake it, so nothing is lost, but no orphan engine
/// keeps burning in the background.
///
/// No respawn/reattach half: the engine's life is claude's own supervisor's
/// business (wake happens on demand at delivery), and the pane viewer
/// self-heals through the managed launcher's attach loop — a user who
/// deliberately left the loop must not be typed at.
pub fn _claude_supervisor_tick(workspace: &str) {
    let panes = hooked_list_panes_all();
    if panes.is_empty() {
        return; // an empty listing is a tmux failure, not an empty server
    }
    let live_panes: HashSet<String> = panes.iter().map(|p| p.pane_id.clone()).collect();
    for pane in hooked_cb_list_recorded_panes() {
        if live_panes.contains(&pane) {
            continue;
        }
        let record = hooked_cb_read_pane_job(&pane);
        hooked_cb_clear_pane_job(&pane);
        if let Some((job_id, _sid, _cwd)) = record {
            hooked_notify_debug_emit(
                workspace,
                "claude.job.park",
                &[
                    ("pane", Value::from(pane.clone())),
                    ("job", Value::from(job_id.clone())),
                ],
            );
            hooked_cb_stop_job(&job_id);
        }
    }
}

/// Shared per-loop state for the claude name/view ticks (the Python
/// `claude_view_state` dict).
#[derive(Debug, Default)]
pub struct ClaudeTickState {
    pub named: HashSet<String>,
    #[allow(clippy::type_complexity)]
    pub signature: Option<(Vec<String>, Vec<(String, String)>)>,
    pub labels: HashMap<String, String>,
}

/// Keep each claude member's job labelled `<team>.<member>`.
///
/// A member spawned by hive is minted under that name already; one adopted
/// from a pane that was running claude first (init, spawn, resume) was minted
/// before the pane carried any tag, so its job keeps a `hive-<pane>`
/// placeholder. The engine's registry entry — read anyway on every tick —
/// carries the current label, so the comparison is free and the rename fires
/// at most once per job.
///
/// The rename is one control frame, but its confirmation polls the registry
/// for up to a few seconds, so it goes to a thread: identity repair must not
/// stall delivery.
pub fn _claude_name_tick(
    members: &[(String, Map<String, Value>)],
    team: &str,
    state: &mut ClaudeTickState,
) {
    let mut sorted: Vec<&(String, Map<String, Value>)> = members.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (member, binding) in sorted {
        if binding.get("cli").and_then(Value::as_str) != Some("claude") {
            continue;
        }
        let pane = map_get_str(binding, "pane");
        let job_id = hooked_cb_job_id_for_pane(&pane).unwrap_or_default();
        let want = format!("{team}.{member}");
        if job_id.is_empty() || state.named.contains(&job_id) {
            continue;
        }
        let Some(engine) = hooked_cb_engine_session_for_job(&job_id) else {
            continue; // asleep or gone: retry on a later tick
        };
        state.named.insert(job_id.clone());
        if engine.name == want {
            continue;
        }
        hooked_ensure_job_named_thread(&job_id, &want);
    }
}

/// Follow the human's attach-panel switches on this team's claude panes.
///
/// A member pane is an attach viewer: pressing the panel key inside it opens
/// any other bg session, and the pane keeps its member tags while the screen
/// shows something else. Each pane's ``@hive-view`` tag carries what is
/// really on screen (empty while it shows its own member) and the border
/// renders it; a switch onto *another* hive member is also logged, which is
/// what a whole-window follow would key on later.
///
/// Two cheap signals gate the work: the attach journal's entry set (an entry
/// appears/disappears on every attach, switch and detach) and the panes'
/// titles (the panel writes the viewed session's name). Probing costs a ps
/// per pane, so it only runs when one of those changed.
pub fn _claude_view_tick(
    workspace: &str,
    team: &str,
    members: &[(String, Map<String, Value>)],
    state: &mut ClaudeTickState,
) {
    let panes = hooked_list_panes_all();
    if panes.is_empty() {
        return; // an empty listing is a tmux failure, not an empty server
    }
    let titles: HashMap<String, String> = panes
        .iter()
        .filter(|p| p.cli == "claude")
        .map(|p| (p.pane_id.clone(), p.title.clone()))
        .collect();
    let mut sorted_titles: Vec<(String, String)> =
        titles.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    sorted_titles.sort();
    let signature = (hooked_cv_journal_signature(), sorted_titles);
    if state.signature.as_ref() == Some(&signature) {
        return;
    }
    state.signature = Some(signature);

    let mut sorted: Vec<&(String, Map<String, Value>)> = members.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, binding) in sorted {
        let pane_id = map_get_str(binding, "pane");
        if binding.get("cli").and_then(Value::as_str) != Some("claude")
            || !titles.contains_key(&pane_id)
        {
            continue;
        }
        let own_job = hooked_cb_job_id_for_pane(&pane_id).unwrap_or_default();
        let view = hooked_cv_view_for_pane(&pane_id, Some(panes.as_slice()));
        let label = crate::adapters::claude_view::view_label(&view, &own_job);
        if state.labels.get(&pane_id) == Some(&label) {
            continue;
        }
        state.labels.insert(pane_id.clone(), label.clone());
        hooked_set_pane_option(&pane_id, "hive-view", &label);
        if view.kind == "member_view" && view.job_id != own_job {
            let other_team = view.member.split('.').next().unwrap_or("") != team;
            hooked_notify_debug_emit(
                workspace,
                "claude.view.foreign_member",
                &[
                    ("team", Value::from(team)),
                    ("member", Value::from(name.clone())),
                    ("pane", Value::from(pane_id.clone())),
                    ("viewing", Value::from(view.member.clone())),
                    ("viewingJob", Value::from(view.job_id.clone())),
                    ("otherTeam", Value::Bool(other_team)),
                    ("certainty", Value::from(view.certainty.clone())),
                ],
            );
        }
    }
}

/// Backfill the team's registry entry from live observation.
///
/// Refreshes fields of members the registry already knows (model switch,
/// cwd change, a sessionId learned late) and the display cache. It never
/// adds or removes a roster name — membership belongs to the CLI writers,
/// and the whole read-merge-write runs under the store lock so an
/// observation racing a `hive kill` cannot resurrect the killed member.
pub fn _write_registry_backfill(workspace: &str, team: &str) {
    let Ok(t) = hooked_team_load(team) else {
        return;
    };
    if t.name.is_empty() || t.agents.is_empty() {
        return;
    }
    let mut observed: Vec<Map<String, Value>> = Vec::new();
    let mut sorted_agents: Vec<&Agent> = t.agents.iter().collect();
    sorted_agents.sort_by(|a, b| a.name.cmp(&b.name));
    for agent in sorted_agents {
        if agent.pane_id.is_empty() {
            continue; // registry-only member: nothing on screen to observe
        }
        let mut session_id = hooked_fresh_snapshot_session_id(&agent.pane_id, None);
        if session_id.is_empty() {
            session_id = agent.session_id.clone().unwrap_or_default();
        }
        if session_id.is_empty() && agent.cli == "grok" {
            // Daemon-family runtimes never reach the transcript-probe path
            // that feeds runtime snapshots, so a grok member's session id
            // must come straight from its leader record.
            session_id = hooked_gl_session_id_for_pane(&agent.pane_id).unwrap_or_default();
        }
        let model = hooked_resolve_model_for_pane(&agent.pane_id, &agent.cli, "");
        let mut row = Map::new();
        row.insert("name".to_string(), Value::from(agent.name.clone()));
        row.insert("cli".to_string(), Value::from(agent.cli.clone()));
        row.insert(
            "model".to_string(),
            Value::from(if model.is_empty() {
                agent.model.clone()
            } else {
                model
            }),
        );
        row.insert("sessionId".to_string(), Value::from(session_id));
        row.insert("cwd".to_string(), Value::from(agent.cwd.clone()));
        observed.push(row);
    }

    let _ = crate::registry::backfill(
        &t.name,
        &observed,
        &py_float_str(t.created_at),
        &t.tmux_window_id,
        workspace,
    );
}

// --------------------------------------------------------------------------
// lifecycle
// --------------------------------------------------------------------------

pub fn _is_tmux_window_alive_impl(tmux_window_id: &str) -> bool {
    crate::tmux::window_exists(tmux_window_id)
}

/// Ensure the team hived socket is alive.
pub fn ensure_hived(
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
) -> Option<i32> {
    let lock_path = _lock_path(workspace);
    if let Some(parent) = lock_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let cpath = CString::new(lock_path.as_os_str().as_bytes()).ok()?;
    let lock_fd = unsafe { libc::open(cpath.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644) };
    if lock_fd < 0 {
        return None;
    }
    unsafe { libc::flock(lock_fd, libc::LOCK_EX) };
    let result = (|| {
        let response = hooked_request_ping(workspace);
        if _hived_identity_matches(response.as_ref(), team) {
            return None;
        }
        if response.is_some() {
            stop_hived(workspace);
        }
        hooked_cleanup_socket(workspace);
        let pid = _start_hived(workspace, team, tmux_window, tmux_window_id);
        let deadline = monotonic() + SOCKET_READY_TIMEOUT;
        while monotonic() < deadline {
            let response = hooked_request_ping(workspace);
            if _hived_identity_matches(response.as_ref(), team) {
                return pid;
            }
            thread::sleep(Duration::from_secs_f64(SOCKET_RETRY_INTERVAL));
        }
        pid
    })();
    unsafe {
        libc::flock(lock_fd, libc::LOCK_UN);
        libc::close(lock_fd);
    }
    result
}

fn hooked_current_exe() -> String {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.current_exe.clone()).flatten() {
        return f();
    }
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

pub fn _start_hived(
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
) -> Option<i32> {
    let command = _hived_reexec_argv(workspace, team, tmux_window, tmux_window_id);
    let stderr_path = devlog::hived_stderr_path(Path::new(workspace));
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.popen.clone()).flatten() {
        return Some(f(&command, &stderr_path));
    }
    if let Some(parent) = stderr_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let stderr_log = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&stderr_path)
        .ok()?;
    let mut cmd = std::process::Command::new(&command[0]);
    cmd.args(&command[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr_log);
    // Python start_new_session=True → setsid in the child.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().ok()?;
    Some(child.id() as i32)
}

pub fn _run_spawned_hived(argv: &[String]) -> i32 {
    if argv.len() != 5 || argv[0] != "--hived" {
        eprintln!("usage: hive --hived <workspace> <team> <tmux_window> <tmux_window_id>");
        return 1;
    }
    hooked_ignore_sigint();
    hooked_hived_loop(&argv[1], &argv[2], &argv[3], &argv[4]);
    0
}

fn hooked_ignore_sigint() {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.ignore_sigint.clone()).flatten() {
        f();
        return;
    }
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
    }
}

fn hooked_hived_loop(workspace: &str, team: &str, tmux_window: &str, tmux_window_id: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.hived_loop.clone()).flatten() {
        f(workspace, team, tmux_window, tmux_window_id);
        return;
    }
    _hived_loop(workspace, team, tmux_window, tmux_window_id);
}

fn hooked_make_busy_monitor(session_target: &str) -> Option<Arc<dyn OutputMonitor>> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.make_busy_monitor.clone()).flatten() {
        return f(session_target);
    }
    if session_target.is_empty() {
        return None;
    }
    Some(Arc::new(crate::tmux::ControlModeOutputMonitor::new(
        session_target,
    )))
}

pub fn _hived_loop(workspace: &str, team: &str, tmux_window: &str, tmux_window_id: &str) {
    _SHUTDOWN.store(false, Ordering::SeqCst);
    let hived_started_at = _now_iso();
    let mut idle_notify: HashMap<String, IdleRecord> = HashMap::new();
    let mut notify_debug_state = NotifyDebugState::default();
    let mut code_reexec_state = ReexecState::default();
    let mut claude_view_state = ClaudeTickState::default();
    // Python inits these to 0.0 against a large system-uptime monotonic, so
    // every periodic check runs on the first tick; our monotonic starts near
    // zero, so seed them to negative infinity to keep that behavior.
    let mut last_window_check = f64::NEG_INFINITY;
    let mut last_owner_check = f64::NEG_INFINITY;
    let mut last_daemon_cleanup = f64::NEG_INFINITY;
    let owner_token = format!(
        "{}:{}",
        getpid(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    hooked_notify_debug_emit(
        workspace,
        "hived.start",
        &[
            ("team", Value::from(team)),
            ("tmux_window", Value::from(tmux_window)),
            ("tmux_window_id", Value::from(tmux_window_id)),
            ("startedAt", Value::from(hived_started_at.clone())),
        ],
    );
    let mut inherited_reexec_lock_fd = _take_reexec_lock_fd_from_env();
    let Ok(mut server) = hooked_open_server_socket(workspace) else {
        return;
    };
    hooked_write_hived_owner(workspace, getpid(), &hived_started_at, &owner_token);
    hooked_release_reexec_lock_fd(inherited_reexec_lock_fd);
    inherited_reexec_lock_fd = None;
    let session_target = tmux_window
        .split_once(':')
        .map(|(session, _)| session)
        .unwrap_or(tmux_window)
        .trim()
        .to_string();
    let busy_monitor = hooked_make_busy_monitor(&session_target);
    _set_output_busy_monitor(busy_monitor.clone());
    if let Some(monitor) = busy_monitor.as_ref() {
        monitor.start();
    }

    // Python's try/finally around the serve loop.
    loop {
        if !Path::new(workspace).is_dir() {
            break;
        }

        let now = monotonic();
        if now - last_window_check >= 30.0 {
            last_window_check = now;
            // The registry entry is the team's existence; the tmux window
            // is only its display. A dead window no longer retires the
            // hived (engines keep running headless) — a *missing*
            // registry file does (`hive delete` archives it). Corrupt or
            // foreign-instance entries are not "missing": never retire on
            // a read that might be wrong.
            if let Some(path) = crate::registry::entry_path(team) {
                if !path.is_file() && !hooked_is_tmux_window_alive(tmux_window_id) {
                    break;
                }
            }
        }

        if now - last_daemon_cleanup >= 30.0 {
            last_daemon_cleanup = now;
            // Supervision must never take the hived down: every tick below
            // swallows its own errors internally.
            _cleanup_dead_daemons(workspace);
            _codex_supervisor_tick(workspace, team);
            _claude_supervisor_tick(workspace);
            _write_registry_backfill(workspace, team);
        }

        if now - last_owner_check >= HIVED_OWNER_CHECK_SECONDS {
            last_owner_check = now;
            if let Some(foreign_pid) = _foreign_owner_pid(workspace, &owner_token) {
                hooked_notify_debug_emit(
                    workspace,
                    "hived.retire_orphan",
                    &[
                        ("team", Value::from(team)),
                        ("tmux_window", Value::from(tmux_window)),
                        ("tmux_window_id", Value::from(tmux_window_id)),
                        ("currentPid", Value::from(getpid())),
                        ("socketPid", Value::from(foreign_pid)),
                    ],
                );
                break;
            }
        }

        let stale_hash = hooked_stale_disk_build_hash(&mut code_reexec_state, now);
        // Never exec out from under an in-flight request thread: its
        // transport work would die mid-flight with the message already on
        // the bus. The stale hash is still stale 5s later.
        if let Some(stale_hash) = stale_hash.filter(|_| !_requests_in_flight()) {
            let emit_reexec = || {
                hooked_notify_debug_emit(
                    workspace,
                    "hived.reexec",
                    &[
                        ("team", Value::from(team)),
                        ("tmux_window", Value::from(tmux_window)),
                        ("tmux_window_id", Value::from(tmux_window_id)),
                        ("oldHash", Value::from(hived_build_hash())),
                        ("newHash", Value::from(stale_hash.clone())),
                    ],
                );
            };
            if let Some(replacement) = _reexec_hived(
                workspace,
                team,
                tmux_window,
                tmux_window_id,
                server.as_ref(),
                busy_monitor.as_ref(),
                Some(&emit_reexec),
            ) {
                // exec failed: keep serving the old build on the rebound
                // socket instead of dying with the socket torn down.
                server = replacement;
            }
        }

        let tick_members = hooked_team_member_bindings(team).unwrap_or_default();

        // Border cosmetics must never take the hived down (the tick fns
        // swallow their own failures).
        _claude_name_tick(&tick_members, team, &mut claude_view_state);
        _claude_view_tick(workspace, team, &tick_members, &mut claude_view_state);

        if !hooked_serve_requests(
            server.as_ref(),
            workspace,
            team,
            tmux_window,
            tmux_window_id,
            &hived_started_at,
            IDLE_NOTIFY_TICK_SECONDS,
        ) {
            break;
        }

        _idle_notify_tick(
            team,
            &session_target,
            &mut idle_notify,
            busy_monitor.as_deref(),
            monotonic(),
            workspace,
            Some(&mut notify_debug_state),
            Some(tick_members.as_slice()),
        );
    }

    // Python `finally`
    hooked_release_reexec_lock_fd(inherited_reexec_lock_fd);
    if let Some(monitor) = busy_monitor.as_ref() {
        monitor.stop();
    }
    _set_output_busy_monitor(None);
    server.close();
    _cleanup_socket_if_owner(workspace, &owner_token);
}

pub fn stop_hived(workspace: &str) {
    let _ = _request_hived(workspace, &action_payload("shutdown"), SOCKET_READY_TIMEOUT);
    let deadline = monotonic() + SOCKET_READY_TIMEOUT;
    while monotonic() < deadline {
        if !_socket_path(workspace).exists() {
            return;
        }
        thread::sleep(Duration::from_secs_f64(SOCKET_RETRY_INTERVAL));
    }
    hooked_cleanup_socket(workspace);
}

// --------------------------------------------------------------------------
// seams (the Rust shape of the Python tests' monkeypatching). Each hooked_*
// consults the process-global test hook, then falls through to the real
// module. The hook is process-global (not thread-local) because hived work
// crosses threads (request handlers); nextest's process-per-test keeps it
// isolated.
// --------------------------------------------------------------------------

#[cfg(test)]
fn hookget<T>(f: impl FnOnce(&testhook::Hook) -> T) -> Option<T> {
    testhook::HOOK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(f)
}

/// Adapter dispatch used by the runtime probes (Python `adapters.get`).
pub enum AdapterHandle {
    Real(Box<dyn crate::adapters::base::SessionAdapter>),
    #[cfg(test)]
    Fake(testhook::FakeAdapter),
}

impl AdapterHandle {
    pub fn resolve_current_session_id(&self, pane_id: &str) -> Option<String> {
        match self {
            AdapterHandle::Real(adapter) => adapter.resolve_current_session_id(pane_id),
            #[cfg(test)]
            AdapterHandle::Fake(fake) => (fake.resolve)(pane_id),
        }
    }

    pub fn find_session_file(&self, session_id: &str, cwd: Option<&str>) -> Option<PathBuf> {
        match self {
            AdapterHandle::Real(adapter) => adapter.find_session_file(session_id, cwd),
            #[cfg(test)]
            AdapterHandle::Fake(fake) => (fake.find)(session_id, cwd),
        }
    }
}

fn hooked_adapters_get(name: &str) -> Option<AdapterHandle> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.adapters_get.clone()).flatten() {
        return f(name);
    }
    let adapter: Box<dyn crate::adapters::base::SessionAdapter> = match name {
        "claude" => Box::new(crate::adapters::claude::ClaudeAdapter),
        "codex" => Box::new(crate::adapters::codex::CodexAdapter),
        "grok" => Box::new(crate::adapters::grok::GrokAdapter),
        _ => return None,
    };
    Some(AdapterHandle::Real(adapter))
}

// --- tmux seams ------------------------------------------------------------

fn hooked_is_pane_alive(pane_id: &str) -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.is_pane_alive.clone()).flatten() {
        return f(pane_id);
    }
    crate::tmux::is_pane_alive(pane_id)
}

fn hooked_display_value(target: &str, fmt: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.display_value.clone()).flatten() {
        return f(target, fmt);
    }
    crate::tmux::display_value(target, fmt)
}

fn hooked_get_most_recent_client_window(session_name: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.get_most_recent_client_window.clone()).flatten() {
        return f(session_name);
    }
    crate::tmux::get_most_recent_client_window(if session_name.is_empty() {
        None
    } else {
        Some(session_name)
    })
}

fn hooked_get_pane_window_target(pane_id: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.get_pane_window_target.clone()).flatten() {
        return f(pane_id);
    }
    crate::tmux::get_pane_window_target(pane_id)
}

fn hooked_get_window_option(target: &str, key: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.get_window_option.clone()).flatten() {
        return f(target, key);
    }
    crate::tmux::get_window_option(target, key)
}

fn hooked_set_pane_option(pane_id: &str, key: &str, value: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.set_pane_option.clone()).flatten() {
        f(pane_id, key, value);
        return;
    }
    crate::tmux::set_pane_option(pane_id, key, value)
}

fn hooked_send_keys(pane_id: &str, text: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.send_keys.clone()).flatten() {
        f(pane_id, text);
        return;
    }
    let _ = crate::tmux::send_keys(pane_id, text, true);
}

fn hooked_list_panes_all() -> Vec<crate::tmux::PaneInfo> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.list_panes_all.clone()).flatten() {
        return f();
    }
    crate::tmux::list_panes_all()
}

fn hooked_is_tmux_window_alive(tmux_window_id: &str) -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.is_tmux_window_alive.clone()).flatten() {
        return f(tmux_window_id);
    }
    _is_tmux_window_alive_impl(tmux_window_id)
}

// --- agent_cli seams -------------------------------------------------------

fn hooked_detect_cli_process_for_pane(
    pane_id: &str,
) -> Option<&'static crate::agent_cli::CLIProfile> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.detect_cli_process_for_pane.clone()).flatten() {
        return f(pane_id);
    }
    crate::agent_cli::detect_cli_process_for_pane(pane_id)
}

fn hooked_detect_profile_for_pane(pane_id: &str) -> Option<&'static crate::agent_cli::CLIProfile> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.detect_profile_for_pane.clone()).flatten() {
        return f(pane_id);
    }
    crate::agent_cli::detect_profile_for_pane(pane_id)
}

fn hooked_claude_pid_for_pane(pane_id: &str) -> Option<i32> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.claude_pid_for_pane.clone()).flatten() {
        return f(pane_id);
    }
    crate::agent_cli::claude_pid_for_pane(pane_id)
}

fn hooked_resolve_model_for_pane(pane_id: &str, cli_name: &str, current_model: &str) -> String {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.resolve_model_for_pane.clone()).flatten() {
        return f(pane_id, cli_name, current_model);
    }
    crate::agent_cli::resolve_model_for_pane(pane_id, cli_name, current_model)
}

fn hooked_member_role_for_pane(pane_id: &str) -> &'static str {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.member_role_for_pane.clone()).flatten() {
        return f(pane_id);
    }
    crate::agent_cli::member_role_for_pane(pane_id)
}

fn hooked_check_input_gate(path: &Path) -> crate::adapters::base::GateResult {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.check_input_gate.clone()).flatten() {
        return f(path);
    }
    crate::adapters::base::check_input_gate(path)
}

// --- claude_bg seams -------------------------------------------------------

fn hooked_cb_read_pane_job(pane: &str) -> Option<(String, String, String)> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cb_read_pane_job.clone()).flatten() {
        return f(pane);
    }
    crate::adapters::claude_bg::read_pane_job(pane)
}

fn hooked_cb_engine_session_for_job(
    job_id: &str,
) -> Option<crate::adapters::claude_bg::EngineSession> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cb_engine_session_for_job.clone()).flatten() {
        return f(job_id);
    }
    crate::adapters::claude_bg::engine_session_for_job(job_id)
}

fn hooked_cb_list_jobs() -> Option<Vec<Map<String, Value>>> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cb_list_jobs.clone()).flatten() {
        return f();
    }
    crate::adapters::claude_bg::list_jobs("claude")
}

fn hooked_cb_job_id_for_pane(pane: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cb_job_id_for_pane.clone()).flatten() {
        return f(pane);
    }
    crate::adapters::claude_bg::job_id_for_pane(pane)
}

fn hooked_cb_list_recorded_panes() -> Vec<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cb_list_recorded_panes.clone()).flatten() {
        return f();
    }
    crate::adapters::claude_bg::list_recorded_panes()
}

fn hooked_cb_clear_pane_job(pane: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cb_clear_pane_job.clone()).flatten() {
        f(pane);
        return;
    }
    crate::adapters::claude_bg::clear_pane_job(pane)
}

fn hooked_cb_stop_job(job_id: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cb_stop_job.clone()).flatten() {
        f(job_id);
        return;
    }
    crate::adapters::claude_bg::stop_job(job_id, "claude")
}

fn hooked_ensure_job_named_thread(job_id: &str, want: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.ensure_job_named.clone()).flatten() {
        f(job_id, want);
        return;
    }
    let job_id = job_id.to_string();
    let want = want.to_string();
    let _ = thread::Builder::new().spawn(move || {
        let _ = crate::adapters::claude_bg::ensure_job_named(&job_id, &want);
    });
}

// --- claude_sessions seams -------------------------------------------------

fn hooked_cs_session_status(pid: Option<i32>) -> Option<(String, String)> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cs_session_status.clone()).flatten() {
        return f(pid);
    }
    crate::adapters::claude_sessions::session_status(pid)
}

fn hooked_cs_list_sessions() -> Vec<crate::adapters::claude_sessions::ClaudeSession> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cs_list_sessions.clone()).flatten() {
        return f();
    }
    crate::adapters::claude_sessions::list_sessions()
}

// --- claude_view seams -----------------------------------------------------

fn hooked_cv_journal_signature() -> Vec<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cv_journal_signature.clone()).flatten() {
        return f();
    }
    crate::adapters::claude_view::journal_signature()
}

fn hooked_cv_view_for_pane(
    pane_id: &str,
    panes: Option<&[crate::tmux::PaneInfo]>,
) -> crate::adapters::claude_view::PaneView {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cv_view_for_pane.clone()).flatten() {
        return f(pane_id);
    }
    crate::adapters::claude_view::view_for_pane(pane_id, panes)
}

// --- codex_app_server seams ------------------------------------------------

fn hooked_cas_runtime_for_pane(
    pane: &str,
) -> Option<crate::adapters::codex_app_server::ThreadRuntime> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_runtime_for_pane.clone()).flatten() {
        return f(pane);
    }
    crate::adapters::codex_app_server::runtime_for_pane(pane)
}

fn hooked_cas_runtime_for_thread(
    thread_id: &str,
) -> Option<crate::adapters::codex_app_server::ThreadRuntime> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_runtime_for_thread.clone()).flatten() {
        return f(thread_id);
    }
    crate::adapters::codex_app_server::runtime_for_thread(thread_id)
}

fn hooked_cas_session_id_for_pane(pane: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_session_id_for_pane.clone()).flatten() {
        return f(pane);
    }
    crate::adapters::codex_app_server::session_id_for_pane(pane)
}

fn hooked_cas_shared_socket_path() -> PathBuf {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_shared_socket_path.clone()).flatten() {
        return f();
    }
    crate::adapters::codex_app_server::shared_socket_path()
}

fn hooked_cas_daemon_alive() -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_daemon_alive.clone()).flatten() {
        return f();
    }
    crate::adapters::codex_app_server::daemon_alive()
}

fn hooked_cas_thread_id_for_pane(pane: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_thread_id_for_pane.clone()).flatten() {
        return f(pane);
    }
    crate::adapters::codex_app_server::thread_id_for_pane(pane)
}

fn hooked_cas_list_recorded_panes() -> Vec<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_list_recorded_panes.clone()).flatten() {
        return f();
    }
    crate::adapters::codex_app_server::list_recorded_panes()
}

fn hooked_cas_clear_pane_thread(pane: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_clear_pane_thread.clone()).flatten() {
        f(pane);
        return;
    }
    let _ = crate::adapters::codex_app_server::clear_pane_thread(pane);
}

fn hooked_cas_drop_client() {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_drop_client.clone()).flatten() {
        f();
        return;
    }
    crate::adapters::codex_app_server::drop_client()
}

fn hooked_cas_spawn_daemon() -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_spawn_daemon.clone()).flatten() {
        return f();
    }
    crate::adapters::codex_app_server::spawn_daemon()
}

fn hooked_cas_connect() -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_connect.clone()).flatten() {
        return f();
    }
    crate::adapters::codex_app_server::connect()
}

// --- grok_leader seams -----------------------------------------------------

fn hooked_gl_runtime_for_pane(pane: &str) -> Option<crate::adapters::grok_leader::SessionRuntime> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_runtime_for_pane.clone()).flatten() {
        return f(pane);
    }
    crate::adapters::grok_leader::runtime_for_pane(pane)
}

fn hooked_gl_runtime_for_key(key: &str) -> Option<crate::adapters::grok_leader::SessionRuntime> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_runtime_for_key.clone()).flatten() {
        return f(key);
    }
    crate::adapters::grok_leader::runtime_for_key(key)
}

fn hooked_gl_session_id_for_pane(pane: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_session_id_for_pane.clone()).flatten() {
        return f(pane);
    }
    crate::adapters::grok_leader::session_id_for_pane(pane)
}

fn hooked_gl_read_session_key(key: &str) -> Option<(String, String)> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_read_session_key.clone()).flatten() {
        return f(key);
    }
    crate::adapters::grok_leader::read_session_key(key)
}

fn hooked_gl_list_daemon_keys() -> Vec<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_list_daemon_keys.clone()).flatten() {
        return f();
    }
    crate::adapters::grok_leader::list_daemon_keys()
}

fn hooked_gl_socket_path_for_key(key: &str) -> PathBuf {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_socket_path_for_key.clone()).flatten() {
        return f(key);
    }
    crate::adapters::grok_leader::socket_path_for_key(key)
}

fn hooked_gl_kill_daemon_key(key: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_kill_daemon_key.clone()).flatten() {
        f(key);
        return;
    }
    crate::adapters::grok_leader::kill_daemon_key(key)
}

fn hooked_gl_pool_drop_key(key: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_pool_drop_key.clone()).flatten() {
        f(key);
        return;
    }
    crate::adapters::grok_leader::pool().drop_key(key)
}

fn hooked_gl_connect_pane(pane: &str) -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_connect_pane.clone()).flatten() {
        return f(pane);
    }
    crate::adapters::grok_leader::connect_pane(pane)
}

// --- notify / plugin seams -------------------------------------------------

fn hooked_notify_debug_emit(workspace: &str, event: &str, fields: &[(&str, Value)]) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.notify_debug_emit.clone()).flatten() {
        f(workspace, event, fields);
        return;
    }
    crate::notify_debug::emit(workspace, event, fields)
}

fn hooked_notify_ui_notify(
    message: &str,
    pane_id: &str,
    workspace: &str,
) -> (bool, Option<String>) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.notify_ui_notify.clone()).flatten() {
        return f(message, pane_id, workspace);
    }
    match crate::notify_ui::notify(message, pane_id, workspace) {
        Ok(payload) => (payload.suppressed, Some(payload.surface)),
        Err(_) => (false, None),
    }
}

#[allow(clippy::too_many_arguments)]
fn hooked_clear_stale_notify(
    window_target: &str,
    panes: &[String],
    token: &str,
    remove_attention: bool,
    source: &str,
    workspace: &str,
) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.clear_stale_notify.clone()).flatten() {
        f(
            window_target,
            panes,
            token,
            remove_attention,
            source,
            workspace,
        );
        return;
    }
    crate::notify_ui::clear_stale_notify(
        window_target,
        panes,
        token,
        remove_attention,
        source,
        workspace,
    )
}

fn hooked_is_plugin_enabled(name: &str) -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.is_plugin_enabled.clone()).flatten() {
        return f(name);
    }
    crate::plugin_manager::is_plugin_enabled(name)
}

// --- team / agent seams ----------------------------------------------------

fn hooked_team_load(name: &str) -> Result<Team> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.team_load.clone()).flatten() {
        return f(name);
    }
    Team::load(name, "")
}

fn hooked_agent_is_alive(agent: &Agent) -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.agent_is_alive.clone()).flatten() {
        return f(agent);
    }
    agent.is_alive()
}

fn hooked_agent_send(agent: &Agent, text: &str) -> std::result::Result<String, DeliveryError> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.agent_send.clone()).flatten() {
        return f(agent, text);
    }
    agent.send(text)
}

// --- self seams (Python monkeypatches on hive.hived itself) ---------------

pub fn _resolve_live_agent(team_name: &str, agent_name: &str) -> Result<(Team, Agent)> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.resolve_live_agent.clone()).flatten() {
        return f(team_name, agent_name);
    }
    _resolve_live_agent_impl(team_name, agent_name)
}

fn hooked_resolve_live_agent(team_name: &str, agent_name: &str) -> Result<(Team, Agent)> {
    _resolve_live_agent(team_name, agent_name)
}

pub fn _check_send_gate(target: &Agent) -> Result<()> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.check_send_gate.clone()).flatten() {
        return f(target);
    }
    _check_send_gate_impl(target)
}

fn hooked_check_send_gate(target: &Agent) -> Result<()> {
    _check_send_gate(target)
}

pub fn _member_runtime_payload(pane_id: &str, role: &str) -> Map<String, Value> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.member_runtime_payload.clone()).flatten() {
        return f(pane_id, role);
    }
    _member_runtime_payload_impl(pane_id, role)
}

fn hooked_member_runtime_payload(pane_id: &str, role: &str) -> Map<String, Value> {
    _member_runtime_payload(pane_id, role)
}

pub fn _busy_output_payload(pane_id: &str) -> Map<String, Value> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.busy_output_payload.clone()).flatten() {
        return f(pane_id);
    }
    _busy_output_payload_impl(pane_id)
}

fn hooked_busy_output_payload(pane_id: &str) -> Map<String, Value> {
    _busy_output_payload(pane_id)
}

pub fn _native_daemon_busy(pane_id: &str) -> Option<bool> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.native_daemon_busy.clone()).flatten() {
        return f(pane_id);
    }
    _native_daemon_busy_impl(pane_id)
}

fn hooked_native_daemon_busy(pane_id: &str) -> Option<bool> {
    _native_daemon_busy(pane_id)
}

pub fn _transcript_progressed_recently(pane_id: &str, threshold_seconds: f64) -> Option<bool> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.transcript_progressed_recently.clone()).flatten() {
        return f(pane_id, threshold_seconds);
    }
    _transcript_progressed_recently_impl(pane_id, threshold_seconds)
}

fn hooked_transcript_progressed_recently(pane_id: &str, threshold_seconds: f64) -> Option<bool> {
    _transcript_progressed_recently(pane_id, threshold_seconds)
}

pub fn _resolve_transcript_path_cached(pane_id: &str, force: bool) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.resolve_transcript_path_cached.clone()).flatten() {
        return f(pane_id, force);
    }
    _resolve_transcript_path_cached_impl(pane_id, force)
}

fn hooked_resolve_transcript_path_cached(pane_id: &str, force: bool) -> Option<String> {
    _resolve_transcript_path_cached(pane_id, force)
}

pub fn _claude_bg_runtime(pane_id: &str) -> Option<Map<String, Value>> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.claude_bg_runtime.clone()).flatten() {
        return f(pane_id);
    }
    _claude_bg_runtime_impl(pane_id)
}

fn hooked_claude_bg_runtime(pane_id: &str) -> Option<Map<String, Value>> {
    _claude_bg_runtime(pane_id)
}

pub fn _codex_app_server_runtime(pane_id: &str) -> Option<Map<String, Value>> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.codex_app_server_runtime.clone()).flatten() {
        return f(pane_id);
    }
    _codex_app_server_runtime_impl(pane_id)
}

fn hooked_codex_app_server_runtime(pane_id: &str) -> Option<Map<String, Value>> {
    _codex_app_server_runtime(pane_id)
}

pub fn _idle_notify_agent_panes(team_name: &str) -> Vec<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.idle_notify_agent_panes.clone()).flatten() {
        return f(team_name);
    }
    _idle_notify_agent_panes_impl(team_name)
}

fn hooked_idle_notify_agent_panes(team_name: &str) -> Vec<String> {
    _idle_notify_agent_panes(team_name)
}

pub fn _team_member_bindings(team_name: &str) -> Result<Vec<(String, Map<String, Value>)>> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.team_member_bindings.clone()).flatten() {
        return f(team_name);
    }
    _team_member_bindings_impl(team_name)
}

fn hooked_team_member_bindings(team_name: &str) -> Result<Vec<(String, Map<String, Value>)>> {
    _team_member_bindings(team_name)
}

pub fn _fresh_snapshot_session_id(pane_id: &str, now: Option<f64>) -> String {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.fresh_snapshot_session_id.clone()).flatten() {
        return f(pane_id);
    }
    _fresh_snapshot_session_id_impl(pane_id, now)
}

fn hooked_fresh_snapshot_session_id(pane_id: &str, now: Option<f64>) -> String {
    _fresh_snapshot_session_id(pane_id, now)
}

pub fn request_ping(workspace: &str) -> Option<Map<String, Value>> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.request_ping.clone()).flatten() {
        return f(workspace);
    }
    request_ping_impl(workspace)
}

fn hooked_request_ping(workspace: &str) -> Option<Map<String, Value>> {
    request_ping(workspace)
}

pub fn _cleanup_socket(workspace: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cleanup_socket.clone()).flatten() {
        f(workspace);
        return;
    }
    _cleanup_socket_impl(workspace)
}

fn hooked_cleanup_socket(workspace: &str) {
    _cleanup_socket(workspace)
}

fn hooked_run_dir(workspace: &str) -> PathBuf {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.run_dir.clone()).flatten() {
        return f(workspace);
    }
    _run_dir_impl(workspace)
}

pub fn _write_hived_owner(workspace: &str, pid: i64, started_at: &str, token: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.write_hived_owner.clone()).flatten() {
        f(workspace, pid, started_at, token);
        return;
    }
    _write_hived_owner_impl(workspace, pid, started_at, token)
}

fn hooked_write_hived_owner(workspace: &str, pid: i64, started_at: &str, token: &str) {
    _write_hived_owner(workspace, pid, started_at, token)
}

pub fn _release_reexec_lock_fd(lock_fd: Option<i32>) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.release_reexec_lock_fd.clone()).flatten() {
        f(lock_fd);
        return;
    }
    _release_reexec_lock_fd_impl(lock_fd)
}

fn hooked_release_reexec_lock_fd(lock_fd: Option<i32>) {
    _release_reexec_lock_fd(lock_fd)
}

pub fn _try_acquire_reexec_lock(workspace: &str) -> Option<i32> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.try_acquire_reexec_lock.clone()).flatten() {
        return f(workspace);
    }
    _try_acquire_reexec_lock_impl(workspace)
}

fn hooked_try_acquire_reexec_lock(workspace: &str) -> Option<i32> {
    _try_acquire_reexec_lock(workspace)
}

fn hooked_execv(argv: &[String]) -> ExecOutcome {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.execv.clone()).flatten() {
        return f(argv);
    }
    _execv_impl(argv)
}

fn hooked_compute_build_hash() -> String {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.compute_build_hash.clone()).flatten() {
        return f();
    }
    _compute_build_hash()
}

fn hooked_stale_disk_build_hash(state: &mut ReexecState, now: f64) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.stale_disk_build_hash.clone()).flatten() {
        return f();
    }
    _stale_disk_build_hash_for_reexec(state, now)
}

#[allow(clippy::too_many_arguments)]
fn hooked_serve_requests(
    server: &dyn HivedServerApi,
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
    hived_started_at: &str,
    timeout: f64,
) -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.serve_requests.clone()).flatten() {
        return f();
    }
    _serve_requests(
        server,
        workspace,
        team,
        tmux_window,
        tmux_window_id,
        hived_started_at,
        timeout,
    )
}

fn hooked_open_server_socket(workspace: &str) -> Result<Box<dyn HivedServerApi>> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.open_server_socket.clone()).flatten() {
        return f(workspace);
    }
    Ok(Box::new(_open_server_socket(workspace)?))
}

// --------------------------------------------------------------------------
// test hook: one process-global environment double, mirroring what the
// Python suite pins with monkeypatch. Closures instead of data so each test
// wires exactly the behavior its Python original monkeypatched.
// --------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod testhook {
    use super::{AdapterHandle, ExecOutcome, HivedServerApi, OutputMonitor};
    use crate::adapters::base::GateResult;
    use crate::adapters::claude_bg::EngineSession;
    use crate::adapters::claude_sessions::ClaudeSession;
    use crate::adapters::claude_view::PaneView;
    use crate::adapters::codex_app_server::ThreadRuntime;
    use crate::adapters::grok_leader::SessionRuntime;
    use crate::agent::{Agent, DeliveryError};
    use crate::team::Team;
    use serde_json::{Map, Value};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    pub type F0<R> = Arc<dyn Fn() -> R + Send + Sync>;
    pub type S1<R> = Arc<dyn Fn(&str) -> R + Send + Sync>;
    pub type S2<R> = Arc<dyn Fn(&str, &str) -> R + Send + Sync>;

    /// The two adapter methods the hived consumes (Python fakes stub only
    /// `resolve_current_session_id` / `find_session_file`).
    #[derive(Clone)]
    pub struct FakeAdapter {
        pub resolve: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
        pub find: Arc<dyn Fn(&str, Option<&str>) -> Option<PathBuf> + Send + Sync>,
    }

    #[derive(Default)]
    pub struct Hook {
        // adapters / gate
        pub adapters_get: Option<S1<Option<AdapterHandle>>>,
        pub check_input_gate: Option<Arc<dyn Fn(&Path) -> GateResult + Send + Sync>>,
        // tmux
        pub is_pane_alive: Option<S1<bool>>,
        pub display_value: Option<S2<Option<String>>>,
        pub get_most_recent_client_window: Option<S1<Option<String>>>,
        pub get_pane_window_target: Option<S1<Option<String>>>,
        pub get_window_option: Option<S2<Option<String>>>,
        pub set_pane_option: Option<Arc<dyn Fn(&str, &str, &str) + Send + Sync>>,
        pub send_keys: Option<Arc<dyn Fn(&str, &str) + Send + Sync>>,
        pub list_panes_all: Option<F0<Vec<crate::tmux::PaneInfo>>>,
        pub is_tmux_window_alive: Option<S1<bool>>,
        // agent_cli
        pub detect_cli_process_for_pane: Option<S1<Option<&'static crate::agent_cli::CLIProfile>>>,
        pub detect_profile_for_pane: Option<S1<Option<&'static crate::agent_cli::CLIProfile>>>,
        pub claude_pid_for_pane: Option<S1<Option<i32>>>,
        pub resolve_model_for_pane: Option<Arc<dyn Fn(&str, &str, &str) -> String + Send + Sync>>,
        pub member_role_for_pane: Option<S1<&'static str>>,
        // claude_bg
        pub cb_read_pane_job: Option<S1<Option<(String, String, String)>>>,
        pub cb_engine_session_for_job: Option<S1<Option<EngineSession>>>,
        pub cb_list_jobs: Option<F0<Option<Vec<Map<String, Value>>>>>,
        pub cb_job_id_for_pane: Option<S1<Option<String>>>,
        pub cb_list_recorded_panes: Option<F0<Vec<String>>>,
        pub cb_clear_pane_job: Option<S1<()>>,
        pub cb_stop_job: Option<S1<()>>,
        pub ensure_job_named: Option<Arc<dyn Fn(&str, &str) + Send + Sync>>,
        // claude_sessions
        pub cs_session_status:
            Option<Arc<dyn Fn(Option<i32>) -> Option<(String, String)> + Send + Sync>>,
        pub cs_list_sessions: Option<F0<Vec<ClaudeSession>>>,
        // claude_view
        pub cv_journal_signature: Option<F0<Vec<String>>>,
        pub cv_view_for_pane: Option<S1<PaneView>>,
        // codex_app_server
        pub cas_runtime_for_pane: Option<S1<Option<ThreadRuntime>>>,
        pub cas_runtime_for_thread: Option<S1<Option<ThreadRuntime>>>,
        pub cas_session_id_for_pane: Option<S1<Option<String>>>,
        pub cas_shared_socket_path: Option<F0<PathBuf>>,
        pub cas_daemon_alive: Option<F0<bool>>,
        pub cas_thread_id_for_pane: Option<S1<Option<String>>>,
        pub cas_list_recorded_panes: Option<F0<Vec<String>>>,
        pub cas_clear_pane_thread: Option<S1<()>>,
        pub cas_drop_client: Option<F0<()>>,
        pub cas_spawn_daemon: Option<F0<bool>>,
        pub cas_connect: Option<F0<bool>>,
        // grok_leader
        pub gl_runtime_for_pane: Option<S1<Option<SessionRuntime>>>,
        pub gl_runtime_for_key: Option<S1<Option<SessionRuntime>>>,
        pub gl_session_id_for_pane: Option<S1<Option<String>>>,
        pub gl_read_session_key: Option<S1<Option<(String, String)>>>,
        pub gl_list_daemon_keys: Option<F0<Vec<String>>>,
        pub gl_socket_path_for_key: Option<S1<PathBuf>>,
        pub gl_kill_daemon_key: Option<S1<()>>,
        pub gl_pool_drop_key: Option<S1<()>>,
        pub gl_connect_pane: Option<S1<bool>>,
        // notify / plugin
        #[allow(clippy::type_complexity)]
        pub notify_debug_emit: Option<Arc<dyn Fn(&str, &str, &[(&str, Value)]) + Send + Sync>>,
        #[allow(clippy::type_complexity)]
        pub notify_ui_notify:
            Option<Arc<dyn Fn(&str, &str, &str) -> (bool, Option<String>) + Send + Sync>>,
        #[allow(clippy::type_complexity)]
        pub clear_stale_notify:
            Option<Arc<dyn Fn(&str, &[String], &str, bool, &str, &str) + Send + Sync>>,
        pub is_plugin_enabled: Option<S1<bool>>,
        // team / agent
        pub team_load: Option<Arc<dyn Fn(&str) -> anyhow::Result<Team> + Send + Sync>>,
        pub agent_is_alive: Option<Arc<dyn Fn(&Agent) -> bool + Send + Sync>>,
        #[allow(clippy::type_complexity)]
        pub agent_send:
            Option<Arc<dyn Fn(&Agent, &str) -> Result<String, DeliveryError> + Send + Sync>>,
        // hived self-seams
        #[allow(clippy::type_complexity)]
        pub resolve_live_agent:
            Option<Arc<dyn Fn(&str, &str) -> anyhow::Result<(Team, Agent)> + Send + Sync>>,
        pub check_send_gate: Option<Arc<dyn Fn(&Agent) -> anyhow::Result<()> + Send + Sync>>,
        pub member_runtime_payload: Option<S2<Map<String, Value>>>,
        pub busy_output_payload: Option<S1<Map<String, Value>>>,
        pub native_daemon_busy: Option<S1<Option<bool>>>,
        #[allow(clippy::type_complexity)]
        pub transcript_progressed_recently:
            Option<Arc<dyn Fn(&str, f64) -> Option<bool> + Send + Sync>>,
        #[allow(clippy::type_complexity)]
        pub resolve_transcript_path_cached:
            Option<Arc<dyn Fn(&str, bool) -> Option<String> + Send + Sync>>,
        pub claude_bg_runtime: Option<S1<Option<Map<String, Value>>>>,
        pub codex_app_server_runtime: Option<S1<Option<Map<String, Value>>>>,
        pub idle_notify_agent_panes: Option<S1<Vec<String>>>,
        #[allow(clippy::type_complexity)]
        pub team_member_bindings: Option<
            Arc<dyn Fn(&str) -> anyhow::Result<Vec<(String, Map<String, Value>)>> + Send + Sync>,
        >,
        pub fresh_snapshot_session_id: Option<S1<String>>,
        // sockets / lifecycle
        pub request_ping: Option<S1<Option<Map<String, Value>>>>,
        pub cleanup_socket: Option<S1<()>>,
        pub run_dir: Option<S1<PathBuf>>,
        pub write_hived_owner: Option<Arc<dyn Fn(&str, i64, &str, &str) + Send + Sync>>,
        pub release_reexec_lock_fd: Option<Arc<dyn Fn(Option<i32>) + Send + Sync>>,
        pub try_acquire_reexec_lock: Option<S1<Option<i32>>>,
        pub execv: Option<Arc<dyn Fn(&[String]) -> ExecOutcome + Send + Sync>>,
        pub compute_build_hash: Option<F0<String>>,
        pub stale_disk_build_hash: Option<F0<Option<String>>>,
        pub serve_requests: Option<F0<bool>>,
        #[allow(clippy::type_complexity)]
        pub open_server_socket:
            Option<Arc<dyn Fn(&str) -> anyhow::Result<Box<dyn HivedServerApi>> + Send + Sync>>,
        #[allow(clippy::type_complexity)]
        pub handle_request:
            Option<Arc<dyn Fn(&Map<String, Value>) -> (Map<String, Value>, bool) + Send + Sync>>,
        pub current_exe: Option<F0<String>>,
        pub popen: Option<Arc<dyn Fn(&[String], &Path) -> i32 + Send + Sync>>,
        pub ignore_sigint: Option<F0<()>>,
        pub hived_loop: Option<Arc<dyn Fn(&str, &str, &str, &str) + Send + Sync>>,
        pub make_busy_monitor: Option<S1<Option<Arc<dyn OutputMonitor>>>>,
    }

    pub static HOOK: Mutex<Option<Hook>> = Mutex::new(None);

    pub struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            *HOOK.lock().unwrap_or_else(|e| e.into_inner()) = None;
            super::_SHUTDOWN.store(false, std::sync::atomic::Ordering::SeqCst);
            super::transcript_path_cache()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            *super::claude_jobs_cache()
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            super::runtime_snapshots()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            super::codex_reattach_at()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            super::_set_output_busy_monitor(None);
        }
    }

    pub fn install(hook: Hook) -> Guard {
        *HOOK.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
        Guard
    }

    /// Mutate the installed hook in place (mid-test re-monkeypatching).
    pub fn update(f: impl FnOnce(&mut Hook)) {
        if let Some(hook) = HOOK.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            f(hook);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testhook::{self, FakeAdapter, Hook};
    use super::*;
    use crate::adapters::claude_bg::EngineSession;
    use crate::adapters::claude_view::PaneView;
    use crate::adapters::codex_app_server::ThreadRuntime;
    use crate::adapters::grok_leader::SessionRuntime;
    use crate::tmux::PaneInfo;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;

    fn claude_profile() -> Option<&'static crate::agent_cli::CLIProfile> {
        crate::agent_cli::get_profile("claude")
    }

    fn grok_profile() -> Option<&'static crate::agent_cli::CLIProfile> {
        crate::agent_cli::get_profile("grok")
    }

    fn codex_profile() -> Option<&'static crate::agent_cli::CLIProfile> {
        crate::agent_cli::get_profile("codex")
    }

    fn backdate(path: &Path, age_seconds: f64) {
        let when = std::time::SystemTime::now() - Duration::from_secs_f64(age_seconds);
        let secs = when
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as libc::time_t;
        let times = [
            libc::timeval {
                tv_sec: secs,
                tv_usec: 0,
            },
            libc::timeval {
                tv_sec: secs,
                tv_usec: 0,
            },
        ];
        let cpath = CString::new(path.as_os_str().as_bytes()).unwrap();
        unsafe { libc::utimes(cpath.as_ptr(), times.as_ptr()) };
    }

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    fn busy_map(busy: bool) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("busy".to_string(), Value::Bool(busy));
        map
    }

    // ---- test_hived_busy_phantom_gate.py -----------------------------------

    /// The Python `_Monitor` fake.
    struct FakeMonitor {
        busy: bool,
        last_output_age: Option<f64>,
    }

    impl FakeMonitor {
        fn new(busy: bool) -> FakeMonitor {
            FakeMonitor {
                busy,
                last_output_age: None,
            }
        }
    }

    impl OutputMonitor for FakeMonitor {
        fn is_busy(&self, _pane_id: &str, _threshold_seconds: f64) -> bool {
            self.busy
        }
        fn last_output_age(&self, _pane_id: &str) -> Option<f64> {
            self.last_output_age
        }
    }

    /// The autouse fixture: fresh path cache, `_native_daemon_busy` → None.
    fn gate_hook() -> Hook {
        Hook {
            native_daemon_busy: Some(Arc::new(|_pane| None)),
            ..Default::default()
        }
    }

    fn stub_path(hook: &mut Hook, path_str: Option<String>) {
        hook.resolve_transcript_path_cached = Some(Arc::new(move |_pane, _force| path_str.clone()));
    }

    fn stub_path_with_force(hook: &mut Hook, cached: Option<String>, fresh: Option<String>) {
        hook.resolve_transcript_path_cached =
            Some(Arc::new(
                move |_pane, force| {
                    if force {
                        fresh.clone()
                    } else {
                        cached.clone()
                    }
                },
            ));
    }

    fn stub_app_server_busy(hook: &mut Hook, value: Option<bool>) {
        hook.native_daemon_busy = Some(Arc::new(move |_pane| value));
    }

    #[test]
    fn test_progressed_returns_none_when_path_unknown() {
        let mut hook = gate_hook();
        stub_path(&mut hook, None);
        let _guard = testhook::install(hook);
        assert_eq!(_transcript_progressed_recently("%1", 3.0), None);
    }

    #[test]
    fn test_progressed_returns_none_when_stat_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let ghost = tmp.path().join("missing.jsonl");
        let mut hook = gate_hook();
        stub_path(&mut hook, Some(ghost.to_string_lossy().to_string()));
        let _guard = testhook::install(hook);
        assert_eq!(_transcript_progressed_recently("%1", 3.0), None);
    }

    #[test]
    fn test_progressed_returns_true_when_mtime_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let fresh = write_file(tmp.path(), "fresh.jsonl", "x");
        let mut hook = gate_hook();
        stub_path(&mut hook, Some(fresh.to_string_lossy().to_string()));
        let _guard = testhook::install(hook);
        assert_eq!(_transcript_progressed_recently("%1", 3.0), Some(true));
    }

    #[test]
    fn test_progressed_returns_false_when_mtime_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let stale = write_file(tmp.path(), "stale.jsonl", "x");
        backdate(&stale, 60.0);
        let mut hook = gate_hook();
        stub_path(&mut hook, Some(stale.to_string_lossy().to_string()));
        let _guard = testhook::install(hook);
        assert_eq!(_transcript_progressed_recently("%1", 3.0), Some(false));
    }

    #[test]
    fn test_progressed_recovers_from_session_switch() {
        // Cached path stale but a forced re-resolve yields a fresh
        // new-session jsonl (e.g. user ran `/new`).
        let tmp = tempfile::tempdir().unwrap();
        let old = write_file(tmp.path(), "old.jsonl", "x");
        backdate(&old, 60.0);
        let new = write_file(tmp.path(), "new.jsonl", "y");
        let mut hook = gate_hook();
        stub_path_with_force(
            &mut hook,
            Some(old.to_string_lossy().to_string()),
            Some(new.to_string_lossy().to_string()),
        );
        let _guard = testhook::install(hook);
        assert_eq!(_transcript_progressed_recently("%1", 3.0), Some(true));
    }

    #[test]
    fn test_progressed_returns_false_when_re_resolve_yields_same_path() {
        let tmp = tempfile::tempdir().unwrap();
        let stale = write_file(tmp.path(), "stale.jsonl", "x");
        backdate(&stale, 60.0);
        let mut hook = gate_hook();
        stub_path_with_force(
            &mut hook,
            Some(stale.to_string_lossy().to_string()),
            Some(stale.to_string_lossy().to_string()),
        );
        let _guard = testhook::install(hook);
        assert_eq!(_transcript_progressed_recently("%1", 3.0), Some(false));
    }

    #[test]
    fn test_progressed_returns_false_when_new_session_also_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let old = write_file(tmp.path(), "old.jsonl", "x");
        backdate(&old, 60.0);
        let new = write_file(tmp.path(), "new.jsonl", "y");
        backdate(&new, 30.0);
        let mut hook = gate_hook();
        stub_path_with_force(
            &mut hook,
            Some(old.to_string_lossy().to_string()),
            Some(new.to_string_lossy().to_string()),
        );
        let _guard = testhook::install(hook);
        assert_eq!(_transcript_progressed_recently("%1", 3.0), Some(false));
    }

    #[test]
    fn test_progressed_returns_false_when_fresh_resolve_yields_no_path() {
        let tmp = tempfile::tempdir().unwrap();
        let stale = write_file(tmp.path(), "stale.jsonl", "x");
        backdate(&stale, 60.0);
        let mut hook = gate_hook();
        stub_path_with_force(&mut hook, Some(stale.to_string_lossy().to_string()), None);
        let _guard = testhook::install(hook);
        assert_eq!(_transcript_progressed_recently("%1", 3.0), Some(false));
    }

    #[test]
    fn test_truly_busy_true_when_app_server_busy() {
        let mut hook = gate_hook();
        stub_path(&mut hook, None);
        stub_app_server_busy(&mut hook, Some(true));
        let _guard = testhook::install(hook);
        assert!(_pane_is_truly_busy("%1", Some(&FakeMonitor::new(false))));
    }

    #[test]
    fn test_truly_busy_false_when_app_server_idle() {
        // App server says idle → authoritative even if tmux monitor reports
        // output.
        let tmp = tempfile::tempdir().unwrap();
        let fresh = write_file(tmp.path(), "fresh.jsonl", "x");
        let mut hook = gate_hook();
        stub_path(&mut hook, Some(fresh.to_string_lossy().to_string()));
        stub_app_server_busy(&mut hook, Some(false));
        let _guard = testhook::install(hook);
        assert!(!_pane_is_truly_busy("%1", Some(&FakeMonitor::new(true))));
    }

    #[test]
    fn test_truly_busy_falls_through_when_no_app_server() {
        let tmp = tempfile::tempdir().unwrap();
        let fresh = write_file(tmp.path(), "fresh.jsonl", "x");
        let mut hook = gate_hook();
        stub_path(&mut hook, Some(fresh.to_string_lossy().to_string()));
        stub_app_server_busy(&mut hook, None);
        let _guard = testhook::install(hook);
        assert!(_pane_is_truly_busy("%1", Some(&FakeMonitor::new(true))));
    }

    #[test]
    fn test_is_output_busy_true_when_app_server_busy() {
        let mut hook = gate_hook();
        stub_path(&mut hook, None);
        stub_app_server_busy(&mut hook, Some(true));
        let _guard = testhook::install(hook);
        assert!(_is_output_busy("%1", Some(&FakeMonitor::new(false)), None));
    }

    #[test]
    fn test_is_output_busy_false_when_app_server_idle() {
        let mut hook = gate_hook();
        stub_path(&mut hook, None);
        stub_app_server_busy(&mut hook, Some(false));
        let _guard = testhook::install(hook);
        assert!(!_is_output_busy("%1", Some(&FakeMonitor::new(true)), None));
    }

    #[test]
    fn test_truly_busy_false_when_monitor_idle() {
        let mut hook = gate_hook();
        stub_path(&mut hook, None);
        let _guard = testhook::install(hook);
        assert!(!_pane_is_truly_busy("%1", Some(&FakeMonitor::new(false))));
    }

    #[test]
    fn test_truly_busy_falls_back_to_monitor_when_path_unknown() {
        // Fallback contract: never silently disable notify for panes the
        // gate can't introspect.
        let mut hook = gate_hook();
        stub_path(&mut hook, None);
        let _guard = testhook::install(hook);
        assert!(_pane_is_truly_busy("%1", Some(&FakeMonitor::new(true))));
    }

    #[test]
    fn test_truly_busy_true_when_monitor_busy_and_transcript_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let fresh = write_file(tmp.path(), "fresh.jsonl", "x");
        let mut hook = gate_hook();
        stub_path(&mut hook, Some(fresh.to_string_lossy().to_string()));
        let _guard = testhook::install(hook);
        assert!(_pane_is_truly_busy("%1", Some(&FakeMonitor::new(true))));
    }

    #[test]
    fn test_truly_busy_false_when_monitor_busy_but_transcript_stale() {
        // Production phantom case: control-mode reports activity but jsonl
        // is 40+ minutes cold.
        let tmp = tempfile::tempdir().unwrap();
        let stale = write_file(tmp.path(), "stale.jsonl", "x");
        backdate(&stale, 60.0);
        let mut hook = gate_hook();
        stub_path(&mut hook, Some(stale.to_string_lossy().to_string()));
        let _guard = testhook::install(hook);
        assert!(!_pane_is_truly_busy("%1", Some(&FakeMonitor::new(true))));
    }

    #[test]
    fn test_truly_busy_false_when_monitor_none() {
        let _guard = testhook::install(gate_hook());
        assert!(!_pane_is_truly_busy("%1", None));
    }

    #[test]
    fn test_truly_busy_false_when_pane_id_empty() {
        let mut hook = gate_hook();
        stub_path(&mut hook, None);
        let _guard = testhook::install(hook);
        assert!(!_pane_is_truly_busy("", Some(&FakeMonitor::new(true))));
    }

    #[test]
    fn test_is_output_busy_respects_inactive_age_when_truly_busy() {
        let tmp = tempfile::tempdir().unwrap();
        let fresh = write_file(tmp.path(), "fresh.jsonl", "x");
        let mut hook = gate_hook();
        stub_path(&mut hook, Some(fresh.to_string_lossy().to_string()));
        let _guard = testhook::install(hook);

        let monitor = FakeMonitor {
            busy: true,
            last_output_age: Some(2.0),
        };
        assert!(_is_output_busy("%1", Some(&monitor), Some(5.0)));
        assert!(!_is_output_busy("%1", Some(&monitor), Some(1.0)));
    }

    #[test]
    fn test_is_output_busy_native_busy_bypasses_inactive_age() {
        // A native runtime source saying busy is independent of when the
        // user last viewed the window.
        let mut hook = gate_hook();
        stub_path(&mut hook, None);
        stub_app_server_busy(&mut hook, Some(true));
        let _guard = testhook::install(hook);
        let monitor = FakeMonitor {
            busy: false,
            last_output_age: Some(20.0),
        };
        assert!(_is_output_busy("%1", Some(&monitor), Some(5.0)));
    }

    #[test]
    fn test_is_output_busy_skips_inactive_age_when_phantom() {
        let tmp = tempfile::tempdir().unwrap();
        let stale = write_file(tmp.path(), "stale.jsonl", "x");
        backdate(&stale, 60.0);
        let mut hook = gate_hook();
        stub_path(&mut hook, Some(stale.to_string_lossy().to_string()));
        let _guard = testhook::install(hook);
        let monitor = FakeMonitor {
            busy: true,
            last_output_age: Some(0.5),
        };
        assert!(!_is_output_busy("%1", Some(&monitor), Some(5.0)));
    }

    #[test]
    fn test_path_cache_hits_within_ttl() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let mut hook = gate_hook();
        hook.is_pane_alive = Some(Arc::new(|_pane| {
            CALLS.fetch_add(1, Ordering::SeqCst);
            false
        }));
        let _guard = testhook::install(hook);

        assert_eq!(_resolve_transcript_path_cached("%1", false), None);
        assert_eq!(_resolve_transcript_path_cached("%1", false), None);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_path_cache_refreshes_after_ttl() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let mut hook = gate_hook();
        hook.is_pane_alive = Some(Arc::new(|_pane| {
            CALLS.fetch_add(1, Ordering::SeqCst);
            false
        }));
        let _guard = testhook::install(hook);

        _resolve_transcript_path_cached("%1", false);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);

        transcript_path_cache().lock().unwrap().insert(
            "%1".to_string(),
            (String::new(), monotonic() - 1.0, String::new()),
        );
        _resolve_transcript_path_cached("%1", false);
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
    }

    // ---- test_hived_runtime_snapshot.py ------------------------------------

    fn seed_snapshot(pane_id: &str, session_id: &str, observed_at: f64, freshness: Option<f64>) {
        runtime_snapshots().lock().unwrap().update_session_id(
            pane_id,
            session_id,
            "pidfile",
            Some(observed_at),
            freshness,
        );
    }

    /// A snapshot written past its freshness window (the `/new` case).
    fn seed_aged_snapshot(pane_id: &str, session_id: &str) -> RuntimeSnapshot {
        runtime_snapshots().lock().unwrap().update_session_id(
            pane_id,
            session_id,
            "pidfile",
            Some(monotonic() - _SESSION_SNAPSHOT_FRESHNESS_S - 1.0),
            Some(_SESSION_SNAPSHOT_FRESHNESS_S),
        )
    }

    #[test]
    fn test_runtime_snapshot_payload_reads_store_without_live_probe() {
        let _guard = testhook::install(Hook::default());
        seed_snapshot("%1", "sid-tick", 10.0, None);

        let payload = _runtime_snapshot_payload("%1");

        assert_eq!(payload["ok"], Value::Bool(true));
        assert_eq!(payload["pane"], Value::from("%1"));
        assert_eq!(payload["snapshot"]["sessionId"], Value::from("sid-tick"));
        assert_eq!(
            payload["snapshot"]["_sessionIdSource"],
            Value::from("pidfile")
        );
    }

    #[test]
    fn test_runtime_snapshot_payload_reports_stale_snapshot() {
        let _guard = testhook::install(Hook::default());
        seed_aged_snapshot("%1", "sid-old");

        let payload = _runtime_snapshot_payload("%1");

        assert_eq!(payload["ok"], Value::Bool(true));
        assert_eq!(payload["snapshot"]["sessionId"], Value::from("sid-old"));
        assert_eq!(payload["snapshot"]["_sessionIdFresh"], Value::Bool(false));
    }

    #[test]
    fn test_runtime_snapshot_payload_returns_none_when_snapshot_missing() {
        let _guard = testhook::install(Hook::default());

        let payload = _runtime_snapshot_payload("%1");

        let mut expected = Map::new();
        expected.insert("ok".to_string(), Value::Bool(true));
        expected.insert("pane".to_string(), Value::from("%1"));
        expected.insert("snapshot".to_string(), Value::Null);
        assert_eq!(payload, expected);
    }

    fn snapshot_resolver_hook(tmp: &Path, new_name: &str) -> (Hook, PathBuf) {
        let new_transcript = write_file(tmp, new_name, "new");
        let find_target = new_transcript.clone();
        let hook = Hook {
            is_pane_alive: Some(Arc::new(|_p| true)),
            display_value: Some(Arc::new(|_p, _f| Some("/repo".to_string()))),
            detect_profile_for_pane: Some(Arc::new(|_p| claude_profile())),
            adapters_get: Some(Arc::new(move |name| {
                if name != "claude" {
                    return None;
                }
                let find_target = find_target.clone();
                Some(AdapterHandle::Fake(FakeAdapter {
                    resolve: Arc::new(|pane| {
                        assert_eq!(pane, "%1");
                        Some("sid-new".to_string())
                    }),
                    find: Arc::new(move |sid, cwd| {
                        assert_eq!(sid, "sid-new");
                        assert_eq!(cwd, Some("/repo"));
                        Some(find_target.clone())
                    }),
                }))
            })),
            ..Default::default()
        };
        (hook, new_transcript)
    }

    #[test]
    fn test_resolve_transcript_path_cached_ignores_stale_snapshot_and_cached_path() {
        let tmp = tempfile::tempdir().unwrap();
        let old_transcript = write_file(tmp.path(), "old.jsonl", "old");
        let (hook, new_transcript) = snapshot_resolver_hook(tmp.path(), "new.jsonl");
        let _guard = testhook::install(hook);
        seed_aged_snapshot("%1", "sid-old");
        transcript_path_cache().lock().unwrap().insert(
            "%1".to_string(),
            (
                old_transcript.to_string_lossy().to_string(),
                monotonic() + 60.0,
                "sid-old".to_string(),
            ),
        );

        assert_eq!(
            _resolve_transcript_path_cached("%1", false),
            Some(new_transcript.to_string_lossy().to_string())
        );
    }

    #[test]
    fn test_resolve_transcript_path_cached_ignores_stale_snapshot_negative_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let (hook, new_transcript) = snapshot_resolver_hook(tmp.path(), "new.jsonl");
        let _guard = testhook::install(hook);
        seed_aged_snapshot("%1", "sid-old");
        transcript_path_cache().lock().unwrap().insert(
            "%1".to_string(),
            (String::new(), monotonic() + 60.0, String::new()),
        );

        assert_eq!(
            _resolve_transcript_path_cached("%1", false),
            Some(new_transcript.to_string_lossy().to_string())
        );
    }

    #[test]
    fn test_resolve_transcript_path_cached_requires_same_snapshot_session() {
        let tmp = tempfile::tempdir().unwrap();
        let old_transcript = write_file(tmp.path(), "old.jsonl", "old");
        let new_transcript = write_file(tmp.path(), "new.jsonl", "new");
        let find_target = new_transcript.clone();
        let hook = Hook {
            is_pane_alive: Some(Arc::new(|_p| true)),
            display_value: Some(Arc::new(|_p, _f| Some("/repo".to_string()))),
            detect_profile_for_pane: Some(Arc::new(|_p| claude_profile())),
            adapters_get: Some(Arc::new(move |_name| {
                let find_target = find_target.clone();
                Some(AdapterHandle::Fake(FakeAdapter {
                    resolve: Arc::new(|_pane| {
                        panic!("fresh snapshot session should be used");
                    }),
                    find: Arc::new(move |sid, cwd| {
                        assert_eq!(sid, "sid-new");
                        assert_eq!(cwd, Some("/repo"));
                        Some(find_target.clone())
                    }),
                }))
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        seed_snapshot("%1", "sid-new", monotonic(), None);
        transcript_path_cache().lock().unwrap().insert(
            "%1".to_string(),
            (
                old_transcript.to_string_lossy().to_string(),
                monotonic() + 60.0,
                "sid-old".to_string(),
            ),
        );

        assert_eq!(
            _resolve_transcript_path_cached("%1", false),
            Some(new_transcript.to_string_lossy().to_string())
        );
    }

    #[test]
    fn test_agent_runtime_payload_does_not_consume_stale_snapshot_or_pidfile() {
        let hook = Hook {
            is_pane_alive: Some(Arc::new(|_p| true)),
            busy_output_payload: Some(Arc::new(|_p| busy_map(false))),
            detect_cli_process_for_pane: Some(Arc::new(|_p| claude_profile())),
            resolve_model_for_pane: Some(Arc::new(|_p, _c, _m| String::new())),
            claude_bg_runtime: Some(Arc::new(|_p| None)),
            claude_pid_for_pane: Some(Arc::new(|_p| None)),
            cs_session_status: Some(Arc::new(|_pid| None)),
            adapters_get: Some(Arc::new(|name| {
                if name != "claude" {
                    return None;
                }
                Some(AdapterHandle::Fake(FakeAdapter {
                    resolve: Arc::new(|pane| {
                        assert_eq!(pane, "%1");
                        None
                    }),
                    find: Arc::new(|_sid, _cwd| {
                        panic!("stale session should not be resolved");
                    }),
                }))
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        let stale = seed_aged_snapshot("%1", "sid-old");

        let runtime = _agent_runtime_payload("%1", Some(&stale));

        assert_eq!(runtime["sessionId"], Value::from("unresolved"));
        assert_eq!(runtime["inputState"], Value::from("unknown"));
        assert_eq!(runtime["inputReason"], Value::from("no_session"));
    }

    #[test]
    fn test_agent_runtime_payload_stamps_a_freshness_window_on_a_probed_session() {
        // Without a window the first probed id is pinned forever: after
        // `/new` in an unmanaged pane the hived would keep serving the dead
        // session.
        let hook = Hook {
            is_pane_alive: Some(Arc::new(|_p| true)),
            display_value: Some(Arc::new(|_p, _f| Some("/repo".to_string()))),
            busy_output_payload: Some(Arc::new(|_p| busy_map(false))),
            claude_bg_runtime: Some(Arc::new(|_p| None)),
            detect_cli_process_for_pane: Some(Arc::new(|_p| claude_profile())),
            resolve_model_for_pane: Some(Arc::new(|_p, _c, _m| String::new())),
            claude_pid_for_pane: Some(Arc::new(|_p| None)),
            cs_session_status: Some(Arc::new(|_pid| None)),
            adapters_get: Some(Arc::new(|name| {
                if name != "claude" {
                    return None;
                }
                Some(AdapterHandle::Fake(FakeAdapter {
                    resolve: Arc::new(|_pane| Some("sid-new".to_string())),
                    find: Arc::new(|_sid, _cwd| None),
                }))
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);

        assert_eq!(
            _agent_runtime_payload("%1", None)["sessionId"],
            Value::from("sid-new")
        );

        let store = runtime_snapshots().lock().unwrap();
        let field = &store.get("%1").unwrap().sessionId;
        assert_eq!(field.freshness_s, Some(_SESSION_SNAPSHOT_FRESHNESS_S));
        assert!(field.is_fresh(Some(field.observed_at + 1.0)));
        assert!(!field.is_fresh(Some(field.observed_at + field.freshness_s.unwrap() + 1.0)));
    }

    // ---- test_hived_claude_runtime.py --------------------------------------

    fn engine(status: &str, waiting_for: &str, session_id: &str) -> EngineSession {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        EngineSession {
            pid: 4242,
            job_id: "cafe1234".to_string(),
            session_id: session_id.to_string(),
            socket_path: "/tmp/cc-socks/4242.sock".to_string(),
            cwd: "/w".to_string(),
            status: status.to_string(),
            waiting_for: waiting_for.to_string(),
            status_updated_at: now,
            name: String::new(),
        }
    }

    fn pin(
        hook: &mut Hook,
        record: Option<(String, String, String)>,
        engine: Option<EngineSession>,
        rows: Option<Vec<Map<String, Value>>>,
    ) {
        hook.cb_read_pane_job = Some(Arc::new(move |_p| record.clone()));
        hook.cb_engine_session_for_job = Some(Arc::new(move |_j| engine.clone()));
        hook.cb_list_jobs = Some(Arc::new(move || rows.clone()));
    }

    fn record(job: &str, sid: &str) -> Option<(String, String, String)> {
        Some((job.to_string(), sid.to_string(), "/w".to_string()))
    }

    #[test]
    fn test_bg_runtime_live_engine_reports_status_and_session() {
        let mut hook = Hook::default();
        pin(
            &mut hook,
            record("cafe1234", "sess-old"),
            Some(engine("busy", "", "sess-live")),
            Some(vec![]),
        );
        let _guard = testhook::install(hook);

        let rt = _claude_bg_runtime("%1").unwrap();

        assert_eq!(rt["cliAlive"], Value::Bool(true));
        assert_eq!(rt["busy"], Value::Bool(true));
        assert_eq!(rt["inputState"], Value::from("ready"));
        assert_eq!(rt["sessionId"], Value::from("sess-live")); // engine truth beats the record
        assert_eq!(rt["_runtimeSource"], Value::from("claude_bg"));
    }

    #[test]
    fn test_bg_runtime_waiting_engine_maps_waiting_for() {
        let mut hook = Hook::default();
        pin(
            &mut hook,
            record("cafe1234", ""),
            Some(engine("waiting", "input needed", "sess-live")),
            Some(vec![]),
        );
        let _guard = testhook::install(hook);

        let rt = _claude_bg_runtime("%1").unwrap();

        assert_eq!(rt["busy"], Value::Bool(false));
        assert_eq!(rt["inputState"], Value::from("waiting_user"));
        assert_eq!(rt["inputReason"], Value::from("registry:input needed"));
    }

    #[test]
    fn test_bg_runtime_asleep_is_reachable_not_dead() {
        // supervisor parked the engine: the ledger row survives without
        // pid/status
        let mut asleep_row = Map::new();
        asleep_row.insert("id".to_string(), Value::from("cafe1234"));
        asleep_row.insert("state".to_string(), Value::from("stopped"));
        asleep_row.insert("sessionId".to_string(), Value::from("sess-row"));
        let mut hook = Hook::default();
        pin(
            &mut hook,
            record("cafe1234", "sess-old"),
            None,
            Some(vec![asleep_row]),
        );
        let _guard = testhook::install(hook);

        let rt = _claude_bg_runtime("%1").unwrap();

        assert_eq!(rt["cliAlive"], Value::Bool(true)); // asleep, wake-on-delivery — never reaped
        assert_eq!(rt["busy"], Value::Bool(false));
        assert_eq!(rt["inputState"], Value::from("ready"));
        assert_eq!(rt["_engineState"], Value::from("asleep"));
        assert_eq!(rt["sessionId"], Value::from("sess-row"));
    }

    #[test]
    fn test_bg_runtime_gone_job_is_offline() {
        let mut hook = Hook::default();
        pin(
            &mut hook,
            record("cafe1234", "sess-old"),
            None,
            Some(vec![]),
        );
        let _guard = testhook::install(hook);

        let rt = _claude_bg_runtime("%1").unwrap();

        assert_eq!(rt["cliAlive"], Value::Bool(false));
        assert_eq!(rt["inputState"], Value::from("offline"));
        assert_eq!(rt["inputReason"], Value::from("engine_gone"));
        assert_eq!(rt["sessionId"], Value::from("sess-old"));
    }

    #[test]
    fn test_bg_runtime_ledger_failure_is_unknown_not_dead() {
        let mut hook = Hook::default();
        pin(&mut hook, record("cafe1234", ""), None, None);
        let _guard = testhook::install(hook);

        let rt = _claude_bg_runtime("%1").unwrap();

        assert_eq!(rt["cliAlive"], Value::Bool(true)); // benefit of the doubt: never a reap signal
        assert_eq!(rt["inputState"], Value::from("unknown"));
        assert_eq!(rt["inputReason"], Value::from("ledger_unavailable"));
    }

    #[test]
    fn test_bg_runtime_none_for_unmanaged_pane() {
        let mut hook = Hook::default();
        pin(&mut hook, None, None, Some(vec![]));
        let _guard = testhook::install(hook);
        assert!(_claude_bg_runtime("%1").is_none());
    }

    #[test]
    fn test_jobs_ledger_is_cached_between_reads() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let mut hook = Hook::default();
        pin(&mut hook, record("cafe1234", ""), None, Some(vec![]));
        hook.cb_list_jobs = Some(Arc::new(|| {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Some(vec![])
        }));
        let _guard = testhook::install(hook);

        _claude_bg_runtime("%1");
        _claude_bg_runtime("%1");

        // the ~270ms CLI call never runs per tick per pane
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    }

    fn quiet_view_hook(hook: &mut Hook) {
        hook.cv_view_for_pane = Some(Arc::new(|_p| crate::adapters::claude_view::PaneView {
            certainty: String::new(),
            kind: "no_viewer".to_string(),
            job_id: String::new(),
            member: String::new(),
            title: String::new(),
            why: String::new(),
        }));
    }

    #[test]
    fn test_agent_runtime_payload_reaches_bg_branch_without_a_viewer() {
        // viewer gap: no process on the tty, but the pane records a live
        // job — the member must not read as cli_exited
        let mut hook = Hook {
            is_pane_alive: Some(Arc::new(|_p| true)),
            busy_output_payload: Some(Arc::new(|_p| busy_map(false))),
            detect_cli_process_for_pane: Some(Arc::new(|_p| None)),
            resolve_model_for_pane: Some(Arc::new(|_p, _c, _m| String::new())),
            ..Default::default()
        };
        pin(
            &mut hook,
            record("cafe1234", ""),
            Some(engine("idle", "", "sess-live")),
            Some(vec![]),
        );
        quiet_view_hook(&mut hook);
        let _guard = testhook::install(hook);

        let rt = _agent_runtime_payload("%1", None);

        assert_eq!(rt["_cli"], Value::from("claude"));
        assert_eq!(rt["cliAlive"], Value::Bool(true));
        assert_eq!(rt["busy"], Value::Bool(false));
        assert_eq!(rt["inputState"], Value::from("ready"));
        assert_eq!(rt["sessionId"], Value::from("sess-live"));
    }

    #[test]
    fn test_claude_registry_busy_prefers_job_engine() {
        let hook = Hook {
            cb_job_id_for_pane: Some(Arc::new(|_p| Some("cafe1234".to_string()))),
            cb_engine_session_for_job: Some(Arc::new(|_j| Some(engine("busy", "", "s")))),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        assert_eq!(_claude_registry_busy("%1"), Some(true));
    }

    #[test]
    fn test_claude_registry_busy_falls_back_to_interactive_entry() {
        let hook = Hook {
            cb_job_id_for_pane: Some(Arc::new(|_p| None)),
            claude_pid_for_pane: Some(Arc::new(|_p| Some(777))),
            cs_session_status: Some(Arc::new(|pid| {
                if pid == Some(777) {
                    Some(("busy".to_string(), String::new()))
                } else {
                    None
                }
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        assert_eq!(_claude_registry_busy("%1"), Some(true));
    }

    #[test]
    fn test_claude_registry_busy_none_without_any_source() {
        let hook = Hook {
            cb_job_id_for_pane: Some(Arc::new(|_p| None)),
            claude_pid_for_pane: Some(Arc::new(|_p| None)),
            cs_session_status: Some(Arc::new(|_pid| None)),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        assert_eq!(_claude_registry_busy("%1"), None);
    }

    /// A live interactive (non-member) claude on the pane tty: no job
    /// record, a resolvable session, and *status* as its registry entry's
    /// report.
    fn interactive_claude_pane(
        tmp: &Path,
        status: Option<(String, String)>,
        transcript: bool,
    ) -> Hook {
        let path = write_file(tmp, "sess-i.jsonl", "{}\n");
        let mut hook = Hook {
            is_pane_alive: Some(Arc::new(|_p| true)),
            display_value: Some(Arc::new(|_p, _f| Some("/w".to_string()))),
            busy_output_payload: Some(Arc::new(|_p| busy_map(false))),
            detect_cli_process_for_pane: Some(Arc::new(|_p| claude_profile())),
            resolve_model_for_pane: Some(Arc::new(|_p, _c, _m| String::new())),
            cb_read_pane_job: Some(Arc::new(|_p| None)),
            claude_pid_for_pane: Some(Arc::new(|_p| Some(777))),
            cs_session_status: Some(Arc::new(move |pid| {
                if pid == Some(777) {
                    status.clone()
                } else {
                    None
                }
            })),
            ..Default::default()
        };
        hook.adapters_get = Some(Arc::new(move |_name| {
            let path = path.clone();
            Some(AdapterHandle::Fake(FakeAdapter {
                resolve: Arc::new(|_p| Some("sess-i".to_string())),
                find: Arc::new(
                    move |_sid, _cwd| {
                        if transcript {
                            Some(path.clone())
                        } else {
                            None
                        }
                    },
                ),
            }))
        }));
        hook
    }

    fn forbid_gate(hook: &mut Hook, message: &'static str) {
        hook.check_input_gate = Some(Arc::new(move |_path| panic!("{}", message)));
    }

    #[test]
    fn test_interactive_claude_takes_input_state_from_its_registry_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let mut hook = interactive_claude_pane(
            tmp.path(),
            Some(("waiting".to_string(), "input needed".to_string())),
            true,
        );
        forbid_gate(&mut hook, "the registry answered; the gate must not run");
        let _guard = testhook::install(hook);

        let rt = _agent_runtime_payload("%7", None);

        assert_eq!(rt["inputState"], Value::from("waiting_user"));
        assert_eq!(rt["inputReason"], Value::from("registry:input needed"));
        assert_eq!(rt["busy"], Value::Bool(false));
        assert_eq!(rt["sessionId"], Value::from("sess-i"));
        assert_eq!(rt["_runtimeSource"], Value::from("claude_registry"));
    }

    #[test]
    fn test_interactive_claude_status_maps_like_the_bg_engine() {
        for (status, expected) in [("busy", true), ("shell", false), ("idle", false)] {
            let tmp = tempfile::tempdir().unwrap();
            let mut hook = interactive_claude_pane(
                tmp.path(),
                Some((status.to_string(), String::new())),
                true,
            );
            forbid_gate(&mut hook, "the registry answered; the gate must not run");
            let _guard = testhook::install(hook);

            let rt = _agent_runtime_payload("%7", None);

            assert_eq!(rt["busy"], Value::Bool(expected), "status={status}");
            // `shell` is neither mid-turn nor a wait
            assert_eq!(rt["inputState"], Value::from("ready"), "status={status}");
        }
    }

    #[test]
    fn test_interactive_claude_without_a_registry_status_falls_back_to_the_gate() {
        // headless/desktop-hosted sessions report nothing; the transcript
        // gate is still the only answer available for them
        let tmp = tempfile::tempdir().unwrap();
        let mut hook = interactive_claude_pane(tmp.path(), None, true);
        hook.check_input_gate = Some(Arc::new(|_path| crate::adapters::base::GateResult {
            status: "waiting",
            reason: String::new(),
        }));
        let _guard = testhook::install(hook);

        let rt = _agent_runtime_payload("%7", None);

        assert_eq!(rt["inputState"], Value::from("waiting_user"));
        assert_eq!(rt["inputReason"], Value::from("ask_pending"));
        assert!(!rt.contains_key("_runtimeSource"));
    }

    #[test]
    fn test_claude_supervisor_tick_parks_jobs_of_dead_panes() {
        let cleared: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let stopped: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cleared_sink = Arc::clone(&cleared);
        let stopped_sink = Arc::clone(&stopped);
        let mut records: HashMap<String, (String, String, String)> = HashMap::new();
        records.insert(
            "%9".to_string(),
            ("dead0001".to_string(), "s".to_string(), "/w".to_string()),
        );
        records.insert(
            "%1".to_string(),
            ("live0001".to_string(), "s".to_string(), "/w".to_string()),
        );
        let hook = Hook {
            list_panes_all: Some(Arc::new(|| {
                vec![crate::tmux::PaneInfo {
                    pane_id: "%1".to_string(),
                    ..Default::default()
                }]
            })),
            cb_list_recorded_panes: Some(Arc::new(|| vec!["%1".to_string(), "%9".to_string()])),
            cb_read_pane_job: Some(Arc::new(move |pane| records.get(pane).cloned())),
            cb_clear_pane_job: Some(Arc::new(move |pane| {
                cleared_sink.lock().unwrap().push(pane.to_string())
            })),
            cb_stop_job: Some(Arc::new(move |job| {
                stopped_sink.lock().unwrap().push(job.to_string())
            })),
            notify_debug_emit: Some(Arc::new(|_ws, _event, _fields| {})),
            ..Default::default()
        };
        let _guard = testhook::install(hook);

        _claude_supervisor_tick("/tmp/ws");

        // the live pane's record is untouched
        assert_eq!(*cleared.lock().unwrap(), vec!["%9".to_string()]);
        assert_eq!(*stopped.lock().unwrap(), vec!["dead0001".to_string()]);
    }

    #[test]
    fn test_claude_supervisor_tick_treats_empty_listing_as_tmux_failure() {
        let cleared: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cleared_sink = Arc::clone(&cleared);
        let hook = Hook {
            list_panes_all: Some(Arc::new(Vec::new)),
            cb_list_recorded_panes: Some(Arc::new(|| vec!["%9".to_string()])),
            cb_clear_pane_job: Some(Arc::new(move |pane| {
                cleared_sink.lock().unwrap().push(pane.to_string())
            })),
            notify_debug_emit: Some(Arc::new(|_ws, _event, _fields| {})),
            ..Default::default()
        };
        let _guard = testhook::install(hook);

        _claude_supervisor_tick("/tmp/ws");

        // unknown is not dead: nothing pruned, nothing parked
        assert!(cleared.lock().unwrap().is_empty());
    }

    // ---- test_hived_codex_runtime.py ---------------------------------------

    fn thread_runtime(busy: bool, turn_phase: &str, input_state: &str) -> ThreadRuntime {
        ThreadRuntime {
            busy,
            turn_phase: turn_phase.to_string(),
            input_state: input_state.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn test_codex_app_server_runtime_maps_fields() {
        let rt = thread_runtime(true, "tool_open", "ready");
        let hook = Hook {
            cas_runtime_for_pane: Some(Arc::new(move |_p| Some(rt.clone()))),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        let out = _codex_app_server_runtime("%5").unwrap();
        assert_eq!(out["busy"], Value::Bool(true));
        assert_eq!(out["turnPhase"], Value::from("tool_open"));
        assert_eq!(out["inputState"], Value::from("ready"));
        assert_eq!(out["_runtimeSource"], Value::from("codex_app_server"));
    }

    #[test]
    fn test_codex_app_server_runtime_none_without_daemon() {
        let hook = Hook {
            cas_runtime_for_pane: Some(Arc::new(|_p| None)),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        assert!(_codex_app_server_runtime("%5").is_none());
    }

    #[test]
    fn test_codex_app_server_runtime_waiting_user() {
        let rt = thread_runtime(true, "tool_open", "waiting_user");
        let hook = Hook {
            cas_runtime_for_pane: Some(Arc::new(move |_p| Some(rt.clone()))),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        let out = _codex_app_server_runtime("%5").unwrap();
        assert_eq!(out["inputState"], Value::from("waiting_user"));
        assert_eq!(out["inputReason"], Value::from("app_server_active_flag"));
    }

    fn fake_team(name: &str, agents: Vec<Agent>) -> Team {
        Team {
            name: name.to_string(),
            agents,
            ..Default::default()
        }
    }

    fn fake_agent(name: &str, pane_id: &str, cli: &str) -> Agent {
        Agent {
            name: name.to_string(),
            team_name: String::new(),
            pane_id: pane_id.to_string(),
            model: String::new(),
            prompt: String::new(),
            cwd: "/repo".to_string(),
            session_id: None,
            spawned_at: 0.0,
            cli: cli.to_string(),
        }
    }

    #[test]
    fn test_doctor_verbose_reports_codex_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let hook = Hook {
            team_load: Some(Arc::new(|_name| {
                Ok(fake_team("t", vec![fake_agent("a", "%5", "codex")]))
            })),
            agent_is_alive: Some(Arc::new(|_a| true)),
            member_runtime_payload: Some(Arc::new(|_p, _r| {
                let mut rt = Map::new();
                rt.insert("alive".to_string(), Value::Bool(true));
                rt.insert("_cli".to_string(), Value::from("codex"));
                rt
            })),
            cas_shared_socket_path: Some(Arc::new(|| PathBuf::from("/x/hive-shared.sock"))),
            cas_daemon_alive: Some(Arc::new(|| true)),
            cas_thread_id_for_pane: Some(Arc::new(|_p| Some("tid-5".to_string()))),
            ..Default::default()
        };
        let _guard = testhook::install(hook);

        let diag = _doctor_payload(&tmp.path().to_string_lossy(), "t", "a", true, None).unwrap();

        let mut expected = Map::new();
        expected.insert("socket".to_string(), Value::from("/x/hive-shared.sock"));
        expected.insert("alive".to_string(), Value::Bool(true));
        expected.insert("threadId".to_string(), Value::from("tid-5"));
        assert_eq!(diag["codexDaemon"], Value::Object(expected));
    }

    // ---- test_hived_grok_runtime.py ----------------------------------------

    fn session_runtime(busy: bool, turn_phase: &str, input_state: &str) -> SessionRuntime {
        SessionRuntime {
            busy,
            turn_phase: turn_phase.to_string(),
            input_state: input_state.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn test_grok_leader_runtime_maps_fields() {
        let rt = session_runtime(true, "tool_open", "ready");
        let hook = Hook {
            gl_runtime_for_pane: Some(Arc::new(move |_p| Some(rt.clone()))),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        let out = _grok_leader_runtime("%5").unwrap();
        assert_eq!(out["busy"], Value::Bool(true));
        assert_eq!(out["turnPhase"], Value::from("tool_open"));
        assert_eq!(out["inputState"], Value::from("ready"));
        assert_eq!(out["inputReason"], Value::from(""));
        assert_eq!(out["_runtimeSource"], Value::from("grok-leader"));
    }

    #[test]
    fn test_grok_leader_runtime_none_without_daemon() {
        let hook = Hook {
            gl_runtime_for_pane: Some(Arc::new(|_p| None)),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        assert!(_grok_leader_runtime("%5").is_none());
    }

    #[test]
    fn test_grok_leader_runtime_defaults_empty_input_state_to_ready() {
        let rt = session_runtime(true, "user_prompt_pending", "");
        let hook = Hook {
            gl_runtime_for_pane: Some(Arc::new(move |_p| Some(rt.clone()))),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        assert_eq!(
            _grok_leader_runtime("%5").unwrap()["inputState"],
            Value::from("ready")
        );
    }

    #[test]
    fn test_grok_leader_runtime_waiting_user() {
        let rt = session_runtime(true, "tool_open", "waiting_user");
        let hook = Hook {
            gl_runtime_for_pane: Some(Arc::new(move |_p| Some(rt.clone()))),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        let out = _grok_leader_runtime("%5").unwrap();
        assert_eq!(out["inputState"], Value::from("waiting_user"));
        assert_eq!(out["inputReason"], Value::from("leader_permission_request"));
    }

    fn live_grok_pane(runtime: Option<SessionRuntime>, session_id: Option<String>) -> Hook {
        Hook {
            is_pane_alive: Some(Arc::new(|_p| true)),
            busy_output_payload: Some(Arc::new(|_p| busy_map(false))),
            detect_cli_process_for_pane: Some(Arc::new(|_p| grok_profile())),
            resolve_model_for_pane: Some(Arc::new(|_p, _c, _m| String::new())),
            gl_runtime_for_pane: Some(Arc::new(move |_p| runtime.clone())),
            gl_session_id_for_pane: Some(Arc::new(move |_p| session_id.clone())),
            ..Default::default()
        }
    }

    #[test]
    fn test_agent_payload_grok_branch_reports_minted_session() {
        let hook = live_grok_pane(
            Some(session_runtime(true, "tool_open", "ready")),
            Some("sid-grok-1".to_string()),
        );
        let _guard = testhook::install(hook);
        let rt = _agent_runtime_payload("%5", None);
        assert_eq!(rt["cliAlive"], Value::Bool(true));
        assert_eq!(rt["busy"], Value::Bool(true));
        assert_eq!(rt["turnPhase"], Value::from("tool_open"));
        assert_eq!(rt["_runtimeSource"], Value::from("grok-leader"));
        assert_eq!(rt["sessionId"], Value::from("sid-grok-1"));
    }

    #[test]
    fn test_agent_payload_grok_session_unresolved_without_record() {
        let hook = live_grok_pane(Some(session_runtime(false, "turn_closed", "ready")), None);
        let _guard = testhook::install(hook);
        assert_eq!(
            _agent_runtime_payload("%5", None)["sessionId"],
            Value::from("unresolved")
        );
    }

    #[test]
    fn test_agent_payload_grok_reports_unknown_without_leader_runtime() {
        // No leader state to read, and the transcript gate below only knows
        // the claude/codex record shapes — it reads a pending grok
        // permission request as clear and opens the send gate
        // mid-permission. Never fall into it.
        let mut hook = live_grok_pane(None, Some("sid-grok-2".to_string()));
        forbid_gate(&mut hook, "grok must not reach the transcript gate");
        let _guard = testhook::install(hook);

        let rt = _agent_runtime_payload("%5", None);
        assert_eq!(rt["sessionId"], Value::from("sid-grok-2"));
        assert_eq!(rt["inputState"], Value::from("unknown"));
        assert_eq!(rt["inputReason"], Value::from("no_leader_runtime"));
        assert!(!rt.contains_key("_transcript"));
        assert!(!rt.contains_key("_runtimeSource"));
    }

    #[test]
    fn test_native_daemon_busy_consults_grok_after_codex() {
        for busy in [true, false] {
            let hook = Hook {
                cas_runtime_for_pane: Some(Arc::new(|_p| None)),
                gl_runtime_for_pane: Some(Arc::new(move |_p| {
                    Some(SessionRuntime {
                        busy,
                        ..Default::default()
                    })
                })),
                ..Default::default()
            };
            let _guard = testhook::install(hook);
            assert_eq!(_native_daemon_busy("%5"), Some(busy));
        }
    }

    #[test]
    fn test_native_daemon_busy_none_when_no_daemon_holds_the_pane() {
        let hook = Hook {
            cas_runtime_for_pane: Some(Arc::new(|_p| None)),
            gl_runtime_for_pane: Some(Arc::new(|_p| None)),
            cb_job_id_for_pane: Some(Arc::new(|_p| None)),
            claude_pid_for_pane: Some(Arc::new(|_p| None)),
            cs_session_status: Some(Arc::new(|_pid| None)),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        assert_eq!(_native_daemon_busy("%5"), None);
    }

    // ---- test_hived_claude_view_tick.py ------------------------------------

    fn view_members() -> Vec<(String, Map<String, Value>)> {
        let mut red = Map::new();
        red.insert("name".to_string(), Value::from("red"));
        red.insert("pane".to_string(), Value::from("%1"));
        red.insert("cli".to_string(), Value::from("claude"));
        red.insert("role".to_string(), Value::from("agent"));
        vec![("red".to_string(), red)]
    }

    fn view_pane(pane_id: &str, title: &str, cli: &str) -> PaneInfo {
        PaneInfo {
            pane_id: pane_id.to_string(),
            title: title.to_string(),
            cli: cli.to_string(),
            ..Default::default()
        }
    }

    fn pane_view(certainty: &str, kind: &str, job_id: &str, member: &str, title: &str) -> PaneView {
        PaneView {
            certainty: certainty.to_string(),
            kind: kind.to_string(),
            job_id: job_id.to_string(),
            member: member.to_string(),
            title: title.to_string(),
            why: String::new(),
        }
    }

    /// Wire the tick's inputs; collect the tmux options it sets.
    struct ViewTickEnv {
        panes: Arc<Mutex<Vec<PaneInfo>>>,
        signature: Arc<Mutex<Vec<String>>>,
        view: Arc<Mutex<PaneView>>,
        options: Arc<Mutex<Vec<(String, String, String)>>>,
        events: Arc<Mutex<Vec<(String, Map<String, Value>)>>>,
        state: ClaudeTickState,
        _guard: testhook::Guard,
    }

    fn view_tick_env() -> ViewTickEnv {
        let panes = Arc::new(Mutex::new(vec![view_pane("%1", "", "claude")]));
        let signature = Arc::new(Mutex::new(vec!["one.json".to_string()]));
        let view = Arc::new(Mutex::new(pane_view(
            "certain",
            "member_view",
            "cafe1234",
            "probe.red",
            "",
        )));
        let options: Arc<Mutex<Vec<(String, String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let events: Arc<Mutex<Vec<(String, Map<String, Value>)>>> =
            Arc::new(Mutex::new(Vec::new()));

        let panes_src = Arc::clone(&panes);
        let signature_src = Arc::clone(&signature);
        let view_src = Arc::clone(&view);
        let options_sink = Arc::clone(&options);
        let events_sink = Arc::clone(&events);
        let hook = Hook {
            list_panes_all: Some(Arc::new(move || panes_src.lock().unwrap().clone())),
            cv_journal_signature: Some(Arc::new(move || signature_src.lock().unwrap().clone())),
            cv_view_for_pane: Some(Arc::new(move |_p| view_src.lock().unwrap().clone())),
            cb_job_id_for_pane: Some(Arc::new(|_p| Some("cafe1234".to_string()))),
            set_pane_option: Some(Arc::new(move |pane, key, value| {
                options_sink.lock().unwrap().push((
                    pane.to_string(),
                    key.to_string(),
                    value.to_string(),
                ))
            })),
            notify_debug_emit: Some(Arc::new(move |_ws, event, fields| {
                let mut map = Map::new();
                for (key, value) in fields {
                    map.insert(key.to_string(), value.clone());
                }
                events_sink.lock().unwrap().push((event.to_string(), map))
            })),
            ..Default::default()
        };
        ViewTickEnv {
            panes,
            signature,
            view,
            options,
            events,
            state: ClaudeTickState::default(),
            _guard: testhook::install(hook),
        }
    }

    fn run_view_tick(env: &mut ViewTickEnv) {
        let members = view_members();
        _claude_view_tick("/tmp/ws", "probe", &members, &mut env.state);
    }

    #[test]
    fn test_pane_on_its_own_member_carries_no_drift_label() {
        let mut env = view_tick_env();
        run_view_tick(&mut env);
        assert_eq!(
            *env.options.lock().unwrap(),
            vec![("%1".to_string(), "hive-view".to_string(), String::new())]
        );
        assert!(env.events.lock().unwrap().is_empty());
    }

    #[test]
    fn test_switching_to_another_member_labels_the_border_and_logs_it() {
        let mut env = view_tick_env();
        *env.view.lock().unwrap() = pane_view("likely", "member_view", "beef5678", "comb.blue", "");

        run_view_tick(&mut env);

        assert_eq!(
            *env.options.lock().unwrap(),
            vec![(
                "%1".to_string(),
                "hive-view".to_string(),
                "comb.blue".to_string()
            )]
        );
        let events = env.events.lock().unwrap();
        let (event, fields) = &events[0];
        assert_eq!(event, "claude.view.foreign_member");
        assert_eq!(fields["viewing"], Value::from("comb.blue"));
        assert_eq!(fields["otherTeam"], Value::Bool(true));
    }

    #[test]
    fn test_a_foreign_session_labels_the_border_without_an_event() {
        let mut env = view_tick_env();
        *env.view.lock().unwrap() = pane_view("likely", "foreign", "", "", "someone-elses-job");

        run_view_tick(&mut env);

        assert_eq!(
            *env.options.lock().unwrap(),
            vec![(
                "%1".to_string(),
                "hive-view".to_string(),
                "someone-elses-job".to_string()
            )]
        );
        assert!(env.events.lock().unwrap().is_empty());
    }

    #[test]
    fn test_unchanged_signals_cost_nothing() {
        let mut env = view_tick_env();
        run_view_tick(&mut env);
        env.options.lock().unwrap().clear();

        run_view_tick(&mut env); // same journal entries, same titles

        assert!(env.options.lock().unwrap().is_empty());
    }

    #[test]
    fn test_a_journal_change_re_probes_and_updates_the_label() {
        // Went to another member's session, then back to the panel list.
        let mut env = view_tick_env();
        *env.view.lock().unwrap() = pane_view("likely", "member_view", "beef5678", "comb.blue", "");
        run_view_tick(&mut env);
        env.options.lock().unwrap().clear();
        *env.signature.lock().unwrap() = vec!["two.json".to_string()];
        *env.view.lock().unwrap() = pane_view("certain", "list_view", "", "", "");

        run_view_tick(&mut env);

        assert_eq!(
            *env.options.lock().unwrap(),
            vec![("%1".to_string(), "hive-view".to_string(), String::new())]
        );
    }

    #[test]
    fn test_a_title_change_alone_re_probes() {
        let mut env = view_tick_env();
        run_view_tick(&mut env);
        env.options.lock().unwrap().clear();
        *env.panes.lock().unwrap() = vec![view_pane("%1", "comb.blue", "claude")];
        *env.view.lock().unwrap() = pane_view("likely", "member_view", "beef5678", "comb.blue", "");

        run_view_tick(&mut env);

        assert_eq!(
            *env.options.lock().unwrap(),
            vec![(
                "%1".to_string(),
                "hive-view".to_string(),
                "comb.blue".to_string()
            )]
        );
    }

    #[test]
    fn test_non_claude_members_are_left_alone() {
        let mut env = view_tick_env();
        *env.panes.lock().unwrap() = vec![view_pane("%1", "", "codex")];

        run_view_tick(&mut env);

        assert!(env.options.lock().unwrap().is_empty());
    }

    #[test]
    fn test_an_empty_pane_listing_is_a_tmux_failure() {
        let mut env = view_tick_env();
        *env.panes.lock().unwrap() = Vec::new();

        run_view_tick(&mut env);

        assert!(env.options.lock().unwrap().is_empty());
        assert!(env.state.signature.is_none());
        assert!(env.state.labels.is_empty());
    }

    // ---- job names (same Python file) --------------------------------------

    fn named_engine(job_id: &str, name: &str) -> EngineSession {
        EngineSession {
            pid: 1,
            job_id: job_id.to_string(),
            session_id: "s".to_string(),
            socket_path: "/tmp/s".to_string(),
            cwd: "/repo".to_string(),
            status: "idle".to_string(),
            waiting_for: String::new(),
            status_updated_at: 0.0,
            name: name.to_string(),
        }
    }

    #[allow(clippy::type_complexity)]
    fn name_wire(
        jobs: HashMap<String, String>,
        engines: HashMap<String, EngineSession>,
    ) -> (testhook::Guard, Arc<Mutex<Vec<(String, String)>>>) {
        let started: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let started_sink = Arc::clone(&started);
        let hook = Hook {
            cb_job_id_for_pane: Some(Arc::new(move |pane| jobs.get(pane).cloned())),
            cb_engine_session_for_job: Some(Arc::new(move |job| engines.get(job).cloned())),
            ensure_job_named: Some(Arc::new(move |job, name| {
                started_sink
                    .lock()
                    .unwrap()
                    .push((job.to_string(), name.to_string()))
            })),
            ..Default::default()
        };
        (testhook::install(hook), started)
    }

    fn name_members(pane: &str, cli: &str, member: &str) -> Vec<(String, Map<String, Value>)> {
        let mut row = Map::new();
        row.insert("pane".to_string(), Value::from(pane));
        row.insert("cli".to_string(), Value::from(cli));
        vec![(member.to_string(), row)]
    }

    #[test]
    fn test_a_placeholder_named_member_job_is_renamed_once() {
        // A pane adopted into a team (duo/squad/resume) was minted before it
        // carried tags, so its job keeps `hive-<pane>`.
        let (_guard, started) = name_wire(
            HashMap::from([("%183".to_string(), "485865b2".to_string())]),
            HashMap::from([("485865b2".to_string(), named_engine("485865b2", "hive-183"))]),
        );
        let mut state = ClaudeTickState::default();
        let members = name_members("%183", "claude", "worker");

        _claude_name_tick(&members, "honey", &mut state);
        _claude_name_tick(&members, "honey", &mut state);

        assert_eq!(
            *started.lock().unwrap(),
            vec![("485865b2".to_string(), "honey.worker".to_string())]
        );
    }

    #[test]
    fn test_an_already_named_job_is_left_alone() {
        let (_guard, started) = name_wire(
            HashMap::from([("%183".to_string(), "485865b2".to_string())]),
            HashMap::from([(
                "485865b2".to_string(),
                named_engine("485865b2", "honey.worker"),
            )]),
        );

        _claude_name_tick(
            &name_members("%183", "claude", "worker"),
            "honey",
            &mut ClaudeTickState::default(),
        );

        assert!(started.lock().unwrap().is_empty());
    }

    #[test]
    fn test_an_asleep_engine_is_retried_on_a_later_tick() {
        // No entry means parked or gone — not a job that needs no rename.
        let mut state = ClaudeTickState::default();
        let members = name_members("%183", "claude", "worker");
        {
            let (_guard, _started) = name_wire(
                HashMap::from([("%183".to_string(), "485865b2".to_string())]),
                HashMap::new(),
            );
            _claude_name_tick(&members, "honey", &mut state);
            assert!(state.named.is_empty());
        }

        let (_guard, _started) = name_wire(
            HashMap::from([("%183".to_string(), "485865b2".to_string())]),
            HashMap::from([("485865b2".to_string(), named_engine("485865b2", "hive-183"))]),
        );
        _claude_name_tick(&members, "honey", &mut state);
        assert_eq!(state.named, HashSet::from(["485865b2".to_string()]));
    }

    #[test]
    fn test_non_claude_members_are_not_renamed() {
        let (_guard, started) = name_wire(
            HashMap::from([("%184".to_string(), "job".to_string())]),
            HashMap::new(),
        );

        _claude_name_tick(
            &name_members("%184", "grok", "validator"),
            "honey",
            &mut ClaudeTickState::default(),
        );

        assert!(started.lock().unwrap().is_empty());
    }

    // ---- test_hived_daemon_cleanup.py --------------------------------------

    /// Daemon keys on disk; records emit/drop/kill call order.
    struct ReapEnv {
        calls: Arc<Mutex<Vec<String>>>,
        keys: Arc<Mutex<Vec<String>>>,
        tmp: tempfile::TempDir,
        _guard: testhook::Guard,
    }

    fn reap_env(pane_alive: bool) -> ReapEnv {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path().join(".hive"));
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let keys: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let keys_src = Arc::clone(&keys);
        let socket_dir = tmp.path().to_path_buf();
        let kill_sink = Arc::clone(&calls);
        let drop_sink = Arc::clone(&calls);
        let emit_sink = Arc::clone(&calls);
        let hook = Hook {
            gl_list_daemon_keys: Some(Arc::new(move || keys_src.lock().unwrap().clone())),
            gl_socket_path_for_key: Some(Arc::new(move |key| {
                socket_dir.join(format!("{key}.sock"))
            })),
            gl_kill_daemon_key: Some(Arc::new(move |key| {
                kill_sink.lock().unwrap().push(format!("kill {key}"))
            })),
            gl_pool_drop_key: Some(Arc::new(move |key| {
                drop_sink.lock().unwrap().push(format!("drop {key}"))
            })),
            notify_debug_emit: Some(Arc::new(move |ws, event, fields| {
                let mut map = Map::new();
                for (key, value) in fields {
                    map.insert(key.to_string(), value.clone());
                }
                emit_sink.lock().unwrap().push(format!(
                    "emit {ws} {event} {}",
                    serde_json::to_string(&Value::Object(map)).unwrap()
                ))
            })),
            is_pane_alive: Some(Arc::new(move |_pane| pane_alive)),
            ..Default::default()
        };
        ReapEnv {
            calls,
            keys,
            tmp,
            _guard: testhook::install(hook),
        }
    }

    fn write_pidfile(tmp: &Path, key: &str, age_seconds: f64) {
        let pidfile = tmp.join(format!("{key}.pid"));
        fs::write(&pidfile, "12345").unwrap();
        backdate(&pidfile, age_seconds);
    }

    #[test]
    fn test_cleanup_skips_live_pane() {
        let env = reap_env(true);
        *env.keys.lock().unwrap() = vec!["p4".to_string()];

        _cleanup_dead_daemons("/tmp/ws");

        assert!(env.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn test_cleanup_reaps_dead_pane_and_logs_before_kill() {
        let env = reap_env(false);
        *env.keys.lock().unwrap() = vec!["p4".to_string()];

        _cleanup_dead_daemons("/tmp/ws");

        assert_eq!(
            *env.calls.lock().unwrap(),
            vec![
                "emit /tmp/ws daemon.reap {\"key\":\"p4\"}".to_string(),
                // dropped first so a dying grok stdio client cannot
                // auto-spawn a replacement leader
                "drop p4".to_string(),
                "kill p4".to_string(),
            ]
        );
    }

    #[test]
    fn test_cleanup_member_daemon_reaped_when_registry_lists_no_such_member() {
        let env = reap_env(true);
        *env.keys.lock().unwrap() = vec!["m-honey.rex".to_string()];
        write_pidfile(env.tmp.path(), "m-honey.rex", 999.0);
        let mut other = Map::new();
        other.insert("name".to_string(), Value::from("other"));
        other.insert("cli".to_string(), Value::from("grok"));
        assert_eq!(
            crate::registry::record_team("honey", "/ws", "1.0", &[other], "").unwrap(),
            "written"
        );

        _cleanup_dead_daemons("/tmp/ws");

        assert_eq!(
            *env.calls.lock().unwrap(),
            vec![
                "emit /tmp/ws daemon.reap {\"key\":\"m-honey.rex\"}".to_string(),
                "drop m-honey.rex".to_string(),
                "kill m-honey.rex".to_string(),
            ]
        );
    }

    #[test]
    fn test_cleanup_member_daemon_kept_while_registry_lists_it() {
        let env = reap_env(true);
        *env.keys.lock().unwrap() = vec!["m-honey.rex".to_string()];
        write_pidfile(env.tmp.path(), "m-honey.rex", 999.0);
        let mut rex = Map::new();
        rex.insert("name".to_string(), Value::from("rex"));
        rex.insert("cli".to_string(), Value::from("grok"));
        assert_eq!(
            crate::registry::record_team("honey", "/ws", "1.0", &[rex], "").unwrap(),
            "written"
        );

        _cleanup_dead_daemons("/tmp/ws");

        assert!(env.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn test_cleanup_member_daemon_survives_unreadable_registry() {
        // A corrupt entry is not proof of absence — never reap on a bad read.
        let env = reap_env(true);
        *env.keys.lock().unwrap() = vec!["m-honey.rex".to_string()];
        write_pidfile(env.tmp.path(), "m-honey.rex", 999.0);
        let path = crate::registry::entry_path("honey").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not json").unwrap();

        _cleanup_dead_daemons("/tmp/ws");

        assert!(env.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn test_cleanup_member_daemon_missing_registry_reaps_after_grace() {
        let env = reap_env(true);
        *env.keys.lock().unwrap() = vec!["m-honey.rex".to_string()];

        // newborn: inside the grace window, spawn registration may be in
        // flight
        write_pidfile(env.tmp.path(), "m-honey.rex", 5.0);
        _cleanup_dead_daemons("/tmp/ws");
        assert!(env.calls.lock().unwrap().is_empty());

        // past the grace window with no registry entry: orphan
        write_pidfile(env.tmp.path(), "m-honey.rex", 999.0);
        _cleanup_dead_daemons("/tmp/ws");
        assert!(env
            .calls
            .lock()
            .unwrap()
            .contains(&"kill m-honey.rex".to_string()));
    }

    // ---- codex shared-daemon supervisor (same Python file) -----------------

    #[derive(Clone)]
    struct SuperState {
        panes: Vec<(String, String, String)>, // pane_id, agent, cli
        recorded: Vec<String>,
        threads: HashMap<String, String>,
        daemon_alive: bool,
        spawn_ok: bool,
        cli_process: HashMap<String, String>, // pane -> live CLI name
        pane_command: HashMap<String, String>,
    }

    /// Baseline supervisor world: one live codex member, healthy daemon.
    fn super_state() -> SuperState {
        SuperState {
            panes: vec![("%1".to_string(), "val".to_string(), "codex".to_string())],
            recorded: vec!["%1".to_string()],
            threads: HashMap::from([("%1".to_string(), "tid-1".to_string())]),
            daemon_alive: true,
            spawn_ok: true,
            cli_process: HashMap::from([("%1".to_string(), "codex".to_string())]),
            pane_command: HashMap::from([("%1".to_string(), "zsh".to_string())]),
        }
    }

    fn super_env(state: SuperState) -> (testhook::Guard, Arc<Mutex<Vec<String>>>) {
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(state);
        let s = Arc::clone(&state);
        let list_panes = move || -> Vec<PaneInfo> {
            s.panes
                .iter()
                .map(|(pane_id, _, _)| PaneInfo {
                    pane_id: pane_id.clone(),
                    ..Default::default()
                })
                .collect()
        };
        let s = Arc::clone(&state);
        let team_agents = move |_name: &str| -> Result<Team> {
            let agents = s
                .panes
                .iter()
                .filter(|(_, agent, _)| !agent.is_empty())
                .map(|(pane, agent, cli)| fake_agent(agent, pane, cli))
                .collect();
            Ok(fake_team("t", agents))
        };
        let clear_sink = Arc::clone(&calls);
        let drop_sink = Arc::clone(&calls);
        let spawn_sink = Arc::clone(&calls);
        let send_sink = Arc::clone(&calls);
        let emit_sink = Arc::clone(&calls);
        let s_recorded = Arc::clone(&state);
        let s_threads = Arc::clone(&state);
        let s_alive = Arc::clone(&state);
        let s_spawn = Arc::clone(&state);
        let s_cli = Arc::clone(&state);
        let s_cmd = Arc::clone(&state);
        let hook = Hook {
            list_panes_all: Some(Arc::new(list_panes)),
            cas_list_recorded_panes: Some(Arc::new(move || s_recorded.recorded.clone())),
            cas_clear_pane_thread: Some(Arc::new(move |pane| {
                clear_sink.lock().unwrap().push(format!("clear {pane}"))
            })),
            cas_thread_id_for_pane: Some(Arc::new(move |pane| {
                s_threads.threads.get(pane).cloned()
            })),
            cas_daemon_alive: Some(Arc::new(move || s_alive.daemon_alive)),
            cas_drop_client: Some(Arc::new(move || {
                drop_sink.lock().unwrap().push("drop_client".to_string())
            })),
            cas_spawn_daemon: Some(Arc::new(move || {
                spawn_sink.lock().unwrap().push("spawn".to_string());
                s_spawn.spawn_ok
            })),
            team_load: Some(Arc::new(team_agents)),
            detect_cli_process_for_pane: Some(Arc::new(move |pane| {
                s_cli
                    .cli_process
                    .get(pane)
                    .and_then(|name| crate::agent_cli::get_profile(name))
            })),
            display_value: Some(Arc::new(move |pane, _fmt| {
                Some(s_cmd.pane_command.get(pane).cloned().unwrap_or_default())
            })),
            send_keys: Some(Arc::new(move |pane, text| {
                send_sink
                    .lock()
                    .unwrap()
                    .push(format!("send {pane} {text}"))
            })),
            notify_debug_emit: Some(Arc::new(move |_ws, event, fields| {
                let mut map = Map::new();
                for (key, value) in fields {
                    map.insert(key.to_string(), value.clone());
                }
                emit_sink.lock().unwrap().push(format!(
                    "emit {event} {}",
                    serde_json::to_string(&Value::Object(map)).unwrap()
                ))
            })),
            ..Default::default()
        };
        (testhook::install(hook), calls)
    }

    #[test]
    fn test_supervisor_healthy_world_does_nothing() {
        let (_guard, calls) = super_env(super_state());
        _codex_supervisor_tick("/tmp/ws", "t");
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn test_supervisor_prunes_records_of_dead_panes() {
        let mut state = super_state();
        state.recorded = vec!["%1".to_string(), "%dead".to_string()];
        let (_guard, calls) = super_env(state);
        _codex_supervisor_tick("/tmp/ws", "t");
        let calls = calls.lock().unwrap();
        assert!(calls.contains(&"clear %dead".to_string()));
        assert!(!calls.contains(&"clear %1".to_string()));
    }

    #[test]
    fn test_supervisor_leaves_daemon_alone_without_codex_members() {
        // Machine-level shared daemon: a team with no live codex member
        // must not respawn (or otherwise touch) it — other teams may be
        // using it.
        let mut state = super_state();
        state.panes = vec![("%9".to_string(), "w".to_string(), "claude".to_string())];
        state.recorded = Vec::new();
        state.daemon_alive = false;
        let (_guard, calls) = super_env(state);
        _codex_supervisor_tick("/tmp/ws", "t");
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn test_supervisor_respawns_dead_daemon_with_live_member() {
        let mut state = super_state();
        state.daemon_alive = false;
        let (_guard, calls) = super_env(state);
        _codex_supervisor_tick("/tmp/ws", "t");
        let calls = calls.lock().unwrap();
        // stale client must reconnect post-respawn
        assert!(calls.contains(&"drop_client".to_string()));
        assert!(calls.contains(&"spawn".to_string()));
        assert!(calls.contains(&"emit codex.daemon.respawn {\"ok\":true}".to_string()));
    }

    #[test]
    fn test_supervisor_reattaches_retained_shell() {
        let mut state = super_state();
        state.cli_process = HashMap::new(); // CLI exited; pane keeps its shell
        let (_guard, calls) = super_env(state);
        _codex_supervisor_tick("/tmp/ws", "t");
        let calls = calls.lock().unwrap();
        assert!(calls.contains(&"send %1 hive codex resume tid-1".to_string()));
        assert!(calls.contains(
            &"emit codex.member.reattach {\"pane\":\"%1\",\"agent\":\"val\",\"thread\":\"tid-1\"}"
                .to_string()
        ));
    }

    #[test]
    fn test_supervisor_reattach_respects_cooldown() {
        let mut state = super_state();
        state.cli_process = HashMap::new();
        let (_guard, calls) = super_env(state);
        _codex_supervisor_tick("/tmp/ws", "t");
        _codex_supervisor_tick("/tmp/ws", "t");
        let sends = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with("send "))
            .count();
        assert_eq!(sends, 1); // one attempt per cooldown window
    }

    #[test]
    fn test_supervisor_never_types_over_a_live_cli() {
        let (_guard, calls) = super_env(super_state());
        _codex_supervisor_tick("/tmp/ws", "t");
        assert!(!calls.lock().unwrap().iter().any(|c| c.starts_with("send ")));
    }

    #[test]
    fn test_supervisor_never_types_into_a_non_shell() {
        let mut state = super_state();
        state.cli_process = HashMap::new();
        state.pane_command = HashMap::from([("%1".to_string(), "vim".to_string())]);
        let (_guard, calls) = super_env(state);
        _codex_supervisor_tick("/tmp/ws", "t");
        assert!(!calls.lock().unwrap().iter().any(|c| c.starts_with("send ")));
    }

    #[test]
    fn test_supervisor_skips_member_without_record() {
        let mut state = super_state();
        state.cli_process = HashMap::new();
        state.threads = HashMap::new();
        let (_guard, calls) = super_env(state);
        _codex_supervisor_tick("/tmp/ws", "t");
        assert!(!calls.lock().unwrap().iter().any(|c| c.starts_with("send ")));
    }

    // ---- test_hived_idle_notify.py -----------------------------------------

    const WINDOW: &str = "team-a:1";
    const WINDOW_B: &str = "team-a:2";

    struct IdleBusyMonitor {
        busy_panes: HashSet<String>,
        last_output_ages: HashMap<String, f64>,
    }

    impl OutputMonitor for IdleBusyMonitor {
        fn is_busy(&self, pane_id: &str, threshold_seconds: f64) -> bool {
            if let Some(age) = self.last_output_ages.get(pane_id) {
                return *age <= threshold_seconds;
            }
            self.busy_panes.contains(pane_id)
        }
        fn last_output_age(&self, pane_id: &str) -> Option<f64> {
            self.last_output_ages.get(pane_id).copied()
        }
    }

    fn bmon(busy: &[&str]) -> IdleBusyMonitor {
        IdleBusyMonitor {
            busy_panes: busy.iter().map(|s| s.to_string()).collect(),
            last_output_ages: HashMap::new(),
        }
    }

    fn bmon_ages(ages: &[(&str, f64)]) -> IdleBusyMonitor {
        IdleBusyMonitor {
            busy_panes: HashSet::new(),
            last_output_ages: ages.iter().map(|(p, a)| (p.to_string(), *a)).collect(),
        }
    }

    type Cleanup = (String, Vec<String>, String, bool, String, String);

    struct IdleSetup {
        calls: Arc<Mutex<Vec<(String, String)>>>,
        cleanups: Arc<Mutex<Vec<Cleanup>>>,
        active_window: Arc<Mutex<String>>,
        panes: Arc<Mutex<Vec<String>>>,
        _guard: testhook::Guard,
    }

    fn idle_setup(
        panes: &[&str],
        active_window: &str,
        pane_windows: &[(&str, &str)],
        plugin_enabled: bool,
        notify_suppressed: bool,
        window_options: &[((&str, &str), &str)],
    ) -> IdleSetup {
        let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let cleanups: Arc<Mutex<Vec<Cleanup>>> = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(Mutex::new(active_window.to_string()));
        let panes = Arc::new(Mutex::new(
            panes.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
        ));
        let pane_window_map: HashMap<String, String> = pane_windows
            .iter()
            .map(|(p, w)| (p.to_string(), w.to_string()))
            .collect();
        let window_option_map: HashMap<(String, String), String> = window_options
            .iter()
            .map(|((w, k), v)| ((w.to_string(), k.to_string()), v.to_string()))
            .collect();

        let panes_src = Arc::clone(&panes);
        let active_src = Arc::clone(&active);
        let calls_sink = Arc::clone(&calls);
        let cleanups_sink = Arc::clone(&cleanups);
        let hook = Hook {
            idle_notify_agent_panes: Some(Arc::new(move |_team| panes_src.lock().unwrap().clone())),
            get_most_recent_client_window: Some(Arc::new(move |_session| {
                Some(active_src.lock().unwrap().clone())
            })),
            get_pane_window_target: Some(Arc::new(move |pane| {
                Some(
                    pane_window_map
                        .get(pane)
                        .cloned()
                        .unwrap_or_else(|| WINDOW.to_string()),
                )
            })),
            get_window_option: Some(Arc::new(move |window, key| {
                window_option_map
                    .get(&(window.to_string(), key.to_string()))
                    .cloned()
            })),
            notify_ui_notify: Some(Arc::new(move |message, pane, _ws| {
                calls_sink
                    .lock()
                    .unwrap()
                    .push((message.to_string(), pane.to_string()));
                (notify_suppressed, None)
            })),
            clear_stale_notify: Some(Arc::new(
                move |window, panes, token, remove_attention, source, workspace| {
                    cleanups_sink.lock().unwrap().push((
                        window.to_string(),
                        panes.to_vec(),
                        token.to_string(),
                        remove_attention,
                        source.to_string(),
                        workspace.to_string(),
                    ))
                },
            )),
            is_plugin_enabled: Some(Arc::new(move |_name| plugin_enabled)),
            transcript_progressed_recently: Some(Arc::new(|_pane, _threshold| None)),
            notify_debug_emit: Some(Arc::new(|_ws, _event, _fields| {})),
            ..Default::default()
        };
        IdleSetup {
            calls,
            cleanups,
            active_window: active,
            panes,
            _guard: testhook::install(hook),
        }
    }

    fn idle_setup_default() -> IdleSetup {
        idle_setup(&["%1"], "", &[], true, false, &[])
    }

    fn idle_tick(state: &mut HashMap<String, IdleRecord>, monitor: &IdleBusyMonitor, now: f64) {
        _idle_notify_tick("team-a", "dev", state, Some(monitor), now, "", None, None);
    }

    fn idle_tick_dbg(
        state: &mut HashMap<String, IdleRecord>,
        monitor: &IdleBusyMonitor,
        now: f64,
        debug_state: &mut NotifyDebugState,
    ) {
        _idle_notify_tick(
            "team-a",
            "dev",
            state,
            Some(monitor),
            now,
            "",
            Some(debug_state),
            None,
        );
    }

    fn seeded(last_busy_ts: f64, notified: bool, seen_since_fire: bool) -> IdleRecord {
        IdleRecord::new(last_busy_ts, notified, seen_since_fire)
    }

    #[test]
    fn test_idle_notify_first_seen_window_is_already_seen_until_new_output() {
        let env = idle_setup_default();
        let mut state = HashMap::new();

        idle_tick(&mut state, &bmon(&[]), 100.0);
        idle_tick(&mut state, &bmon(&[]), 106.0);

        assert!(env.calls.lock().unwrap().is_empty());
        assert_eq!(
            state,
            HashMap::from([(WINDOW.to_string(), seeded(100.0, true, true))])
        );
    }

    #[test]
    fn test_idle_notify_first_seen_busy_window_can_notify_after_it_goes_idle() {
        let env = idle_setup_default();
        let mut state = HashMap::new();

        idle_tick(&mut state, &bmon(&["%1"]), 100.0);
        idle_tick(&mut state, &bmon(&[]), 104.9);
        idle_tick(&mut state, &bmon(&[]), 105.0);

        assert_eq!(
            *env.calls.lock().unwrap(),
            vec![(IDLE_NOTIFY_MESSAGE.to_string(), "%1".to_string())]
        );
        assert!(state[WINDOW].notified);
    }

    #[test]
    fn test_idle_notify_fires_once_after_threshold() {
        let env = idle_setup_default();
        let mut state = HashMap::from([(WINDOW.to_string(), seeded(95.0, false, true))]);

        idle_tick(&mut state, &bmon(&[]), 100.0);
        idle_tick(&mut state, &bmon(&[]), 101.0);

        assert_eq!(
            *env.calls.lock().unwrap(),
            vec![(IDLE_NOTIFY_MESSAGE.to_string(), "%1".to_string())]
        );
        assert!(state[WINDOW].notified);
    }

    #[test]
    fn test_idle_notify_suppressed_result_counts_as_seen() {
        let env = idle_setup(&["%1"], "", &[], true, true, &[]);
        let mut state = HashMap::from([(WINDOW.to_string(), seeded(95.0, false, true))]);

        idle_tick(&mut state, &bmon(&[]), 100.0);
        idle_tick(&mut state, &bmon(&[]), 101.0);

        assert_eq!(
            *env.calls.lock().unwrap(),
            vec![(IDLE_NOTIFY_MESSAGE.to_string(), "%1".to_string())]
        );
        assert!(state[WINDOW].notified);
        assert!(state[WINDOW].seen_since_fire);
    }

    #[test]
    fn test_idle_notify_busy_pane_resets_timer() {
        let env = idle_setup_default();
        let mut state = HashMap::from([(WINDOW.to_string(), seeded(80.0, true, true))]);

        idle_tick(&mut state, &bmon(&["%1"]), 100.0);

        assert!(env.calls.lock().unwrap().is_empty());
        let mut expected = seeded(100.0, false, true);
        expected.last_busy_pane = Some("%1".to_string());
        assert_eq!(state, HashMap::from([(WINDOW.to_string(), expected)]));
    }

    #[test]
    fn test_idle_notify_active_window_counts_as_seen() {
        let env = idle_setup(&["%1"], WINDOW, &[], true, false, &[]);
        let mut state = HashMap::from([(WINDOW.to_string(), seeded(80.0, false, true))]);

        idle_tick(&mut state, &bmon(&[]), 100.0);

        assert!(env.calls.lock().unwrap().is_empty());
        assert_eq!(
            state,
            HashMap::from([(WINDOW.to_string(), seeded(100.0, true, true))])
        );
    }

    #[test]
    fn test_idle_notify_does_not_refire_until_user_sees_target() {
        let env = idle_setup_default();
        let mut state = HashMap::from([(WINDOW.to_string(), seeded(95.0, false, true))]);

        idle_tick(&mut state, &bmon(&[]), 101.0);
        idle_tick(&mut state, &bmon(&["%1"]), 105.0);
        idle_tick(&mut state, &bmon(&[]), 115.0);

        assert_eq!(
            *env.calls.lock().unwrap(),
            vec![(IDLE_NOTIFY_MESSAGE.to_string(), "%1".to_string())]
        );
        assert!(state[WINDOW].notified);
        assert!(!state[WINDOW].seen_since_fire);
    }

    #[test]
    fn test_idle_notify_refires_after_user_sees_target_and_new_round() {
        let env = idle_setup(&["%1"], WINDOW, &[], true, false, &[]);
        let mut state = HashMap::from([(WINDOW.to_string(), seeded(80.0, true, false))]);

        idle_tick(&mut state, &bmon(&[]), 100.0);
        *env.active_window.lock().unwrap() = String::new();
        idle_tick(&mut state, &bmon(&["%1"]), 105.0);
        idle_tick(&mut state, &bmon(&[]), 115.0);

        assert_eq!(
            *env.calls.lock().unwrap(),
            vec![(IDLE_NOTIFY_MESSAGE.to_string(), "%1".to_string())]
        );
        assert!(state[WINDOW].notified);
        assert!(!state[WINDOW].seen_since_fire);
    }

    #[test]
    fn test_idle_notify_multi_pane_window_waits_for_every_pane_idle() {
        let env = idle_setup(&["%1", "%2"], "", &[], true, false, &[]);
        let mut state = HashMap::new();

        idle_tick(&mut state, &bmon(&[]), 100.0);
        idle_tick(&mut state, &bmon(&["%1"]), 101.0);
        idle_tick(&mut state, &bmon(&[]), 103.0);
        idle_tick(&mut state, &bmon(&["%2"]), 104.0);
        idle_tick(&mut state, &bmon(&[]), 108.9);
        assert!(env.calls.lock().unwrap().is_empty());
        idle_tick(&mut state, &bmon(&[]), 109.0);

        assert_eq!(
            *env.calls.lock().unwrap(),
            vec![(IDLE_NOTIFY_MESSAGE.to_string(), "%2".to_string())]
        );
        assert!(state[WINDOW].notified);
    }

    #[test]
    fn test_idle_notify_tracks_windows_independently() {
        let env = idle_setup(
            &["%1", "%2"],
            "",
            &[("%1", WINDOW), ("%2", WINDOW_B)],
            true,
            false,
            &[],
        );
        let mut state = HashMap::from([
            (WINDOW.to_string(), seeded(95.0, false, true)),
            (WINDOW_B.to_string(), seeded(99.9, false, true)),
        ]);

        idle_tick(&mut state, &bmon(&[]), 101.0);

        assert_eq!(
            *env.calls.lock().unwrap(),
            vec![(IDLE_NOTIFY_MESSAGE.to_string(), "%1".to_string())]
        );
        assert!(state[WINDOW].notified);
        assert!(!state[WINDOW_B].notified);
    }

    #[test]
    fn test_idle_notify_prunes_removed_windows_after_grace() {
        let env = idle_setup(&["%2"], "", &[("%2", WINDOW_B)], true, false, &[]);
        let mut state = HashMap::from([
            (WINDOW.to_string(), seeded(80.0, true, true)),
            (WINDOW_B.to_string(), seeded(100.0, true, true)),
        ]);

        for i in 0..IDLE_NOTIFY_MISSING_PRUNE_TICKS {
            idle_tick(&mut state, &bmon(&[]), 101.0 + i as f64);
            if i < IDLE_NOTIFY_MISSING_PRUNE_TICKS - 1 {
                assert!(state.contains_key(WINDOW));
            }
        }

        assert!(env.calls.lock().unwrap().is_empty());
        let mut keys: Vec<&String> = state.keys().collect();
        keys.sort();
        assert_eq!(keys, vec![WINDOW_B]);
    }

    #[test]
    fn test_idle_notify_transient_pane_query_failure_does_not_reset_state() {
        let env = idle_setup_default();
        let mut state = HashMap::new();

        idle_tick(&mut state, &bmon(&["%1"]), 100.0);
        idle_tick(&mut state, &bmon(&[]), 106.0);
        assert_eq!(
            *env.calls.lock().unwrap(),
            vec![(IDLE_NOTIFY_MESSAGE.to_string(), "%1".to_string())]
        );
        assert!(!state[WINDOW].seen_since_fire);

        *env.panes.lock().unwrap() = Vec::new();
        idle_tick(&mut state, &bmon(&[]), 107.0);
        idle_tick(&mut state, &bmon(&[]), 108.0);
        *env.panes.lock().unwrap() = vec!["%1".to_string()];

        assert!(!state[WINDOW].seen_since_fire);
        idle_tick(&mut state, &bmon(&[]), 120.0);
        idle_tick(&mut state, &bmon(&[]), 130.0);

        assert_eq!(
            *env.calls.lock().unwrap(),
            vec![(IDLE_NOTIFY_MESSAGE.to_string(), "%1".to_string())]
        );
    }

    #[test]
    fn test_idle_notify_existing_window_flash_keeps_rebuilt_state_locked() {
        let env = idle_setup(
            &["%1"],
            "",
            &[],
            true,
            false,
            &[((WINDOW, "hive-notify-token"), "%1:old-fire")],
        );
        let mut state = HashMap::new();

        idle_tick(&mut state, &bmon(&["%1"]), 100.0);
        idle_tick(&mut state, &bmon(&[]), 106.0);

        assert!(env.calls.lock().unwrap().is_empty());
        assert!(state[WINDOW].notified);
        assert!(!state[WINDOW].seen_since_fire);
    }

    #[test]
    fn test_idle_notify_clears_notify_when_target_window_is_selected() {
        let env = idle_setup(
            &["%1"],
            WINDOW,
            &[],
            true,
            false,
            &[((WINDOW, "hive-notify-token"), "%1:selected-fire")],
        );
        let mut state = HashMap::new();

        idle_tick(&mut state, &bmon(&[]), 100.0);

        assert!(env.calls.lock().unwrap().is_empty());
        assert_eq!(
            *env.cleanups.lock().unwrap(),
            vec![(
                WINDOW.to_string(),
                vec!["%1".to_string()],
                "%1:selected-fire".to_string(),
                false,
                "hived.active_window".to_string(),
                String::new(),
            )]
        );
        assert!(state[WINDOW].notified);
        assert!(state[WINDOW].seen_since_fire);
    }

    #[test]
    fn test_idle_notify_reconciles_selected_notify_even_when_plugin_disabled() {
        let env = idle_setup(
            &["%1"],
            WINDOW,
            &[],
            false,
            false,
            &[((WINDOW, "hive-notify-token"), "%1:selected-fire")],
        );
        let mut state = HashMap::from([(WINDOW.to_string(), seeded(80.0, false, true))]);

        idle_tick(&mut state, &bmon(&[]), 100.0);

        assert!(env.calls.lock().unwrap().is_empty());
        assert_eq!(
            *env.cleanups.lock().unwrap(),
            vec![(
                WINDOW.to_string(),
                vec!["%1".to_string()],
                "%1:selected-fire".to_string(),
                false,
                "hived.active_window".to_string(),
                String::new(),
            )]
        );
        assert!(state.is_empty());
    }

    #[test]
    fn test_idle_notify_skips_and_clears_state_when_plugin_disabled() {
        let env = idle_setup(&["%1"], "", &[], false, false, &[]);
        let mut state = HashMap::from([(WINDOW.to_string(), seeded(80.0, false, true))]);

        idle_tick(&mut state, &bmon(&[]), 200.0);

        assert!(env.calls.lock().unwrap().is_empty());
        assert!(state.is_empty());
    }

    #[test]
    fn test_active_window_switch_does_not_rearm_for_seen_output() {
        // Output the user already saw on the active window must not be
        // treated as fresh activity right after they switch away.
        let env = idle_setup(&["%1"], WINDOW, &[("%1", WINDOW)], true, false, &[]);
        let mut state = HashMap::new();
        let mut debug_state = NotifyDebugState::default();

        // t=100: WINDOW is active and saw real output 0.5s ago.
        idle_tick_dbg(
            &mut state,
            &bmon_ages(&[("%1", 0.5)]),
            100.0,
            &mut debug_state,
        );
        assert!(state[WINDOW].notified);

        // t=101: user switches to OTHER. Same output now 1.5s old; monitor
        // still reports busy because it's within the 3s threshold.
        *env.active_window.lock().unwrap() = "team-a:99".to_string();
        idle_tick_dbg(
            &mut state,
            &bmon_ages(&[("%1", 1.5)]),
            101.0,
            &mut debug_state,
        );
        assert!(
            state[WINDOW].notified,
            "seen output must not rearm notified"
        );

        // t=106.5: 5s past last_busy_ts and beyond the busy threshold; no
        // fire because the boundary check prevented the rearm above.
        idle_tick_dbg(
            &mut state,
            &bmon_ages(&[("%1", 6.5)]),
            106.5,
            &mut debug_state,
        );
        assert!(env.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn test_active_window_switch_still_rearms_for_post_switch_output() {
        // Dual of the regression above: real new output produced AFTER the
        // user switches away must still flag busy and rearm idle notify.
        let env = idle_setup(&["%1"], WINDOW, &[("%1", WINDOW)], true, false, &[]);
        let mut state = HashMap::new();
        let mut debug_state = NotifyDebugState::default();

        // Active and quiet — set up baseline.
        idle_tick_dbg(
            &mut state,
            &bmon_ages(&[("%1", 5.0)]),
            100.0,
            &mut debug_state,
        );

        // User switches to OTHER at t=101. inactive_at[WINDOW] = 101.
        *env.active_window.lock().unwrap() = "team-a:99".to_string();
        idle_tick_dbg(
            &mut state,
            &bmon_ages(&[("%1", 6.0)]),
            101.0,
            &mut debug_state,
        );

        // t=104: claude emits brand-new output 0.5s old. inactive_age=3.0,
        // output_age=0.5 — fresh post-switch activity, must rearm.
        idle_tick_dbg(
            &mut state,
            &bmon_ages(&[("%1", 0.5)]),
            104.0,
            &mut debug_state,
        );
        assert!(!state[WINDOW].notified, "post-switch output must rearm");
        assert_eq!(state[WINDOW].last_busy_pane.as_deref(), Some("%1"));
    }

    #[test]
    fn test_idle_notify_agent_panes_filters_to_live_agent_roles() {
        let bindings: Vec<(String, Map<String, Value>)> = [
            ("agent-a", "agent", "%1"),
            ("terminal", "terminal", "%2"),
            ("legacy-orch", "orchestrator", "%3"),
            ("dead", "agent", "%4"),
            ("dup", "agent", "%1"),
        ]
        .iter()
        .map(|(name, role, pane)| {
            let mut row = Map::new();
            row.insert("role".to_string(), Value::from(*role));
            row.insert("pane".to_string(), Value::from(*pane));
            (name.to_string(), row)
        })
        .collect();
        let hook = Hook {
            team_member_bindings: Some(Arc::new(move |_team| Ok(bindings.clone()))),
            is_pane_alive: Some(Arc::new(|pane| pane != "%4")),
            detect_cli_process_for_pane: Some(Arc::new(|_p| claude_profile())),
            ..Default::default()
        };
        let _guard = testhook::install(hook);

        assert_eq!(_idle_notify_agent_panes("team-a"), vec!["%1".to_string()]);
    }

    // ---- test_hived_queue.py -----------------------------------------------

    fn short_workspace() -> tempfile::TempDir {
        // AF_UNIX sun_path caps near 104 bytes: the hived socket cannot live
        // under a long tmp path.
        tempfile::Builder::new()
            .prefix("hive-sq-")
            .tempdir_in("/tmp")
            .unwrap()
    }

    struct RecServer {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl HivedServerApi for RecServer {
        fn close(&self) {
            self.calls.lock().unwrap().push("server.close".to_string());
        }
        fn accept_timeout(&self, _timeout: f64) -> Option<UnixStream> {
            None
        }
    }

    struct RecMonitor {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl OutputMonitor for RecMonitor {
        fn is_busy(&self, _pane_id: &str, _threshold_seconds: f64) -> bool {
            false
        }
        fn last_output_age(&self, _pane_id: &str) -> Option<f64> {
            None
        }
        fn start(&self) {
            self.calls.lock().unwrap().push("monitor.start".to_string());
        }
        fn stop(&self) {
            self.calls.lock().unwrap().push("monitor.stop".to_string());
        }
    }

    fn json_obj(pairs: &[(&str, Value)]) -> Map<String, Value> {
        let mut map = Map::new();
        for (key, value) in pairs {
            map.insert(key.to_string(), value.clone());
        }
        map
    }

    #[test]
    fn test_serve_requests_answers_a_read_while_a_send_holds_the_transport() {
        // C1: delivery may hold the native transport for ~52s while `hive
        // team` gives up after 2s and reports "no hived". Handlers run off
        // the accept loop so the short read is answered immediately.
        let started = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let started_hook = Arc::clone(&started);
        let release_hook = Arc::clone(&release);
        let hook = Hook {
            handle_request: Some(Arc::new(move |request| {
                if request.get("action").and_then(Value::as_str) == Some("send") {
                    {
                        let (lock, cvar) = &*started_hook;
                        *lock.lock().unwrap() = true;
                        cvar.notify_all();
                    }
                    let (lock, cvar) = &*release_hook;
                    let guard = lock.lock().unwrap();
                    let _ = cvar
                        .wait_timeout_while(guard, Duration::from_secs(10), |done| !*done)
                        .unwrap();
                    return (
                        json_obj(&[("ok", Value::Bool(true)), ("slow", Value::Bool(true))]),
                        true,
                    );
                }
                (
                    json_obj(&[("ok", Value::Bool(true)), ("fast", Value::Bool(true))]),
                    true,
                )
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        let tmp = short_workspace();
        let workspace = tmp.path().to_string_lossy().to_string();
        let server = Arc::new(_open_server_socket(&workspace).unwrap());

        let ws_slow = workspace.clone();
        let slow_client =
            thread::spawn(move || _request_hived(&ws_slow, &action_payload("send"), 10.0));
        let ws_serve = workspace.clone();
        let server_serve = Arc::clone(&server);
        let serve_thread = thread::spawn(move || {
            _serve_requests(
                server_serve.as_ref(),
                &ws_serve,
                "team-a",
                "dev:3",
                "@99",
                "2026-01-01T00:00:00Z",
                2.0,
            )
        });

        {
            let (lock, cvar) = &*started;
            let guard = lock.lock().unwrap();
            let (guard, timeout) = cvar
                .wait_timeout_while(guard, Duration::from_secs(2), |s| !*s)
                .unwrap();
            assert!(!timeout.timed_out(), "slow handler never started");
            drop(guard);
        }

        let began = monotonic();
        let response = _request_hived(
            &workspace,
            &action_payload("team-runtime"),
            SOCKET_READY_TIMEOUT,
        );
        let elapsed = monotonic() - began;

        assert_eq!(
            response,
            Some(json_obj(&[
                ("ok", Value::Bool(true)),
                ("fast", Value::Bool(true))
            ]))
        );
        assert!(elapsed < 1.0, "fast read took {elapsed}s");

        {
            let (lock, cvar) = &*release;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
        let slow_response = slow_client.join().unwrap();
        assert_eq!(
            slow_response,
            Some(json_obj(&[
                ("ok", Value::Bool(true)),
                ("slow", Value::Bool(true))
            ]))
        );
        let keep_running = serve_thread.join().unwrap();
        server.close();
        _cleanup_socket_impl(&workspace);

        assert!(keep_running);
        assert!(!_requests_in_flight());
    }

    #[test]
    fn test_serve_requests_still_retires_the_loop_on_shutdown() {
        let hook = Hook {
            handle_request: Some(Arc::new(|_request| {
                (json_obj(&[("ok", Value::Bool(true))]), false)
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        let tmp = short_workspace();
        let workspace = tmp.path().to_string_lossy().to_string();
        let server = Arc::new(_open_server_socket(&workspace).unwrap());

        let ws_serve = workspace.clone();
        let server_serve = Arc::clone(&server);
        let serve_thread = thread::spawn(move || {
            _serve_requests(
                server_serve.as_ref(),
                &ws_serve,
                "team-a",
                "dev:3",
                "@99",
                "2026-01-01T00:00:00Z",
                1.0,
            )
        });

        let response = _request_hived(&workspace, &action_payload("shutdown"), 2.0);
        let keep_running = serve_thread.join().unwrap();

        assert_eq!(response, Some(json_obj(&[("ok", Value::Bool(true))])));
        assert!(!keep_running);

        _SHUTDOWN.store(false, Ordering::SeqCst);
        server.close();
        _cleanup_socket_impl(&workspace);
    }

    #[test]
    fn test_socket_alive_requires_matching_api_version() {
        let hook = Hook {
            request_ping: Some(Arc::new(|_ws| Some(json_obj(&[("ok", Value::Bool(true))])))),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        assert!(!_socket_alive("/tmp/ws"));

        testhook::update(|h| {
            h.request_ping = Some(Arc::new(|_ws| {
                Some(json_obj(&[
                    ("ok", Value::Bool(true)),
                    ("apiVersion", Value::from(HIVED_API_VERSION)),
                ]))
            }));
        });
        assert!(_socket_alive("/tmp/ws"));
    }

    #[test]
    fn test_hived_identity_matches_team_and_ignores_window() {
        assert!(!_hived_identity_matches(
            Some(&json_obj(&[
                ("ok", Value::Bool(true)),
                ("apiVersion", Value::from(HIVED_API_VERSION)),
            ])),
            "team-a",
        ));
        assert!(!_hived_identity_matches(
            Some(&json_obj(&[
                ("ok", Value::Bool(true)),
                ("apiVersion", Value::from(HIVED_API_VERSION)),
                ("team", Value::from("team-b")),
            ])),
            "team-a",
        ));
        assert!(!_hived_identity_matches(
            Some(&json_obj(&[
                ("ok", Value::Bool(true)),
                ("apiVersion", Value::from(HIVED_API_VERSION)),
                ("buildHash", Value::from("stale")),
                ("team", Value::from("team-a")),
            ])),
            "team-a",
        ));
        // The window is display, not identity: a moved/killed/recreated
        // window must not bounce a healthy hived.
        assert!(_hived_identity_matches(
            Some(&json_obj(&[
                ("ok", Value::Bool(true)),
                ("apiVersion", Value::from(HIVED_API_VERSION)),
                ("buildHash", Value::from(hived_build_hash())),
                ("team", Value::from("team-a")),
                ("tmuxWindowId", Value::from("@9")),
            ])),
            "team-a",
        ));
        assert!(_hived_identity_matches(
            Some(&json_obj(&[
                ("ok", Value::Bool(true)),
                ("apiVersion", Value::from(HIVED_API_VERSION)),
                ("buildHash", Value::from(hived_build_hash())),
                ("team", Value::from("team-a")),
            ])),
            "team-a",
        ));
    }

    #[test]
    fn test_handle_request_ping_returns_hived_identity() {
        let (response, keep_running) = _handle_request(
            "/tmp/ws",
            "team-a",
            "dev:3",
            "@99",
            "2026-04-17T00:00:00Z",
            &json_obj(&[("action", Value::from("ping"))]),
        );

        assert!(keep_running);
        let expected = json_obj(&[
            ("ok", Value::Bool(true)),
            ("apiVersion", Value::from(HIVED_API_VERSION)),
            ("buildHash", Value::from(hived_build_hash())),
            ("team", Value::from("team-a")),
            ("tmuxWindow", Value::from("dev:3")),
            ("tmuxWindowId", Value::from("@99")),
            (
                "hived",
                Value::Object(json_obj(&[
                    ("pid", Value::from(getpid())),
                    ("started_at", Value::from("2026-04-17T00:00:00Z")),
                    ("code_hash", Value::from(hived_build_hash())),
                ])),
            ),
        ]);
        assert_eq!(response, expected);
    }

    #[test]
    fn test_handle_request_connect_codex_brings_2nd_client_online() {
        let connected: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&connected);
        let hook = Hook {
            cas_connect: Some(Arc::new(move || {
                sink.lock().unwrap().push(true);
                true
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);

        let (response, keep_running) = _handle_request(
            "/tmp/ws",
            "team-a",
            "dev:3",
            "@99",
            "2026-04-17T00:00:00Z",
            &json_obj(&[("action", Value::from("connect-codex"))]),
        );

        assert!(keep_running);
        assert_eq!(
            response,
            json_obj(&[("ok", Value::Bool(true)), ("connected", Value::Bool(true))])
        );
        assert_eq!(*connected.lock().unwrap(), vec![true]);
    }

    #[test]
    fn test_handle_request_connect_grok_brings_2nd_client_online() {
        let connected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&connected);
        let hook = Hook {
            gl_connect_pane: Some(Arc::new(move |pane| {
                sink.lock().unwrap().push(pane.to_string());
                true
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);

        let (response, keep_running) = _handle_request(
            "/tmp/ws",
            "team-a",
            "dev:3",
            "@99",
            "2026-04-17T00:00:00Z",
            &json_obj(&[
                ("action", Value::from("connect-grok")),
                ("pane", Value::from("%5")),
            ]),
        );

        assert!(keep_running);
        assert_eq!(
            response,
            json_obj(&[("ok", Value::Bool(true)), ("connected", Value::Bool(true))])
        );
        assert_eq!(*connected.lock().unwrap(), vec!["%5".to_string()]);
    }

    #[test]
    fn test_start_hived_spawns_fresh_python_process() {
        // Adapted: the Rust build spawns its own binary, not `python -m`.
        let captured: Arc<Mutex<Vec<(Vec<String>, PathBuf)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        let hook = Hook {
            current_exe: Some(Arc::new(|| "/tmp/fake-python".to_string())),
            popen: Some(Arc::new(move |command, stderr_path| {
                sink.lock()
                    .unwrap()
                    .push((command.to_vec(), stderr_path.to_path_buf()));
                4321
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);

        let pid = _start_hived("/tmp/ws", "team-a", "dev:3", "@99");

        assert_eq!(pid, Some(4321));
        let captured = captured.lock().unwrap();
        assert_eq!(
            captured[0].0,
            vec![
                "/tmp/fake-python".to_string(),
                "--hived".to_string(),
                "/tmp/ws".to_string(),
                "team-a".to_string(),
                "dev:3".to_string(),
                "@99".to_string(),
            ]
        );
        assert_eq!(
            captured[0].1,
            devlog::hived_stderr_path(Path::new("/tmp/ws"))
        );
    }

    #[test]
    fn test_run_spawned_hived_ignores_sigint_and_runs_loop() {
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sigint_sink = Arc::clone(&calls);
        let loop_sink = Arc::clone(&calls);
        let hook = Hook {
            ignore_sigint: Some(Arc::new(move || {
                sigint_sink.lock().unwrap().push("sigint".to_string())
            })),
            hived_loop: Some(Arc::new(move |ws, team, window, window_id| {
                loop_sink
                    .lock()
                    .unwrap()
                    .push(format!("loop {ws} {team} {window} {window_id}"))
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);

        let exit_code = _run_spawned_hived(&[
            "--hived".to_string(),
            "/tmp/ws".to_string(),
            "team-a".to_string(),
            "dev:3".to_string(),
            "@99".to_string(),
        ]);

        assert_eq!(exit_code, 0);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "sigint".to_string(),
                "loop /tmp/ws team-a dev:3 @99".to_string()
            ]
        );
    }

    #[test]
    fn test_stale_disk_build_hash_requires_stable_changed_hash() {
        let hook = Hook {
            compute_build_hash: Some(Arc::new(|| "new-hash".to_string())),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        let mut state = ReexecState::default();
        state.last_code_check_at = 5.0;

        assert_eq!(_stale_disk_build_hash_for_reexec(&mut state, 10.0), None);
        assert_eq!(state.candidate_hash.as_deref(), Some("new-hash"));
        assert_eq!(_stale_disk_build_hash_for_reexec(&mut state, 14.9), None);
        assert_eq!(
            _stale_disk_build_hash_for_reexec(&mut state, 15.0),
            Some("new-hash".to_string())
        );
    }

    #[test]
    fn test_stale_disk_build_hash_clears_candidate_when_code_matches() {
        let hook = Hook {
            compute_build_hash: Some(Arc::new(|| hived_build_hash().to_string())),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        let mut state = ReexecState {
            last_code_check_at: 5.0,
            candidate_hash: Some("new-hash".to_string()),
        };

        assert_eq!(_stale_disk_build_hash_for_reexec(&mut state, 10.0), None);
        assert!(state.candidate_hash.is_none());
    }

    #[test]
    fn test_try_acquire_reexec_lock_returns_inheritable_lock_fd() {
        let _guard = testhook::install(Hook::default());
        let tmp = tempfile::tempdir().unwrap();
        let lock_fd = _try_acquire_reexec_lock(&tmp.path().to_string_lossy());
        let fd = lock_fd.expect("lock fd");
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, 0); // inheritable
        _release_reexec_lock_fd(lock_fd);
    }

    #[test]
    fn test_try_acquire_reexec_lock_returns_none_when_lock_is_busy() {
        let _guard = testhook::install(Hook::default());
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().to_string_lossy().to_string();
        let lock_path = _lock_path(&workspace);
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let cpath = CString::new(lock_path.as_os_str().as_bytes()).unwrap();
        let held_fd = unsafe { libc::open(cpath.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644) };
        assert!(held_fd >= 0);
        assert_eq!(unsafe { libc::flock(held_fd, libc::LOCK_EX) }, 0);

        assert_eq!(_try_acquire_reexec_lock(&workspace), None);

        unsafe {
            libc::flock(held_fd, libc::LOCK_UN);
            libc::close(held_fd);
        }
    }

    #[test]
    fn test_reexec_hived_stops_monitor_closes_socket_and_execs() {
        std::env::remove_var(_HIVED_REEXEC_LOCK_ENV);
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let lock_sink = Arc::clone(&calls);
        let release_sink = Arc::clone(&calls);
        let cleanup_sink = Arc::clone(&calls);
        let execv_sink = Arc::clone(&calls);
        let hook = Hook {
            current_exe: Some(Arc::new(|| "/tmp/fake-python".to_string())),
            try_acquire_reexec_lock: Some(Arc::new(move |workspace| {
                lock_sink.lock().unwrap().push(format!("lock {workspace}"));
                Some(42)
            })),
            release_reexec_lock_fd: Some(Arc::new(move |fd| {
                release_sink.lock().unwrap().push(format!("release {fd:?}"))
            })),
            cleanup_socket: Some(Arc::new(move |workspace| {
                cleanup_sink
                    .lock()
                    .unwrap()
                    .push(format!("cleanup {workspace}"))
            })),
            execv: Some(Arc::new(move |argv| {
                execv_sink.lock().unwrap().push(format!(
                    "execv {} env={}",
                    argv.join(" "),
                    std::env::var(_HIVED_REEXEC_LOCK_ENV).unwrap_or_default()
                ));
                ExecOutcome::Replaced
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        let server = RecServer {
            calls: Arc::clone(&calls),
        };
        let monitor: Arc<dyn OutputMonitor> = Arc::new(RecMonitor {
            calls: Arc::clone(&calls),
        });

        let replacement = _reexec_hived(
            "/ws",
            "team-a",
            "dev:3",
            "@99",
            &server,
            Some(&monitor),
            None,
        );

        assert!(replacement.is_none());
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "lock /ws".to_string(),
                "monitor.stop".to_string(),
                "server.close".to_string(),
                "cleanup /ws".to_string(),
                "execv /tmp/fake-python --hived /ws team-a dev:3 @99 env=42".to_string(),
                "release Some(42)".to_string(),
            ]
        );
        assert!(std::env::var(_HIVED_REEXEC_LOCK_ENV).is_err());
    }

    #[test]
    fn test_reexec_hived_skips_when_reexec_lock_is_busy() {
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let execv_sink = Arc::clone(&calls);
        let hook = Hook {
            try_acquire_reexec_lock: Some(Arc::new(|_workspace| None)),
            execv: Some(Arc::new(move |_argv| {
                execv_sink.lock().unwrap().push("execv".to_string());
                ExecOutcome::Replaced
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        let server = RecServer {
            calls: Arc::clone(&calls),
        };
        let monitor: Arc<dyn OutputMonitor> = Arc::new(RecMonitor {
            calls: Arc::clone(&calls),
        });

        let replacement = _reexec_hived(
            "/ws",
            "team-a",
            "dev:3",
            "@99",
            &server,
            Some(&monitor),
            None,
        );

        assert!(replacement.is_none());
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn test_reexec_hived_rebinds_and_keeps_serving_when_execv_fails() {
        // execv failing after the teardown used to punch through the loop
        // and leave the window with no hived *and* no socket.
        std::env::remove_var(_HIVED_REEXEC_LOCK_ENV);
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let release_sink = Arc::clone(&calls);
        let cleanup_sink = Arc::clone(&calls);
        let open_sink = Arc::clone(&calls);
        let open_calls = Arc::clone(&calls);
        let hook = Hook {
            current_exe: Some(Arc::new(|| "/tmp/fake-python".to_string())),
            try_acquire_reexec_lock: Some(Arc::new(|_workspace| Some(42))),
            release_reexec_lock_fd: Some(Arc::new(move |fd| {
                release_sink.lock().unwrap().push(format!("release {fd:?}"))
            })),
            cleanup_socket: Some(Arc::new(move |workspace| {
                cleanup_sink
                    .lock()
                    .unwrap()
                    .push(format!("cleanup {workspace}"))
            })),
            execv: Some(Arc::new(|_argv| {
                ExecOutcome::Failed(std::io::Error::from_raw_os_error(8))
            })),
            open_server_socket: Some(Arc::new(move |workspace| {
                open_sink.lock().unwrap().push(format!("open {workspace}"));
                Ok(Box::new(RecServer {
                    calls: Arc::clone(&open_calls),
                }) as Box<dyn HivedServerApi>)
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        let server = RecServer {
            calls: Arc::clone(&calls),
        };
        let monitor: Arc<dyn OutputMonitor> = Arc::new(RecMonitor {
            calls: Arc::clone(&calls),
        });

        let replacement = _reexec_hived(
            "/ws",
            "team-a",
            "dev:3",
            "@99",
            &server,
            Some(&monitor),
            None,
        );

        assert!(replacement.is_some());
        {
            let calls = calls.lock().unwrap();
            assert!(calls.contains(&"open /ws".to_string()));
            assert!(calls.contains(&"monitor.start".to_string()));
        }
        let installed = _get_output_busy_monitor().expect("monitor restored");
        assert!(Arc::ptr_eq(&installed, &monitor));
        assert!(std::env::var(_HIVED_REEXEC_LOCK_ENV).is_err());
        _set_output_busy_monitor(None);
    }

    #[test]
    fn test_cleanup_socket_if_owner_skips_foreign_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().to_string_lossy().to_string();
        _write_hived_owner_impl(
            &workspace,
            getpid() + 1000,
            "2026-04-28T00:00:00Z",
            "foreign",
        );
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&calls);
        let hook = Hook {
            cleanup_socket: Some(Arc::new(move |workspace| {
                sink.lock().unwrap().push(format!("cleanup {workspace}"))
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);

        _cleanup_socket_if_owner(&workspace, "mine");

        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn test_hived_loop_retires_orphan_before_idle_tick() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path().join(".hive"));
        std::env::remove_var(_HIVED_REEXEC_LOCK_ENV);
        let workspace = tmp.path().to_string_lossy().to_string();
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let events: Arc<Mutex<Vec<(String, Map<String, Value>)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let open_sink = Arc::clone(&calls);
        let open_calls = Arc::clone(&calls);
        let serve_sink = Arc::clone(&calls);
        let cleanup_sink = Arc::clone(&calls);
        let events_sink = Arc::clone(&events);
        let hook = Hook {
            open_server_socket: Some(Arc::new(move |workspace| {
                open_sink.lock().unwrap().push(format!("open {workspace}"));
                Ok(Box::new(RecServer {
                    calls: Arc::clone(&open_calls),
                }) as Box<dyn HivedServerApi>)
            })),
            write_hived_owner: Some(Arc::new(|workspace, pid, started_at, token| {
                _write_hived_owner_impl(workspace, pid, started_at, token);
                _write_hived_owner_impl(workspace, pid + 1, started_at, "foreign");
            })),
            release_reexec_lock_fd: Some(Arc::new(|_fd| {})),
            is_tmux_window_alive: Some(Arc::new(|_id| true)),
            stale_disk_build_hash: Some(Arc::new(|| None)),
            serve_requests: Some(Arc::new(move || {
                serve_sink.lock().unwrap().push("serve".to_string());
                true
            })),
            cleanup_socket: Some(Arc::new(move |workspace| {
                cleanup_sink
                    .lock()
                    .unwrap()
                    .push(format!("cleanup {workspace}"))
            })),
            make_busy_monitor: Some(Arc::new(|_session| None)),
            team_load: Some(Arc::new(|_name| anyhow::bail!("no team"))),
            gl_list_daemon_keys: Some(Arc::new(Vec::new)),
            list_panes_all: Some(Arc::new(Vec::new)),
            cb_list_recorded_panes: Some(Arc::new(Vec::new)),
            cas_list_recorded_panes: Some(Arc::new(Vec::new)),
            notify_debug_emit: Some(Arc::new(move |_ws, event, fields| {
                let mut map = Map::new();
                for (key, value) in fields {
                    map.insert(key.to_string(), value.clone());
                }
                events_sink.lock().unwrap().push((event.to_string(), map))
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);

        _hived_loop(&workspace, "team-a", "dev:3", "@99");

        let events = events.lock().unwrap();
        let retire: Vec<_> = events
            .iter()
            .filter(|(event, _)| event == "hived.retire_orphan")
            .collect();
        assert!(!retire.is_empty());
        assert_eq!(retire[0].1["currentPid"], Value::from(getpid()));
        assert_eq!(retire[0].1["socketPid"], Value::from(getpid() + 1));
        let calls = calls.lock().unwrap();
        assert!(!calls.contains(&"serve".to_string()));
        assert!(!calls.contains(&format!("cleanup {workspace}")));
        assert!(calls.contains(&"server.close".to_string()));
    }

    #[test]
    fn test_hived_loop_releases_inherited_reexec_lock_after_socket_ready() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path().join(".hive"));
        std::env::set_var(_HIVED_REEXEC_LOCK_ENV, "77");
        let workspace = tmp.path().to_string_lossy().to_string();
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let open_sink = Arc::clone(&calls);
        let open_calls = Arc::clone(&calls);
        let release_sink = Arc::clone(&calls);
        let cleanup_sink = Arc::clone(&calls);
        let hook = Hook {
            open_server_socket: Some(Arc::new(move |workspace| {
                open_sink.lock().unwrap().push(format!("open {workspace}"));
                Ok(Box::new(RecServer {
                    calls: Arc::clone(&open_calls),
                }) as Box<dyn HivedServerApi>)
            })),
            release_reexec_lock_fd: Some(Arc::new(move |fd| {
                release_sink.lock().unwrap().push(format!("release {fd:?}"))
            })),
            cleanup_socket: Some(Arc::new(move |workspace| {
                cleanup_sink
                    .lock()
                    .unwrap()
                    .push(format!("cleanup {workspace}"))
            })),
            is_tmux_window_alive: Some(Arc::new(|_id| false)),
            make_busy_monitor: Some(Arc::new(|_session| None)),
            notify_debug_emit: Some(Arc::new(|_ws, _event, _fields| {})),
            ..Default::default()
        };
        let _guard = testhook::install(hook);

        _hived_loop(&workspace, "team-a", "", "");

        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                format!("open {workspace}"),
                "release Some(77)".to_string(),
                "release None".to_string(),
                "server.close".to_string(),
                format!("cleanup {workspace}"),
            ]
        );
        assert!(std::env::var(_HIVED_REEXEC_LOCK_ENV).is_err());
    }

    #[test]
    fn test_send_request_budget_covers_native_submission() {
        // The CLI socket budget is strictly longer than the worst-case
        // native transport submission: a valid slow acceptance must never
        // surface as `hived unavailable`.
        let native = crate::adapters::claude_sessions::SUBMIT_TIMEOUT
            .max(crate::adapters::codex_app_server::SUBMIT_TIMEOUT)
            .max(crate::adapters::grok_leader::SUBMIT_TIMEOUT);
        assert!(_send_request_timeout() > native);
    }

    #[test]
    fn test_request_send_survives_delayed_but_valid_acceptance() {
        // A hived that answers after a delay still gets its truthful queued
        // response back to the CLI (no duplicate-inviting None).
        let run_tmp = tempfile::Builder::new()
            .prefix("hsq")
            .tempdir_in("/tmp")
            .unwrap();
        let run_dir = run_tmp.path().to_path_buf();
        let hook = Hook {
            run_dir: Some(Arc::new(move |_ws| run_dir.clone())),
            ..Default::default()
        };
        let _guard = testhook::install(hook);

        let listener = UnixListener::bind(run_tmp.path().join("hived.sock")).unwrap();
        thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut conn = conn;
            let mut buf = [0u8; 65536];
            loop {
                match conn.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue, // drain until the client half-closes
                }
            }
            thread::sleep(Duration::from_millis(800)); // valid latency, below the budget
            let _ =
                (&conn).write_all(b"{\"ok\": true, \"msgId\": \"x1\", \"delivery\": \"queued\"}\n");
        });

        let response = request_send("/tmp/ws-x", "t", "a", "%1", "b", "hello", "", "");

        let response = response.expect("delayed acceptance must not be dropped");
        assert_eq!(response["delivery"], Value::from("queued"));
    }

    // ---- test_hived_views.py -----------------------------------------------

    #[test]
    fn test_thread_payload_projects_pure_send_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        bus::init_workspace(&workspace).unwrap();

        bus::write_event(
            &workspace, "momo", "orch", "send", "root", "", None, "a001", "",
        )
        .unwrap();
        bus::write_event(
            &workspace, "orch", "momo", "send", "reply", "", None, "a002", "a001",
        )
        .unwrap();
        bus::write_event(
            &workspace,
            "momo",
            "orch",
            "send",
            "follow-up",
            "",
            None,
            "a003",
            "a002",
        )
        .unwrap();
        let mut metadata = Map::new();
        metadata.insert("msgId".to_string(), Value::from("a002"));
        metadata.insert("result".to_string(), Value::from("success"));
        metadata.insert(
            "observedAt".to_string(),
            Value::from("2026-04-15T00:00:00Z"),
        );
        bus::write_event(
            &workspace,
            "_system",
            "",
            "observation",
            "",
            "",
            Some(&metadata),
            "a002",
            "",
        )
        .unwrap();

        let payload = _thread_payload(&workspace.to_string_lossy(), "a003").unwrap();

        assert_eq!(payload["ok"], Value::Bool(true));
        assert_eq!(payload["rootMsgId"], Value::from("a001"));
        assert_eq!(payload["focusMsgId"], Value::from("a003"));
        let messages = payload["messages"].as_array().unwrap();
        let ids: Vec<&str> = messages
            .iter()
            .map(|m| m["msgId"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["a001", "a002", "a003"]);
        let depths: Vec<i64> = messages
            .iter()
            .map(|m| m["depth"].as_i64().unwrap())
            .collect();
        assert_eq!(depths, vec![0, 1, 2]);
        assert_eq!(messages[2]["focus"], Value::Bool(true));
        // threads are pure message chains: no delivery decoration exists
        assert!(messages
            .iter()
            .all(|m| m.as_object().unwrap().get("delivery").is_none()));
    }

    // ---- test_delivery_durability.py ---------------------------------------

    fn wire_send(hook: &mut Hook, workspace: &Path) {
        let workspace = workspace.to_string_lossy().to_string();
        hook.resolve_live_agent = Some(Arc::new(move |_team, _agent| {
            let team = Team {
                name: "team-x".to_string(),
                workspace: workspace.clone(),
                tmux_session: "dev".to_string(),
                tmux_window: "dev:0".to_string(),
                ..Default::default()
            };
            Ok((team, fake_agent("b", "%9", "claude")))
        }));
        hook.check_send_gate = Some(Arc::new(|_target| Ok(())));
    }

    #[allow(clippy::too_many_arguments)]
    fn send_payload_for_test(
        workspace: &Path,
        sender: &str,
        target: &str,
        body: &str,
        artifact: &str,
        reply_to: &str,
    ) -> Map<String, Value> {
        _send_payload(
            &workspace.to_string_lossy(),
            "team-x",
            sender,
            "%1",
            target,
            body,
            artifact,
            reply_to,
        )
        .unwrap()
    }

    #[test]
    fn test_accepted_send_returns_identity_only() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        bus::init_workspace(&workspace).unwrap();
        let mut hook = Hook::default();
        wire_send(&mut hook, &workspace);
        hook.agent_send = Some(Arc::new(|_agent, _text| Ok("udsWriteAccepted".to_string())));
        let _guard = testhook::install(hook);

        let payload = send_payload_for_test(&workspace, "a", "b", "hi", "", "");

        assert_eq!(payload["ok"], Value::Bool(true));
        assert!(!payload["msgId"].as_str().unwrap().is_empty());
        assert!(!payload.contains_key("delivery"));
        // exactly one durable event: the send itself — no observations, no
        // tracking
        let intents: Vec<String> = bus::read_all_events(&workspace)
            .unwrap()
            .into_iter()
            .map(|e| e.intent)
            .collect();
        assert_eq!(intents, vec!["send".to_string()]);
    }

    #[test]
    fn test_refused_send_fails_synchronously() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        bus::init_workspace(&workspace).unwrap();
        let mut hook = Hook::default();
        wire_send(&mut hook, &workspace);
        hook.agent_send = Some(Arc::new(|_agent, _text| {
            Err(DeliveryError("no channel".to_string()))
        }));
        let _guard = testhook::install(hook);

        let payload = send_payload_for_test(&workspace, "a", "b", "hi", "", "");

        assert_eq!(payload["ok"], Value::Bool(false));
        assert!(payload["error"]
            .as_str()
            .unwrap()
            .contains("transport refused"));
    }

    #[test]
    fn test_three_message_busy_incident_regression() {
        // Three sends to a busy target all succeed in order with zero
        // duplicate transport submissions and zero sender-pane disturbance.
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        bus::init_workspace(&workspace).unwrap();
        let delivered: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&delivered);
        let mut hook = Hook::default();
        wire_send(&mut hook, &workspace);
        hook.agent_send = Some(Arc::new(move |_agent, text| {
            sink.lock().unwrap().push(text.to_string());
            Ok("udsWriteAccepted".to_string())
        }));
        let _guard = testhook::install(hook);

        let mut results = Vec::new();
        for body in ["first", "second", "third"] {
            results.push(send_payload_for_test(
                &workspace,
                "validator",
                "worker",
                body,
                "",
                "",
            ));
        }

        assert!(results.iter().all(|r| r["ok"] == Value::Bool(true)));
        let delivered = delivered.lock().unwrap();
        let bodies: Vec<&str> = delivered
            .iter()
            .map(|d| d.split('\n').nth(1).unwrap())
            .collect();
        assert_eq!(bodies, vec!["first", "second", "third"]);
        assert_eq!(delivered.len(), 3); // no duplicate submissions, ever
        let ids: HashSet<&str> = results
            .iter()
            .map(|r| r["msgId"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn test_send_to_flow_mailbox_writes_bus_row_without_transport() {
        // `flow.run` is a mailbox: the durable bus row IS the delivery. No
        // member resolution, no gate, no transport — a member's
        // `hive send flow.run` must succeed with no flow-runner pane
        // anywhere.
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        bus::init_workspace(&workspace).unwrap();
        let hook = Hook {
            resolve_live_agent: Some(Arc::new(|_team, _agent| {
                panic!("mailbox send must not resolve a live agent")
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);

        let payload =
            send_payload_for_test(&workspace, "impl", "flow.run", "done", "/tmp/a.md", "m1");

        assert_eq!(payload["ok"], Value::Bool(true));
        assert_eq!(payload["mailbox"], Value::Bool(true));
        let events = bus::read_all_events(&workspace).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].to, "flow.run");
        assert_eq!(events[0].from, "impl");
        assert_eq!(events[0].in_reply_to, "m1");
    }

    // ---- test_retained_shell_liveness.py (hived-owned surface) -------------

    fn retained_shell_hook() -> Hook {
        Hook {
            is_pane_alive: Some(Arc::new(|_p| true)),
            // output-based busy would say True: the contract must force it
            // off for anything that is not a live CLI
            busy_output_payload: Some(Arc::new(|_p| busy_map(true))),
            claude_bg_runtime: Some(Arc::new(|_p| None)),
            codex_app_server_runtime: Some(Arc::new(|_p| {
                panic!("daemon runtime must not be consulted for a retained shell")
            })),
            ..Default::default()
        }
    }

    #[test]
    fn test_payload_pane_dead_is_fully_offline() {
        let mut hook = retained_shell_hook();
        hook.is_pane_alive = Some(Arc::new(|_p| false));
        hook.codex_app_server_runtime = None;
        let _guard = testhook::install(hook);
        let rt = _agent_runtime_payload("%9", None);
        assert_eq!(rt["alive"], Value::Bool(false));
        assert_eq!(rt["cliAlive"], Value::Bool(false));
        assert_eq!(rt["busy"], Value::Bool(false));
        assert_eq!(rt["inputState"], Value::from("offline"));
        assert_eq!(rt["inputReason"], Value::from("pane_dead"));
    }

    #[test]
    fn test_payload_retained_shell_with_stale_codex_title() {
        // the title/daemon still smell of codex but the TTY has only the
        // shell — neither is liveness evidence
        let mut hook = retained_shell_hook();
        hook.detect_cli_process_for_pane = Some(Arc::new(|_p| None));
        let _guard = testhook::install(hook);
        let rt = _agent_runtime_payload("%9", None);
        assert_eq!(rt["alive"], Value::Bool(true));
        assert_eq!(rt["cliAlive"], Value::Bool(false));
        assert_eq!(rt["busy"], Value::Bool(false));
        assert_eq!(rt["inputState"], Value::from("offline"));
        assert_eq!(rt["inputReason"], Value::from("cli_exited"));
    }

    #[test]
    fn test_payload_live_codex_process_reaches_daemon_runtime() {
        let mut hook = retained_shell_hook();
        hook.detect_cli_process_for_pane = Some(Arc::new(|_p| codex_profile()));
        hook.resolve_model_for_pane = Some(Arc::new(|_p, _c, _m| String::new()));
        hook.codex_app_server_runtime = Some(Arc::new(|_p| {
            Some(json_obj(&[
                ("busy", Value::Bool(true)),
                ("inputState", Value::from("ready")),
                ("inputReason", Value::from("")),
            ]))
        }));
        hook.cas_session_id_for_pane = Some(Arc::new(|_p| Some("sid-1".to_string())));
        let _guard = testhook::install(hook);
        let rt = _agent_runtime_payload("%9", None);
        assert_eq!(rt["cliAlive"], Value::Bool(true));
        assert_eq!(rt["busy"], Value::Bool(true));
        assert_eq!(rt["sessionId"], Value::from("sid-1"));
    }

    #[test]
    fn test_payload_live_claude_process_is_cli_alive() {
        let mut hook = retained_shell_hook();
        hook.codex_app_server_runtime = None;
        hook.detect_cli_process_for_pane = Some(Arc::new(|_p| claude_profile()));
        hook.resolve_model_for_pane = Some(Arc::new(|_p, _c, _m| String::new()));
        hook.adapters_get = Some(Arc::new(|_name| None));
        let _guard = testhook::install(hook);
        let rt = _agent_runtime_payload("%9", None);
        assert_eq!(rt["cliAlive"], Value::Bool(true));
        // flow passed the liveness gate and stopped at the adapter, not at
        // offline
        assert_eq!(rt["inputState"], Value::from("unknown"));
        assert_eq!(rt["inputReason"], Value::from("no_session"));
    }

    #[test]
    fn test_send_to_retained_shell_fails_closed_with_durable_bus_event() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        bus::init_workspace(&workspace).unwrap();
        // real Agent::send path with the agent-side probes pinned to "no
        // live CLI"
        let _agent_guard = crate::agent::testhook::install(crate::agent::testhook::Hook::new());
        let mut hook = Hook::default();
        wire_send(&mut hook, &workspace);
        hook.resolve_live_agent = Some(Arc::new({
            let workspace = workspace.to_string_lossy().to_string();
            move |_team, _agent| {
                let team = Team {
                    name: "team-x".to_string(),
                    workspace: workspace.clone(),
                    ..Default::default()
                };
                Ok((team, fake_agent("v", "%9", "codex")))
            }
        }));
        let _guard = testhook::install(hook);

        let payload = send_payload_for_test(&workspace, "w", "v", "hi", "", "");

        assert_eq!(payload["ok"], Value::Bool(false));
        let error = payload["error"].as_str().unwrap();
        assert!(error.contains("transport refused"));
        assert!(error.contains("cli_exited"));
        // the send event is durable: recoverable from the bus by msgId
        let intents: Vec<String> = bus::read_all_events(&workspace)
            .unwrap()
            .into_iter()
            .map(|e| e.intent)
            .collect();
        assert_eq!(intents, vec!["send".to_string()]);
        assert!(!payload["msgId"].as_str().unwrap().is_empty());
    }

    #[test]
    fn test_send_with_live_cli_still_uses_native_transport() {
        for cli_name in ["codex", "grok", "claude"] {
            let tmp = tempfile::tempdir().unwrap();
            let workspace = tmp.path().join("ws");
            bus::init_workspace(&workspace).unwrap();

            let mut agent_hook = crate::agent::testhook::Hook::new();
            agent_hook.cli_probe = Some(cli_name.to_string());
            match cli_name {
                "codex" => agent_hook.codex_send_to_pane = Some("turnStartAccepted"),
                "grok" => agent_hook.grok_send_to_pane = Some("sessionPromptQueued"),
                _ => {
                    agent_hook.job_id_for_pane = Some("cafe1234".to_string());
                    agent_hook.engines_by_job = HashMap::from([(
                        "cafe1234".to_string(),
                        crate::agent::testhook::fake_engine(4242, "cafe1234", "sid-1"),
                    )]);
                    agent_hook.sessions_send = Some("udsWriteAccepted");
                }
            }
            let _agent_guard = crate::agent::testhook::install(agent_hook);

            let mut hook = Hook::default();
            hook.check_send_gate = Some(Arc::new(|_target| Ok(())));
            hook.resolve_live_agent = Some(Arc::new({
                let cli = cli_name.to_string();
                move |_team, _agent| Ok((fake_team("team-x", vec![]), fake_agent("v", "%9", &cli)))
            }));
            let _guard = testhook::install(hook);

            let payload = send_payload_for_test(&workspace, "w", "v", "hi", "", "");
            assert_eq!(payload["ok"], Value::Bool(true), "cli={cli_name}");

            match cli_name {
                "codex" => {
                    let sent = crate::agent::testhook::with(|h| h.codex_sent.clone()).unwrap();
                    assert_eq!(sent[0].0, "%9");
                }
                "grok" => {
                    let sent = crate::agent::testhook::with(|h| h.grok_sent.clone()).unwrap();
                    assert_eq!(sent[0].0, "%9");
                }
                _ => {
                    let writes = crate::agent::testhook::with(|h| h.inbox_writes.clone()).unwrap();
                    // claude routes pane -> job record -> engine entry ->
                    // that engine's inbox socket
                    assert_eq!(writes[0].0, "/tmp/hive-test-inbox-4242.sock");
                }
            }
        }
    }

    #[test]
    fn test_idle_notify_excludes_retained_shell_pane() {
        let bindings: Vec<(String, Map<String, Value>)> = [("w", "%1"), ("v", "%2")]
            .iter()
            .map(|(name, pane)| {
                let mut row = Map::new();
                row.insert("role".to_string(), Value::from("agent"));
                row.insert("pane".to_string(), Value::from(*pane));
                (name.to_string(), row)
            })
            .collect();
        let hook = Hook {
            team_member_bindings: Some(Arc::new(move |_team| Ok(bindings.clone()))),
            is_pane_alive: Some(Arc::new(|_p| true)),
            detect_cli_process_for_pane: Some(Arc::new(|pane| {
                if pane == "%1" {
                    claude_profile()
                } else {
                    None
                }
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        assert_eq!(_idle_notify_agent_panes("t"), vec!["%1".to_string()]);
    }

    #[test]
    fn test_doctor_payload_exposes_cli_alive() {
        let hook = Hook {
            team_load: Some(Arc::new(|_name| {
                Ok(fake_team("t", vec![fake_agent("v", "%1", "codex")]))
            })),
            agent_is_alive: Some(Arc::new(|_a| true)),
            member_runtime_payload: Some(Arc::new(|_p, _r| {
                json_obj(&[
                    ("alive", Value::Bool(true)),
                    ("cliAlive", Value::Bool(false)),
                    ("busy", Value::Bool(false)),
                    ("inputState", Value::from("offline")),
                    ("inputReason", Value::from("cli_exited")),
                ])
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);
        let diag = _doctor_payload("/tmp/ws", "t", "v", false, None).unwrap();
        assert_eq!(diag["alive"], Value::Bool(true));
        assert_eq!(diag["cliAlive"], Value::Bool(false));
    }

    // ---- test_agent_headless.py (the two hived-owned tests) ----------------

    fn headless_member(cli: &str, session_id: Option<&str>) -> Agent {
        Agent {
            name: "rex".to_string(),
            team_name: "honey".to_string(),
            pane_id: String::new(),
            model: String::new(),
            prompt: String::new(),
            cwd: "/repo".to_string(),
            session_id: session_id.map(|s| s.to_string()),
            spawned_at: 0.0,
            cli: cli.to_string(),
        }
    }

    #[test]
    fn test_headless_member_runtime_grok() {
        let hook = Hook {
            gl_runtime_for_key: Some(Arc::new(|key| {
                if key == "m-honey.rex" {
                    Some(session_runtime(true, "tool_open", "ready"))
                } else {
                    None
                }
            })),
            gl_read_session_key: Some(Arc::new(|_key| {
                Some(("sid-g".to_string(), "/repo".to_string()))
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);

        let payload = _headless_member_runtime(&headless_member("grok", Some("sid-1")));

        assert_eq!(payload["headless"], Value::Bool(true));
        assert_eq!(payload["alive"], Value::Bool(true));
        assert_eq!(payload["busy"], Value::Bool(true));
        assert_eq!(payload["sessionId"], Value::from("sid-g"));
    }

    #[test]
    fn test_headless_member_runtime_unknown_engine() {
        let _guard = testhook::install(Hook::default());

        let payload = _headless_member_runtime(&headless_member("codex", None));

        assert_eq!(payload["alive"], Value::Bool(false));
        assert_eq!(payload["inputState"], Value::from("unknown"));
    }

    // ---- test_registry.py: the hived writer over the registry --------------

    fn writer_team(agents: Vec<Agent>) -> Team {
        Team {
            name: "honey".to_string(),
            tmux_window: "dev:0".to_string(),
            tmux_window_id: "@0".to_string(),
            created_at: 123.0,
            agents,
            ..Default::default()
        }
    }

    fn writer_hook(team: Team, sessions: &[(&str, &str)]) -> Hook {
        let sessions: HashMap<String, String> = sessions
            .iter()
            .map(|(p, s)| (p.to_string(), s.to_string()))
            .collect();
        Hook {
            team_load: Some(Arc::new(move |_name| Ok(team.clone()))),
            fresh_snapshot_session_id: Some(Arc::new(move |pane| {
                sessions.get(pane).cloned().unwrap_or_default()
            })),
            resolve_model_for_pane: Some(Arc::new(|_pane, cli_name, _current| {
                format!("m-{cli_name}")
            })),
            ..Default::default()
        }
    }

    fn roster_by_name(team: &str) -> HashMap<String, Map<String, Value>> {
        crate::registry::load(team)
            .unwrap()
            .get("members")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|m| {
                let m = m.as_object().unwrap().clone();
                (m["name"].as_str().unwrap().to_string(), m)
            })
            .collect()
    }

    #[test]
    fn test_writer_backfills_roster_and_display() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path().join(".hive"));
        let mut worker_row = Map::new();
        worker_row.insert("name".to_string(), Value::from("worker"));
        let mut validator_row = Map::new();
        validator_row.insert("name".to_string(), Value::from("validator"));
        assert_eq!(
            crate::registry::record_team("honey", "/ws", "123.0", &[worker_row, validator_row], "")
                .unwrap(),
            "written"
        );
        {
            let hook = writer_hook(
                writer_team(vec![
                    fake_agent("worker", "%1", "claude"),
                    fake_agent("validator", "%2", "codex"),
                ]),
                &[("%1", "sid-w"), ("%2", "sid-v")],
            );
            let _guard = testhook::install(hook);

            _write_registry_backfill("/ws", "honey");
        }

        let entry = crate::registry::load("honey").unwrap();
        let by_name = roster_by_name("honey");
        assert_eq!(by_name["worker"]["sessionId"], Value::from("sid-w"));
        assert_eq!(by_name["validator"]["sessionId"], Value::from("sid-v"));
        assert_eq!(by_name["validator"]["model"], Value::from("m-codex"));
        assert_eq!(entry["display"], Value::from("@0"));

        // validator pane dies: only the worker observed, session rotated
        {
            let hook = writer_hook(
                writer_team(vec![fake_agent("worker", "%1", "claude")]),
                &[("%1", "sid-w2")],
            );
            let _guard = testhook::install(hook);
            _write_registry_backfill("/ws", "honey");
        }
        let by_name2 = roster_by_name("honey");
        assert_eq!(by_name2["validator"]["sessionId"], Value::from("sid-v")); // dead member survives
        assert_eq!(by_name2["worker"]["sessionId"], Value::from("sid-w2"));
    }

    #[test]
    fn test_writer_without_registry_entry_writes_nothing() {
        // Observation never creates a roster: membership belongs to the CLI.
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path().join(".hive"));
        let hook = writer_hook(
            writer_team(vec![fake_agent("worker", "%1", "claude")]),
            &[("%1", "sid-w")],
        );
        let _guard = testhook::install(hook);

        _write_registry_backfill("/ws", "honey");

        assert!(crate::registry::load("honey").is_none());
    }
}
