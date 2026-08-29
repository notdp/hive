//! Grok leader client over a *per-pane* leader daemon.
//!
//! Each hive-spawned grok pane runs its own `grok agent leader --leader-socket
//! <sock>` daemon sharing the real GROK_HOME. The grok TUI in that pane attaches
//! to it (`grok --leader --leader-socket <sock> --session-id <uuid>`); hive
//! attaches as a second client through `grok agent --leader stdio --leader-socket
//! <sock>` — a subprocess speaking ACP JSON-RPC 2.0 as newline-delimited JSON on
//! stdin/stdout. The leader's own socket protocol is private, so hive never talks
//! to the socket directly: the stdio subprocess is the supported door.
//!
//! Which session that second client drives is not discoverable from the leader
//! (`session/list` returns every session of the cwd), so hive mints the pane's
//! session id at spawn time and records it beside the socket in a `.session`
//! file. The client loads exactly that session and folds only its notifications.
//!
//! `session/load` replays the session's past `session/update` notifications
//! before it answers, so everything received before the load response is dropped —
//! a replayed turn must never mark the pane busy. Delivery acks on the leader
//! echoing the prompt back (queue entry or `user_message_chunk`): the
//! `session/prompt` response itself only lands when the whole turn ends, which
//! can be minutes.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{json, Value};

const _INIT_TIMEOUT: f64 = 10.0; // initialize answers ~2 s after process start
const _LOAD_TIMEOUT: f64 = 5.0; // session/load ~0.8 s plus the notification replay
const _HANDSHAKE_TIMEOUT: f64 = _INIT_TIMEOUT + _LOAD_TIMEOUT;
const _ACK_TIMEOUT: f64 = 10.0;
const _CALL_TIMEOUT: f64 = 10.0;
const _DAEMON_START_TIMEOUT: f64 = 8.0;
const _CONNECT_COOLDOWN: f64 = 5.0;

/// Worst-case local submission budget for one send_to_pane call: a cold client
/// (initialize + session/load) plus the ack wait. The hived derives its request
/// budgets from this so a valid slow acceptance can never outlive its caller.
pub const SUBMIT_TIMEOUT: f64 = _HANDSHAKE_TIMEOUT + _ACK_TIMEOUT;

/// Accepted-transport classification for durable delivery observations: the
/// leader took the prompt into the session queue. Not proof the turn ran.
pub const PROMPT_QUEUED: &str = "sessionPromptQueued";

/// The ACP cancel left for the leader. It is a notification, so this is the
/// only accept class there is — see [`GrokStdioClient::cancel`].
pub const CANCEL_SENT: &str = "sessionCancelSent";

const _TOOL_PHASES: [&str; 2] = ["tool_open", "tool_result_pending_reply"];
const _MESSAGE_CHUNKS: [&str; 3] = [
    "agent_message_chunk",
    "agent_thought_chunk",
    "user_message_chunk",
];

pub fn grok_home() -> PathBuf {
    match env::var("GROK_HOME") {
        Ok(value) => PathBuf::from(value),
        Err(_) => PathBuf::from(env::var("HOME").unwrap_or_default()).join(".grok"),
    }
}

// --------------------------------------------------------------------------
// daemon keys: the engine's identity on disk
//
// A leader daemon is keyed by WHO it serves, not where it is displayed:
// `m-<team>.<member>` for a team member (the engine survives its pane),
// `p<slug>` for a raw `hive grok` pane outside any team (pane lifecycle).
// Pane-facing APIs resolve the pane to its key through the pane's member
// tags, so a tagged member pane and a headless caller reach the same files.
// --------------------------------------------------------------------------

const _KEY_TTL: f64 = 5.0;

static _KEY_CACHE: OnceLock<Mutex<HashMap<String, (Instant, String)>>> = OnceLock::new();

fn _key_cache() -> &'static Mutex<HashMap<String, (Instant, String)>> {
    _KEY_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn member_key(team: &str, member: &str) -> String {
    format!("m-{team}.{member}")
}

pub fn pane_key(pane: &str) -> String {
    let slug = pane.replace('%', "");
    if slug.is_empty() {
        "pdefault".to_string()
    } else {
        format!("p{slug}")
    }
}

/// `m-<team>.<member>` -> (team, member); team names are dot-free.
pub fn member_from_key(key: &str) -> Option<(String, String)> {
    let rest = key.strip_prefix("m-")?;
    let (team, member) = rest.split_once('.')?;
    if team.is_empty() || member.is_empty() {
        return None;
    }
    Some((team.to_string(), member.to_string()))
}

/// The pane-option read seam: the real tmux round-trip in production, a
/// per-test override (default: untagged) under cfg(test) — tests must never
/// hit the real tmux server.
fn _pane_option(pane: &str, key: &str) -> Option<String> {
    #[cfg(test)]
    {
        tests::pane_option_override(pane, key)
    }
    #[cfg(not(test))]
    {
        crate::tmux::get_pane_option(pane, key)
    }
}

/// The daemon key a pane addresses: its member key when tagged, else its
/// pane key. Cached briefly — tag reads are tmux round-trips on hot paths.
pub fn resolve_pane_key(pane: &str) -> String {
    let now = Instant::now();
    {
        let cache = _key_cache().lock().unwrap();
        if let Some((at, key)) = cache.get(pane) {
            if now.duration_since(*at).as_secs_f64() < _KEY_TTL {
                return key.clone();
            }
        }
    }
    let mut key = pane_key(pane);
    if !pane.is_empty() {
        let team = _pane_option(pane, "hive-team").unwrap_or_default();
        let member = _pane_option(pane, "hive-agent").unwrap_or_default();
        if !team.is_empty() && !member.is_empty() {
            key = member_key(&team, &member);
        }
    }
    _key_cache()
        .lock()
        .unwrap()
        .insert(pane.to_string(), (now, key.clone()));
    key
}

/// Leader socket under the real GROK_HOME.
///
/// Deliberately short (`hive/p19.sock` / `hive/m-honey.rex.sock`):
/// AF_UNIX paths cap at 104 bytes and the leader binds this path itself.
pub fn socket_path_for_key(key: &str) -> PathBuf {
    grok_home().join("hive").join(format!("{key}.sock"))
}

pub fn pane_socket_path(pane: &str) -> PathBuf {
    socket_path_for_key(&resolve_pane_key(pane))
}

/// Sibling pidfile of the leader socket.
///
/// Written once the socket appears so the hived (which does not start the
/// daemon) can prove liveness and reap orphans.
pub fn pane_pidfile_path(pane: &str) -> PathBuf {
    pane_socket_path(pane).with_extension("pid")
}

/// Sibling record of the session id hive minted for this daemon.
pub fn session_path_for_key(key: &str) -> PathBuf {
    socket_path_for_key(key).with_extension("session")
}

pub fn pane_session_path(pane: &str) -> PathBuf {
    session_path_for_key(&resolve_pane_key(pane))
}

pub fn write_session_key(key: &str, session_id: &str, cwd: &str) -> Result<()> {
    let path = session_path_for_key(key);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &path,
        json!({"sessionId": session_id, "cwd": cwd}).to_string(),
    )?;
    Ok(())
}

pub fn write_pane_session(pane: &str, session_id: &str, cwd: &str) -> Result<()> {
    write_session_key(&resolve_pane_key(pane), session_id, cwd)
}

pub fn read_session_key(key: &str) -> Option<(String, String)> {
    let text = fs::read_to_string(session_path_for_key(key)).ok()?;
    let data: Value = serde_json::from_str(&text).ok()?;
    let obj = data.as_object()?;
    let session_id = obj
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|sid| !sid.is_empty())?;
    let cwd = obj
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())?;
    Some((session_id.to_string(), cwd.to_string()))
}

pub fn read_pane_session(pane: &str) -> Option<(String, String)> {
    read_session_key(&resolve_pane_key(pane))
}

/// Leader env: the member's identity, nothing inherited that lies.
///
/// The spawner may itself run inside another member's engine (an orch's
/// flow runner), whose env carries that engine's identity markers —
/// CLAUDE_CODE_MESSAGING_SOCKET would make every hive call inside this
/// grok member resolve to the *orch's* pane, and inherited HIVE_TEAM /
/// HIVE_MEMBER would name the spawner. Wash them; pin our own.
fn _daemon_env_for_pane(pane: &str) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| {
            !(key.starts_with("CLAUDE")
                || key.starts_with("ANTHROPIC")
                || matches!(
                    key.as_str(),
                    "CODEX_THREAD_ID" | "HIVE_TEAM" | "HIVE_MEMBER"
                ))
        })
        .collect();
    env.insert("TMUX_PANE".to_string(), pane.to_string());
    if let Some((team, member)) = member_from_key(&resolve_pane_key(pane)) {
        env.insert("HIVE_TEAM".to_string(), team);
        env.insert("HIVE_MEMBER".to_string(), member);
    }
    env
}

/// Inverse of [`socket_path_for_key`]: `p19.sock` -> `p19`.
fn _key_from_socket_name(name: &str) -> Option<String> {
    let key = name.strip_suffix(".sock")?;
    if key.starts_with("m-") {
        return if member_from_key(key).is_some() {
            Some(key.to_string())
        } else {
            None
        };
    }
    if let Some(rest) = key.strip_prefix('p') {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return Some(key.to_string());
        }
    }
    None
}

// --------------------------------------------------------------------------
// per-session runtime state, kept current by the reader thread
// --------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct SessionRuntime {
    pub busy: bool,
    pub turn_phase: String,
    pub input_state: String,
    pub session_id: Option<String>,
    pub observed_at: f64,
}

impl Default for SessionRuntime {
    fn default() -> Self {
        SessionRuntime {
            busy: false,
            turn_phase: "unknown_evidence".to_string(),
            input_state: String::new(),
            session_id: None,
            observed_at: 0.0,
        }
    }
}

fn _now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// --------------------------------------------------------------------------
// the leader subprocess seam (Python `subprocess.Popen`)
// --------------------------------------------------------------------------

/// The stdio child as the client sees it. Production wraps a real
/// `grok agent --leader stdio` child; tests substitute a scripted fake.
trait LeaderProc: Send + Sync {
    /// Write one newline-terminated JSON line to the child's stdin and flush.
    fn write_line(&self, line: &str) -> io::Result<()>;
    /// Hand out the child's stdout exactly once (for the reader thread).
    fn take_stdout(&self) -> Option<Box<dyn Read + Send>>;
    fn poll(&self) -> Option<i32>;
    fn terminate(&self);
    fn wait(&self, timeout: f64);
    fn close_stdin(&self);
}

#[cfg_attr(test, allow(dead_code))]
struct RealProc {
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    stdout: Mutex<Option<Box<dyn Read + Send>>>,
}

#[cfg_attr(test, allow(dead_code))]
impl RealProc {
    fn spawn(argv: &[String]) -> io::Result<RealProc> {
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .map(|out| Box::new(out) as Box<dyn Read + Send>);
        Ok(RealProc {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(stdout),
        })
    }
}

impl LeaderProc for RealProc {
    fn write_line(&self, line: &str) -> io::Result<()> {
        let mut guard = self.stdin.lock().unwrap();
        let stdin = guard
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "stdin closed"))?;
        stdin.write_all(format!("{line}\n").as_bytes())?;
        stdin.flush()
    }

    fn take_stdout(&self) -> Option<Box<dyn Read + Send>> {
        self.stdout.lock().unwrap().take()
    }

    fn poll(&self) -> Option<i32> {
        let mut child = self.child.lock().unwrap();
        match child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or_else(|| {
                use std::os::unix::process::ExitStatusExt;
                -status.signal().unwrap_or(1)
            })),
            Ok(None) => None,
            Err(_) => Some(-1),
        }
    }

    fn terminate(&self) {
        // Python Popen.terminate: a no-op once the child was reaped.
        let mut child = self.child.lock().unwrap();
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
    }

    fn wait(&self, timeout: f64) {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout);
        while Instant::now() < deadline {
            if self.poll().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn close_stdin(&self) {
        *self.stdin.lock().unwrap() = None;
    }
}

fn _spawn_stdio_proc(argv: &[String]) -> io::Result<Arc<dyn LeaderProc>> {
    #[cfg(test)]
    {
        tests::stdio_spawn_override(argv)
    }
    #[cfg(not(test))]
    {
        Ok(Arc::new(RealProc::spawn(argv)?))
    }
}

// --------------------------------------------------------------------------
// one stdio client attached to one pane's leader
// --------------------------------------------------------------------------

/// Python `threading.Event`.
struct Event {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Event {
    fn new() -> Arc<Event> {
        Arc::new(Event {
            flag: Mutex::new(false),
            cv: Condvar::new(),
        })
    }

    fn set(&self) {
        *self.flag.lock().unwrap() = true;
        self.cv.notify_all();
    }

    /// True when the event was set within `timeout` seconds.
    fn wait(&self, timeout: f64) -> bool {
        let guard = self.flag.lock().unwrap();
        let (guard, _res) = self
            .cv
            .wait_timeout_while(guard, Duration::from_secs_f64(timeout), |set| !*set)
            .unwrap();
        *guard
    }
}

struct Ack {
    text: String,
    event: Arc<Event>,
}

struct Slot {
    event: Arc<Event>,
    msg: Mutex<Option<Value>>,
    /// Session id this response binds when it lands without error.
    loads: Option<String>,
}

#[derive(Default)]
struct ClientShared {
    runtime: SessionRuntime,
    ack: Option<Ack>,
    loaded: bool,
}

struct ClientInner {
    proc: Arc<dyn LeaderProc>,
    /// Python `_io_lock`: stdin writes are atomic per message.
    io_lock: Mutex<()>,
    next_id: Mutex<u64>,
    state: Mutex<ClientShared>,
    pending: Mutex<HashMap<u64, Arc<Slot>>>,
    closed: AtomicBool,
}

impl ClientInner {
    fn next_rid(&self) -> u64 {
        let mut id = self.next_id.lock().unwrap();
        *id += 1;
        *id
    }

    fn write(&self, message: &Value) -> bool {
        let _guard = self.io_lock.lock().unwrap();
        self.proc.write_line(&message.to_string()).is_ok()
    }
}

/// Fail every in-flight waiter: a dead child never answers them.
fn _fail_pending(inner: &ClientInner) {
    let slots: Vec<Arc<Slot>> = inner
        .pending
        .lock()
        .unwrap()
        .drain()
        .map(|(_rid, slot)| slot)
        .collect();
    for slot in slots {
        *slot.msg.lock().unwrap() = Some(json!({"error": "closed"}));
        slot.event.set();
    }
}

/// Answer a permission prompt with `cancelled`.
///
/// The decision belongs to the human at the TUI, which gets its own copy of
/// the request; hive must still answer its copy or the turn stalls, and
/// cancelling is the only answer that neither approves nor rejects for them.
fn _on_request(inner: &ClientInner, rid: &Value, method: &str, params: &Value) {
    if method != "session/request_permission" {
        return;
    }
    inner.write(&json!({
        "jsonrpc": "2.0",
        "id": rid,
        "result": {"outcome": {"outcome": "cancelled"}},
    }));
    let mut state = inner.state.lock().unwrap();
    if params.get("sessionId").and_then(Value::as_str) != state.runtime.session_id.as_deref() {
        return;
    }
    state.runtime.input_state = "waiting_user".to_string();
    state.runtime.observed_at = _now_epoch();
}

fn _on_notification(inner: &ClientInner, method: &str, params: &Value) {
    let mut state = inner.state.lock().unwrap();
    if !state.loaded {
        return; // session/load replays past updates; replay is not evidence
    }
    if method == "_x.ai/sessions/changed" {
        let entries: Vec<Value> = params
            .get("upserted")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for entry in entries {
            if entry.is_object()
                && entry.get("sessionId").and_then(Value::as_str)
                    == state.runtime.session_id.as_deref()
            {
                _apply_activity(&mut state, entry.get("activity"));
            }
        }
        return;
    }
    if params.get("sessionId").and_then(Value::as_str) != state.runtime.session_id.as_deref() {
        return;
    }
    state.runtime.observed_at = _now_epoch();
    match method {
        "session/update" => {
            let update = params.get("update").cloned().unwrap_or_else(|| json!({}));
            _apply_update(&mut state, &update);
        }
        "_x.ai/session_notification" => {
            let kind = params
                .get("update")
                .and_then(|update| update.get("sessionUpdate"))
                .and_then(Value::as_str);
            if kind == Some("turn_completed") {
                state.runtime.busy = false;
                state.runtime.turn_phase = "turn_closed".to_string();
                state.runtime.input_state = "ready".to_string();
            }
        }
        "_x.ai/queue/changed" => _apply_queue(&mut state, params),
        _ => {}
    }
}

/// Fold `activity` — the leader's busy authority — into the runtime.
fn _apply_activity(state: &mut ClientShared, activity: Option<&Value>) {
    state.runtime.observed_at = _now_epoch();
    match activity.and_then(Value::as_str) {
        Some("working") => state.runtime.busy = true,
        Some("idle") => {
            state.runtime.busy = false;
            state.runtime.turn_phase = "turn_closed".to_string();
            state.runtime.input_state = "ready".to_string();
        }
        _ => {}
    }
}

fn _apply_update(state: &mut ClientShared, update: &Value) {
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "tool_call" => {
            state.runtime.busy = true;
            state.runtime.turn_phase = "tool_open".to_string();
        }
        "tool_call_update" => {
            // An update on a tool call means the turn is running and any
            // permission it was blocked on has been decided.
            state.runtime.busy = true;
            state.runtime.input_state = "ready".to_string();
            if update.get("status").and_then(Value::as_str) == Some("completed") {
                state.runtime.turn_phase = "tool_result_pending_reply".to_string();
            }
        }
        kind if _MESSAGE_CHUNKS.contains(&kind) => {
            state.runtime.busy = true;
            if !_TOOL_PHASES.contains(&state.runtime.turn_phase.as_str()) {
                state.runtime.turn_phase = "user_prompt_pending".to_string();
            }
            if kind == "user_message_chunk" {
                let text = update
                    .get("content")
                    .and_then(|content| content.get("text"));
                _note_ack(state, text);
            }
        }
        _ => {}
    }
}

fn _apply_queue(state: &mut ClientShared, params: &Value) {
    let entries: Vec<Value> = params
        .get("entries")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter(|entry| entry.is_object())
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if !entries.is_empty() {
        state.runtime.turn_phase = "input_backlog".to_string();
    }
    for entry in &entries {
        _note_ack(state, entry.get("text"));
    }
    _note_ack(state, params.get("runningText"));
}

fn _note_ack(state: &ClientShared, text: Option<&Value>) {
    if let (Some(ack), Some(text)) = (state.ack.as_ref(), text) {
        if text.as_str() == Some(ack.text.as_str()) {
            ack.event.set();
        }
    }
}

fn _reader_loop(inner: Arc<ClientInner>, stdout: Box<dyn Read + Send>) {
    let mut reader = BufReader::new(stdout);
    while !inner.closed.load(Ordering::SeqCst) {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break, // process death
            Ok(_) => {}
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(msg) => msg,
            Err(_) => continue,
        };
        if !msg.is_object() {
            continue;
        }
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .filter(|method| !method.is_empty())
            .map(str::to_string);
        let rid = msg.get("id").filter(|rid| !rid.is_null()).cloned();
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
        match (method, rid) {
            (Some(method), Some(rid)) => _on_request(&inner, &rid, &method, &params),
            (Some(method), None) => _on_notification(&inner, &method, &params),
            _ => {
                // Pop atomically: a `call()` that timed out concurrently may have
                // removed this rid already, and a missing slot only means the
                // waiter is gone — drop the late response instead of raising.
                let slot = msg
                    .get("id")
                    .and_then(Value::as_u64)
                    .and_then(|rid| inner.pending.lock().unwrap().remove(&rid));
                if let Some(slot) = slot {
                    if let Some(session_id) = slot.loads.as_ref() {
                        if msg.get("error").is_none() {
                            let mut state = inner.state.lock().unwrap();
                            state.runtime.session_id = Some(session_id.clone());
                            state.loaded = true;
                        }
                    }
                    *slot.msg.lock().unwrap() = Some(msg);
                    slot.event.set();
                }
            }
        }
    }
    inner.closed.store(true, Ordering::SeqCst);
    _fail_pending(&inner);
}

/// `grok agent --leader stdio` subprocess bound to one daemon key's session.
pub struct GrokStdioClient {
    pub key: String,
    pub socket_path: String,
    inner: Arc<ClientInner>,
    reader: Mutex<Option<thread::JoinHandle<()>>>,
}

impl GrokStdioClient {
    pub fn new(key: &str) -> io::Result<GrokStdioClient> {
        let socket_path = socket_path_for_key(key).to_string_lossy().into_owned();
        let argv: Vec<String> = vec![
            "grok".to_string(),
            "agent".to_string(),
            "--leader".to_string(),
            "stdio".to_string(),
            "--leader-socket".to_string(),
            socket_path.clone(),
        ];
        let proc = _spawn_stdio_proc(&argv)?;
        let stdout = proc
            .take_stdout()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "stdout unavailable"))?;
        let inner = Arc::new(ClientInner {
            proc,
            io_lock: Mutex::new(()),
            next_id: Mutex::new(0),
            state: Mutex::new(ClientShared::default()),
            pending: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        });
        let reader_inner = inner.clone();
        let handle = thread::spawn(move || _reader_loop(reader_inner, stdout));
        Ok(GrokStdioClient {
            key: key.to_string(),
            socket_path,
            inner,
            reader: Mutex::new(Some(handle)),
        })
    }

    /// One request/response. `loads` marks the call that binds a session.
    ///
    /// The reader thread flips the client to loaded before waking this waiter,
    /// so a notification queued right behind the response is folded instead of
    /// being mistaken for replay.
    pub fn call(&self, method: &str, params: Value, timeout: f64, loads: Option<&str>) -> Value {
        if self.inner.closed.load(Ordering::SeqCst) {
            return json!({"__error__": "closed"});
        }
        let rid = self.inner.next_rid();
        let slot = Arc::new(Slot {
            event: Event::new(),
            msg: Mutex::new(None),
            loads: loads.map(str::to_string),
        });
        self.inner.pending.lock().unwrap().insert(rid, slot.clone());
        let message = json!({"jsonrpc": "2.0", "id": rid, "method": method, "params": params});
        if !self.inner.write(&message) {
            self.inner.pending.lock().unwrap().remove(&rid);
            return json!({"__error__": "write failed"});
        }
        if !slot.event.wait(timeout) {
            self.inner.pending.lock().unwrap().remove(&rid);
            return json!({"__timeout__": true});
        }
        let msg = slot.msg.lock().unwrap().take().unwrap_or_else(|| json!({}));
        if let Some(error) = msg.get("error") {
            return json!({"__error__": error.clone()});
        }
        json!({"result": msg.get("result").cloned().unwrap_or(Value::Null)})
    }

    // ---- protocol ----

    /// `initialize` then `session/load` of the key's minted session.
    ///
    /// Both values come from the key's session file — cwd is recorded at
    /// spawn time, so no tmux query is needed here.
    pub fn handshake(&self) -> bool {
        let (session_id, cwd) = match read_session_key(&self.key) {
            Some(session) => session,
            None => return false,
        };
        let initialized = self.call(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientInfo": {"name": "hive", "version": "1"},
                "clientCapabilities": {},
            }),
            _INIT_TIMEOUT,
            None,
        );
        if initialized.get("result").is_none() {
            return false;
        }
        let loaded = self.call(
            "session/load",
            json!({
                "sessionId": session_id,
                "cwd": cwd,
                "mcpServers": [],
            }),
            _LOAD_TIMEOUT,
            Some(&session_id),
        );
        loaded.get("result").is_some()
    }

    /// `initialize` then `session/new` with hive's minted id.
    ///
    /// The headless spawn primitive: the leader materializes the session
    /// (spike-verified: the id must ride `_meta.sessionId` — a top-level
    /// `sessionId` is silently ignored and the server mints its own).
    /// Binds this client to the new session on success.
    pub fn new_session(&self, session_id: &str, cwd: &str) -> bool {
        let initialized = self.call(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientInfo": {"name": "hive", "version": "1"},
                "clientCapabilities": {},
            }),
            _INIT_TIMEOUT,
            None,
        );
        if initialized.get("result").is_none() {
            return false;
        }
        let created = self.call(
            "session/new",
            json!({
                "cwd": cwd,
                "mcpServers": [],
                "_meta": {"sessionId": session_id},
            }),
            _LOAD_TIMEOUT,
            Some(session_id),
        );
        created
            .get("result")
            .and_then(|result| result.get("sessionId"))
            .and_then(Value::as_str)
            == Some(session_id)
    }

    /// Queue one prompt; True once the leader echoes it back.
    ///
    /// The `session/prompt` response only arrives when the turn ends, so the
    /// accept boundary is the echo — a queue entry carrying the text, or the
    /// turn's `user_message_chunk`. The response id stays registered just long
    /// enough to catch an immediate rpc error; its eventual result is dropped.
    pub fn prompt(&self, text: &str) -> bool {
        let done = Event::new();
        let session_id = {
            let mut state = self.inner.state.lock().unwrap();
            state.ack = Some(Ack {
                text: text.to_string(),
                event: done.clone(),
            });
            state.runtime.session_id.clone()
        };
        let rid = self.inner.next_rid();
        let slot = Arc::new(Slot {
            event: done.clone(),
            msg: Mutex::new(None),
            loads: None,
        });
        self.inner.pending.lock().unwrap().insert(rid, slot.clone());
        let accepted = (|| {
            let sent = self.inner.write(&json!({
                "jsonrpc": "2.0",
                "id": rid,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": text}],
                },
            }));
            if !sent || !done.wait(_ack_timeout()) {
                return false;
            }
            let msg = slot.msg.lock().unwrap();
            !matches!(msg.as_ref(), Some(msg) if msg.get("error").is_some())
        })();
        self.inner.pending.lock().unwrap().remove(&rid);
        self.inner.state.lock().unwrap().ack = None;
        accepted
    }

    /// Abort the session's running turn with ACP `session/cancel`.
    ///
    /// Cancel is a *notification*, not a request: 1.0.5 real-machine
    /// verified, one carrying an id is refused `-32601 Method not found`
    /// and the turn runs to completion, while the bare notification ended
    /// the pending `session/prompt` with `stopReason: "cancelled"`
    /// (`MidTurnAbort`) ~0.1 s later. So it goes out without an id and
    /// there is nothing to wait for — the accept boundary is the write onto
    /// a loaded session, and a cancel on an idle session is a no-op.
    pub fn cancel(&self) -> bool {
        let session_id = self.inner.state.lock().unwrap().runtime.session_id.clone();
        let session_id = match session_id {
            Some(session_id) => session_id,
            None => return false,
        };
        self.inner.write(&json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {"sessionId": session_id},
        }))
    }

    /// Compact the session's context; `busy` defers instead of aborting.
    ///
    /// Compaction replaces the running turn, so a mid-turn agent is left alone
    /// and the caller keystrokes `/compact` into the TUI instead.
    pub fn compact(&self) -> &'static str {
        let (busy, session_id) = {
            let state = self.inner.state.lock().unwrap();
            (state.runtime.busy, state.runtime.session_id.clone())
        };
        if busy {
            return "busy";
        }
        let result = self.call(
            "x.ai/compact_conversation",
            json!({"sessionId": session_id}),
            _CALL_TIMEOUT,
            None,
        );
        if result.get("result").is_some() {
            "compacted"
        } else {
            "unavailable"
        }
    }

    /// Snapshot, or None while nothing has been observed for this session.
    pub fn runtime(&self) -> Option<SessionRuntime> {
        let state = self.inner.state.lock().unwrap();
        if state.runtime.observed_at != 0.0 {
            Some(state.runtime.clone())
        } else {
            None
        }
    }

    /// Session this client is bound to, so the pool can spot a rotation.
    pub fn session_id(&self) -> Option<String> {
        self.inner.state.lock().unwrap().runtime.session_id.clone()
    }

    pub fn is_alive(&self) -> bool {
        !self.inner.closed.load(Ordering::SeqCst)
            && self
                .reader
                .lock()
                .unwrap()
                .as_ref()
                .map(|handle| !handle.is_finished())
                .unwrap_or(false)
            && self.inner.proc.poll().is_none()
    }

    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        _fail_pending(&self.inner);
        self.inner.proc.close_stdin();
        self.inner.proc.terminate();
        self.inner.proc.wait(1.0);
    }
}

impl Drop for GrokStdioClient {
    /// Python `weakref.finalize(self, self._proc.terminate)`: a short-lived
    /// CLI (`hive compact`) drops without close(); without this the stdio
    /// child would outlive it and hold a leader connection forever.
    fn drop(&mut self) {
        self.inner.proc.terminate();
    }
}

fn _ack_timeout() -> f64 {
    #[cfg(test)]
    {
        if let Some(timeout) = tests::ack_timeout_override() {
            return timeout;
        }
    }
    _ACK_TIMEOUT
}

// --------------------------------------------------------------------------
// daemon lifecycle
// --------------------------------------------------------------------------

/// True when the socket exists and its recorded daemon pid is alive.
///
/// No ACP traffic: the leader's socket protocol is private, so liveness is the
/// pidfile plus `kill(pid, 0)` rather than a handshake.
pub fn probe_socket(socket_path: &Path) -> bool {
    if !socket_path.exists() {
        return false;
    }
    let pid: libc::pid_t = match fs::read_to_string(socket_path.with_extension("pid")) {
        Ok(text) => match text.trim().parse() {
            Ok(pid) => pid,
            Err(_) => return false,
        },
        Err(_) => return false,
    };
    unsafe { libc::kill(pid, 0) == 0 }
}

/// The spawned leader as `_spawn_daemon_key` sees it (Python `Popen` handle).
trait DaemonChild: Send {
    fn pid(&self) -> u32;
    fn poll(&self) -> Option<i32>;
    fn terminate(&self);
}

#[cfg_attr(test, allow(dead_code))]
struct RealDaemonChild(Mutex<Child>);

impl DaemonChild for RealDaemonChild {
    fn pid(&self) -> u32 {
        self.0.lock().unwrap().id()
    }

    fn poll(&self) -> Option<i32> {
        let mut child = self.0.lock().unwrap();
        match child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or_else(|| {
                use std::os::unix::process::ExitStatusExt;
                -status.signal().unwrap_or(1)
            })),
            Ok(None) => None,
            Err(_) => Some(-1),
        }
    }

    fn terminate(&self) {
        let mut child = self.0.lock().unwrap();
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
    }
}

#[cfg_attr(test, allow(dead_code))]
fn _spawn_leader_real(
    argv: &[String],
    env: &HashMap<String, String>,
) -> io::Result<Box<dyn DaemonChild>> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        use std::os::unix::process::CommandExt;
        // Python start_new_session=True: detach from the short-lived CLI.
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(Box::new(RealDaemonChild(Mutex::new(cmd.spawn()?))))
}

fn _spawn_leader(
    argv: &[String],
    env: &HashMap<String, String>,
) -> io::Result<Box<dyn DaemonChild>> {
    #[cfg(test)]
    {
        tests::daemon_spawn_override(argv, env)
    }
    #[cfg(not(test))]
    {
        _spawn_leader_real(argv, env)
    }
}

/// Ensure the leader daemon the pane addresses is listening.
///
/// Idempotent: a live daemon on the resolved key's socket is reused (a tagged
/// member pane and its spawner reach the same member daemon). The daemon env
/// carries `TMUX_PANE` (shell tools report the right pane) and, for a
/// member key, `HIVE_TEAM`/`HIVE_MEMBER`.
pub fn spawn_daemon(pane: &str) -> bool {
    _spawn_daemon_key(
        &resolve_pane_key(pane),
        _daemon_env_for_pane(pane),
        "grok",
        _DAEMON_START_TIMEOUT,
    )
}

/// Ensure the member's leader daemon is listening — no pane involved.
///
/// The headless spawn lane: env carries the member identity only (no
/// `TMUX_PANE` — there is no pane to report).
pub fn spawn_member_daemon(team: &str, member: &str) -> bool {
    let mut env: HashMap<String, String> = env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| {
            !(key.starts_with("CLAUDE")
                || key.starts_with("ANTHROPIC")
                || matches!(
                    key.as_str(),
                    "CODEX_THREAD_ID" | "HIVE_TEAM" | "HIVE_MEMBER" | "TMUX_PANE" | "TMUX"
                ))
        })
        .collect();
    env.insert("HIVE_TEAM".to_string(), team.to_string());
    env.insert("HIVE_MEMBER".to_string(), member.to_string());
    _spawn_daemon_key(
        &member_key(team, member),
        env,
        "grok",
        _DAEMON_START_TIMEOUT,
    )
}

/// Start (or reuse) the leader daemon on *key*'s socket.
///
/// `start_new_session` detaches it from the short-lived CLI; the hived
/// reaps member daemons the registry no longer lists, and pane-keyed ones
/// when their pane dies.
fn _spawn_daemon_key(
    key: &str,
    env: HashMap<String, String>,
    grok_bin: &str,
    timeout: f64,
) -> bool {
    let sock = socket_path_for_key(key);
    if let Some(parent) = sock.parent() {
        if fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    if sock.exists() {
        if probe_socket(&sock) {
            return true;
        }
        let _ = fs::remove_file(&sock); // stale socket from a dead daemon
    }
    let argv: Vec<String> = vec![
        grok_bin.to_string(),
        "agent".to_string(),
        "leader".to_string(),
        "--leader-socket".to_string(),
        sock.to_string_lossy().into_owned(),
        "--no-auto-update".to_string(),
        "--no-exit-on-disconnect".to_string(),
    ];
    let child = match _spawn_leader(&argv, &env) {
        Ok(child) => child,
        Err(_) => return false,
    };
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    while Instant::now() < deadline {
        if child.poll().is_some() {
            return false; // died before binding
        }
        if sock.exists() {
            let _ = fs::write(sock.with_extension("pid"), child.pid().to_string());
            return true;
        }
        thread::sleep(Duration::from_millis(200));
    }
    child.terminate();
    false
}

/// Daemon keys that currently have a leader socket on disk.
pub fn list_daemon_keys() -> Vec<String> {
    let root = grok_home().join("hive");
    if !root.is_dir() {
        return Vec::new();
    }
    let mut keys = Vec::new();
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if let Some(key) = _key_from_socket_name(name) {
                    keys.push(key);
                }
            }
        }
    }
    keys
}

/// SIGTERM the pid's process group, escalating to SIGKILL if it lingers.
///
/// spawn_daemon uses `start_new_session`, so the leader is a process-group
/// leader and its children share the group; `killpg` reaps them together.
fn _terminate_process_group(pid: libc::pid_t) {
    #[cfg(test)]
    {
        if tests::terminate_pg_override(pid) {
            return;
        }
    }
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid < 0 {
        return;
    }
    for sig in [libc::SIGTERM, libc::SIGKILL] {
        if unsafe { libc::killpg(pgid, sig) } != 0 {
            return;
        }
        for _ in 0..10 {
            // up to ~1s before escalating
            if unsafe { libc::kill(pid, 0) } != 0 {
                return; // exited
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Stop a key's leader and remove its socket, pidfile and session record.
pub fn kill_daemon_key(key: &str) {
    let sock = socket_path_for_key(key);
    let pidfile = sock.with_extension("pid");
    let pid: Option<libc::pid_t> = fs::read_to_string(&pidfile)
        .ok()
        .and_then(|text| text.trim().parse().ok());
    if let Some(pid) = pid {
        _terminate_process_group(pid);
    }
    for path in [
        sock.clone(),
        sock.with_extension("lock"),
        pidfile,
        sock.with_extension("session"),
    ] {
        let _ = fs::remove_file(path);
    }
}

/// Stop the leader the pane addresses (member daemon for a tagged pane).
pub fn kill_pane_daemon(pane: &str) {
    kill_daemon_key(&resolve_pane_key(pane));
}

// --------------------------------------------------------------------------
// per-pane client pool (hived-side)
// --------------------------------------------------------------------------

/// What the pool's delivery paths need from a client. GrokStdioClient is the
/// only production implementation; tests substitute fakes (the Python
/// per-instance `_client_for_key` monkeypatch). `Err` models a raising
/// client — any client failure is a transport failure.
pub trait LeaderClient: Send + Sync {
    fn prompt(&self, _text: &str) -> Result<bool> {
        unreachable!("prompt not expected on this client")
    }
    fn cancel(&self) -> Result<bool> {
        unreachable!("cancel not expected on this client")
    }
    fn compact(&self) -> &'static str {
        unreachable!("compact not expected on this client")
    }
    fn runtime(&self) -> Option<SessionRuntime> {
        unreachable!("runtime not expected on this client")
    }
}

impl LeaderClient for GrokStdioClient {
    fn prompt(&self, text: &str) -> Result<bool> {
        Ok(GrokStdioClient::prompt(self, text))
    }

    fn cancel(&self) -> Result<bool> {
        Ok(GrokStdioClient::cancel(self))
    }

    fn compact(&self) -> &'static str {
        GrokStdioClient::compact(self)
    }

    fn runtime(&self) -> Option<SessionRuntime> {
        GrokStdioClient::runtime(self)
    }
}

#[derive(Default)]
struct PoolState {
    clients: HashMap<String, Arc<GrokStdioClient>>,
    cooldown: HashMap<String, Instant>,
}

/// One persistent stdio client per daemon key.
///
/// The hived reads runtime every tick; each client's reader thread keeps
/// its session state current between calls. Clients are created lazily the
/// first time a read finds both a socket and a session record, and a dead
/// one is dropped and retried after a cooldown so a missing daemon does not
/// storm subprocess spawns.
pub struct GrokClientPool {
    state: Mutex<PoolState>,
    #[cfg(test)]
    client_override: Mutex<Option<Box<dyn Fn(&str) -> Option<Arc<dyn LeaderClient>> + Send>>>,
}

impl GrokClientPool {
    pub fn new() -> GrokClientPool {
        GrokClientPool {
            state: Mutex::new(PoolState::default()),
            #[cfg(test)]
            client_override: Mutex::new(None),
        }
    }

    pub fn runtime_for_key(&self, key: &str) -> Option<SessionRuntime> {
        self._acting_client(key)?.runtime()
    }

    /// Bring the stdio client online for a key (called at spawn time).
    pub fn connect_key(&self, key: &str) -> bool {
        self._acting_client(key).is_some()
    }

    /// Deliver text as a prompt over the key's leader.
    ///
    /// Returns [`PROMPT_QUEUED`] when the leader echoed the prompt back, else
    /// None: no daemon, no session record, an rpc error, or an ack timeout.
    /// A busy session is not bounced — the leader queues the prompt FIFO and
    /// runs it when the current turn ends, the same as typing into the TUI.
    pub fn send_to_key(&self, key: &str, text: &str) -> Option<&'static str> {
        let client = self._acting_client(key)?;
        match client.prompt(text) {
            Ok(true) => Some(PROMPT_QUEUED),
            _ => None,
        }
    }

    /// Cancel the running turn over the key's leader.
    ///
    /// Returns [`CANCEL_SENT`] when the notification went out on a loaded
    /// session, else None: no daemon, no session record, or a dead pipe.
    pub fn interrupt_key(&self, key: &str) -> Option<&'static str> {
        let client = self._acting_client(key)?;
        match client.cancel() {
            Ok(true) => Some(CANCEL_SENT),
            _ => None,
        }
    }

    pub fn compact_key(&self, key: &str) -> &'static str {
        match self._acting_client(key) {
            Some(client) => client.compact(),
            None => "unavailable",
        }
    }

    /// The Python per-instance `_client_for_key` monkeypatch seam.
    fn _acting_client(&self, key: &str) -> Option<Arc<dyn LeaderClient>> {
        #[cfg(test)]
        {
            if let Some(factory) = self.client_override.lock().unwrap().as_ref() {
                return factory(key);
            }
        }
        self._client_for_key(key)
            .map(|client| client as Arc<dyn LeaderClient>)
    }

    fn _client_for_key(&self, key: &str) -> Option<Arc<GrokStdioClient>> {
        // A relaunched grok on the same key mints a new session id, so the
        // record — not just the client's liveness — decides whether the bound
        // client is still the key's.
        let record = read_session_key(key);
        {
            let mut state = self.state.lock().unwrap();
            if let Some(client) = state.clients.get(key).cloned() {
                if client.is_alive()
                    && record.is_some()
                    && client.session_id().as_deref()
                        == record.as_ref().map(|(sid, _cwd)| sid.as_str())
                {
                    return Some(client);
                }
                client.close();
                state.clients.remove(key);
            }
            if let Some(until) = state.cooldown.get(key) {
                if Instant::now() < *until {
                    return None;
                }
            }
        }

        if record.is_none() || !probe_socket(&socket_path_for_key(key)) {
            self._set_cooldown(key);
            return None;
        }
        let client = match GrokStdioClient::new(key) {
            Ok(client) => Arc::new(client),
            Err(_) => {
                self._set_cooldown(key);
                return None;
            }
        };
        if !client.handshake() {
            client.close();
            self._set_cooldown(key);
            return None;
        }
        self.state
            .lock()
            .unwrap()
            .clients
            .insert(key.to_string(), client.clone());
        Some(client)
    }

    fn _set_cooldown(&self, key: &str) {
        self.state.lock().unwrap().cooldown.insert(
            key.to_string(),
            Instant::now() + Duration::from_secs_f64(_CONNECT_COOLDOWN),
        );
    }

    pub fn drop(&self, pane: &str) {
        self.drop_key(&resolve_pane_key(pane));
    }

    /// Drop every client attached to *key*'s socket (reap path).
    pub fn drop_key(&self, key: &str) {
        let sock = socket_path_for_key(key).to_string_lossy().into_owned();
        let doomed: Vec<Arc<GrokStdioClient>> = {
            let mut state = self.state.lock().unwrap();
            let keys: Vec<String> = state
                .clients
                .iter()
                .filter(|(_key, client)| client.socket_path == sock)
                .map(|(key, _client)| key.clone())
                .collect();
            keys.into_iter()
                .filter_map(|key| state.clients.remove(&key))
                .collect()
        };
        for client in doomed {
            client.close();
        }
    }

    /// `create_member_session`'s adopt path: Python pokes `pool()._clients`
    /// directly under `pool()._lock`.
    fn _adopt_client(&self, key: &str, client: Arc<GrokStdioClient>) {
        let existing = {
            let mut state = self.state.lock().unwrap();
            let existing = state.clients.remove(key);
            state.clients.insert(key.to_string(), client);
            existing
        };
        if let Some(existing) = existing {
            existing.close();
        }
    }
}

impl Default for GrokClientPool {
    fn default() -> Self {
        GrokClientPool::new()
    }
}

static _POOL: OnceLock<GrokClientPool> = OnceLock::new();

pub fn pool() -> &'static GrokClientPool {
    _POOL.get_or_init(GrokClientPool::new)
}

pub fn runtime_for_pane(pane: &str) -> Option<SessionRuntime> {
    pool().runtime_for_key(&resolve_pane_key(pane))
}

pub fn runtime_for_key(key: &str) -> Option<SessionRuntime> {
    pool().runtime_for_key(key)
}

pub fn connect_pane(pane: &str) -> bool {
    pool().connect_key(&resolve_pane_key(pane))
}

pub fn connect_key(key: &str) -> bool {
    pool().connect_key(key)
}

pub fn send_to_pane(pane: &str, text: &str) -> Option<&'static str> {
    pool().send_to_key(&resolve_pane_key(pane), text)
}

pub fn send_to_key(key: &str, text: &str) -> Option<&'static str> {
    pool().send_to_key(key, text)
}

pub fn interrupt_pane(pane: &str) -> Option<&'static str> {
    pool().interrupt_key(&resolve_pane_key(pane))
}

pub fn interrupt_key(key: &str) -> Option<&'static str> {
    pool().interrupt_key(key)
}

pub fn compact_pane(pane: &str) -> &'static str {
    pool().compact_key(&resolve_pane_key(pane))
}

/// Session id hive minted for this pane, from its session record.
pub fn session_id_for_pane(pane: &str) -> Option<String> {
    read_pane_session(pane).map(|(session_id, _cwd)| session_id)
}

/// Materialize the member's session on its leader — the headless spawn.
///
/// Ensures the member daemon, asks it for `session/new` with hive's
/// minted id, and records the session beside the socket on success. The
/// creating client stays in the pool, already bound and folding the
/// session's notifications.
pub fn create_member_session(team: &str, member: &str, session_id: &str, cwd: &str) -> bool {
    if !spawn_member_daemon(team, member) {
        return false;
    }
    let key = member_key(team, member);
    let client = match GrokStdioClient::new(&key) {
        Ok(client) => Arc::new(client),
        Err(_) => return false,
    };
    if !client.new_session(session_id, cwd) {
        client.close();
        return false;
    }
    if write_session_key(&key, session_id, cwd).is_err() {
        return false;
    }
    pool()._adopt_client(&key, client);
    true
}

// --------------------------------------------------------------------------
// tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::TEST_ENV_LOCK;
    use std::cell::{Cell, RefCell};
    use std::os::unix::net::UnixStream;
    use std::sync::MutexGuard;

    const SID: &str = "11111111-2222-3333-4444-555555555555";
    const CWD: &str = "/w/project";

    // ---- test seams ------------------------------------------------------

    type StdioSpawn = Box<dyn FnMut(&[String]) -> io::Result<Arc<dyn LeaderProc>>>;
    type DaemonSpawn =
        Box<dyn FnMut(&[String], &HashMap<String, String>) -> io::Result<Box<dyn DaemonChild>>>;

    thread_local! {
        static PANE_OPTION_OVERRIDE: RefCell<Option<Box<dyn Fn(&str, &str) -> Option<String>>>> =
            RefCell::new(None);
        static STDIO_SPAWN_OVERRIDE: RefCell<Option<StdioSpawn>> = RefCell::new(None);
        static DAEMON_SPAWN_OVERRIDE: RefCell<Option<DaemonSpawn>> = RefCell::new(None);
        static TERMINATE_PG_OVERRIDE: RefCell<Option<Box<dyn FnMut(libc::pid_t)>>> =
            RefCell::new(None);
        static ACK_TIMEOUT_OVERRIDE: Cell<Option<f64>> = const { Cell::new(None) };
    }

    /// Panes resolve to their raw pane key unless a test tags them — the
    /// Python autouse `_untagged_panes` fixture (never the real tmux).
    pub(super) fn pane_option_override(pane: &str, key: &str) -> Option<String> {
        PANE_OPTION_OVERRIDE.with(|slot| slot.borrow().as_ref().and_then(|f| f(pane, key)))
    }

    pub(super) fn stdio_spawn_override(argv: &[String]) -> io::Result<Arc<dyn LeaderProc>> {
        STDIO_SPAWN_OVERRIDE.with(|slot| match slot.borrow_mut().as_mut() {
            Some(factory) => factory(argv),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no stdio spawn override in tests",
            )),
        })
    }

    pub(super) fn daemon_spawn_override(
        argv: &[String],
        env: &HashMap<String, String>,
    ) -> io::Result<Box<dyn DaemonChild>> {
        DAEMON_SPAWN_OVERRIDE.with(|slot| match slot.borrow_mut().as_mut() {
            Some(factory) => factory(argv, env),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no daemon spawn override in tests",
            )),
        })
    }

    /// True when a test override consumed the terminate call.
    pub(super) fn terminate_pg_override(pid: libc::pid_t) -> bool {
        TERMINATE_PG_OVERRIDE.with(|slot| match slot.borrow_mut().as_mut() {
            Some(record) => {
                record(pid);
                true
            }
            None => false,
        })
    }

    pub(super) fn ack_timeout_override() -> Option<f64> {
        ACK_TIMEOUT_OVERRIDE.with(|slot| slot.get())
    }

    fn set_pane_options(tags: HashMap<(String, String), String>) {
        PANE_OPTION_OVERRIDE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |pane, key| {
                tags.get(&(pane.to_string(), key.to_string())).cloned()
            }))
        });
    }

    fn set_stdio_spawn(
        factory: impl FnMut(&[String]) -> io::Result<Arc<dyn LeaderProc>> + 'static,
    ) {
        STDIO_SPAWN_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(Box::new(factory)));
    }

    fn set_daemon_spawn(
        factory: impl FnMut(&[String], &HashMap<String, String>) -> io::Result<Box<dyn DaemonChild>>
            + 'static,
    ) {
        DAEMON_SPAWN_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(Box::new(factory)));
    }

    fn set_terminate_pg(record: impl FnMut(libc::pid_t) + 'static) {
        TERMINATE_PG_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(Box::new(record)));
    }

    /// Serialized test bed: env lock held, GROK_HOME pinned to a tempdir,
    /// key cache and every thread-local seam reset.
    struct TestBed {
        _guard: MutexGuard<'static, ()>,
        tmp: tempfile::TempDir,
    }

    fn setup() -> TestBed {
        let guard = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        _key_cache().lock().unwrap().clear();
        PANE_OPTION_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
        STDIO_SPAWN_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
        DAEMON_SPAWN_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
        TERMINATE_PG_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
        ACK_TIMEOUT_OVERRIDE.with(|slot| slot.set(None));
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("GROK_HOME", tmp.path());
        TestBed { _guard: guard, tmp }
    }

    // ---- fake subprocess -------------------------------------------------

    type Responder = Box<dyn Fn(&Value) -> Vec<Value> + Send + Sync>;

    struct FakeProc {
        lines: Mutex<Vec<String>>,
        writer: Mutex<Option<UnixStream>>,
        reader: Mutex<Option<UnixStream>>,
        responder: Mutex<Option<Responder>>,
        write_fail: AtomicBool,
        terminated: AtomicBool,
        returncode: Mutex<Option<i32>>,
    }

    impl FakeProc {
        fn new(responder: Option<Responder>) -> Arc<FakeProc> {
            let (reader, writer) = UnixStream::pair().unwrap();
            Arc::new(FakeProc {
                lines: Mutex::new(Vec::new()),
                writer: Mutex::new(Some(writer)),
                reader: Mutex::new(Some(reader)),
                responder: Mutex::new(responder),
                write_fail: AtomicBool::new(false),
                terminated: AtomicBool::new(false),
                returncode: Mutex::new(None),
            })
        }

        fn feed(&self, message: &Value) {
            if let Some(writer) = self.writer.lock().unwrap().as_mut() {
                let _ = writer.write_all(format!("{message}\n").as_bytes());
            }
        }

        fn sent(&self) -> Vec<Value> {
            self.lines
                .lock()
                .unwrap()
                .iter()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }

        fn eof(&self) {
            *self.writer.lock().unwrap() = None;
        }

        fn set_write_fail(&self) {
            self.write_fail.store(true, Ordering::SeqCst);
        }
    }

    impl LeaderProc for FakeProc {
        fn write_line(&self, line: &str) -> io::Result<()> {
            if self.write_fail.load(Ordering::SeqCst) {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe"));
            }
            self.lines.lock().unwrap().push(line.to_string());
            let replies = {
                let responder = self.responder.lock().unwrap();
                match (responder.as_ref(), serde_json::from_str::<Value>(line)) {
                    (Some(respond), Ok(msg)) => respond(&msg),
                    _ => Vec::new(),
                }
            };
            for reply in replies {
                self.feed(&reply);
            }
            Ok(())
        }

        fn take_stdout(&self) -> Option<Box<dyn Read + Send>> {
            self.reader
                .lock()
                .unwrap()
                .take()
                .map(|stream| Box::new(stream) as Box<dyn Read + Send>)
        }

        fn poll(&self) -> Option<i32> {
            *self.returncode.lock().unwrap()
        }

        fn terminate(&self) {
            self.terminated.store(true, Ordering::SeqCst);
            *self.returncode.lock().unwrap() = Some(-15);
        }

        fn wait(&self, _timeout: f64) {}

        fn close_stdin(&self) {}
    }

    fn _ok(msg: &Value, result: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": msg["id"], "result": result})
    }

    /// Answers the handshake; `extra` handles everything else.
    fn responder(extra: Option<Responder>, replay: Vec<Value>) -> Responder {
        Box::new(
            move |msg: &Value| match msg.get("method").and_then(Value::as_str) {
                Some("initialize") => vec![_ok(msg, json!({"protocolVersion": 1}))],
                Some("session/load") => {
                    let mut replies = replay.clone();
                    replies.push(_ok(msg, json!({"models": {"currentModelId": "grok-4.6"}})));
                    replies
                }
                _ => extra.as_ref().map(|e| e(msg)).unwrap_or_default(),
            },
        )
    }

    /// The Python grok_client fixture factory for pane %19.
    fn make(
        respond: Option<Responder>,
        session: Option<(&str, &str)>,
        pane: &str,
    ) -> (Arc<GrokStdioClient>, Arc<FakeProc>) {
        if let Some((session_id, cwd)) = session {
            write_pane_session(pane, session_id, cwd).unwrap();
        }
        let proc = FakeProc::new(respond);
        let handout = proc.clone();
        set_stdio_spawn(move |_argv| Ok(handout.clone() as Arc<dyn LeaderProc>));
        let client = Arc::new(GrokStdioClient::new(&resolve_pane_key(pane)).unwrap());
        (client, proc)
    }

    fn _loaded(
        respond: Option<Responder>,
        replay: Vec<Value>,
    ) -> (Arc<GrokStdioClient>, Arc<FakeProc>) {
        let respond = respond.unwrap_or_else(|| responder(None, replay));
        let (client, proc) = make(Some(respond), Some((SID, CWD)), "%19");
        assert!(client.handshake());
        (client, proc)
    }

    fn teardown(client: &GrokStdioClient, proc: &FakeProc) {
        client.inner.closed.store(true, Ordering::SeqCst);
        proc.eof();
        if let Some(handle) = client.reader.lock().unwrap().take() {
            let _ = handle.join();
        }
    }

    fn _settle(
        client: &GrokStdioClient,
        predicate: impl Fn(&SessionRuntime) -> bool,
    ) -> SessionRuntime {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Some(runtime) = client.runtime() {
                if predicate(&runtime) {
                    return runtime;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("runtime never matched: {:?}", client.runtime());
    }

    fn _settle_sent(proc: &FakeProc, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            for msg in proc.sent() {
                if predicate(&msg) {
                    return msg;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("no matching write: {:?}", proc.sent());
    }

    fn _update_for(session_id: &str, kind: &str, fields: Value) -> Value {
        let mut update = json!({"sessionUpdate": kind});
        if let (Some(target), Some(extra)) = (update.as_object_mut(), fields.as_object()) {
            for (key, value) in extra {
                target.insert(key.clone(), value.clone());
            }
        }
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": session_id, "update": update},
        })
    }

    fn _update(kind: &str, fields: Value) -> Value {
        _update_for(SID, kind, fields)
    }

    fn _activity_for(activity: &str, session_id: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "_x.ai/sessions/changed",
            "params": {"upserted": [
                {"sessionId": session_id, "activity": activity, "resident": true},
            ]},
        })
    }

    fn _activity(activity: &str) -> Value {
        _activity_for(activity, SID)
    }

    // ----------------------------------------------------------------------
    // handshake
    // ----------------------------------------------------------------------

    #[test]
    fn test_handshake_sends_initialize_then_session_load() {
        let _bed = setup();
        let (client, proc) = _loaded(None, vec![]);
        let sent = proc.sent();
        let methods: Vec<&str> = sent
            .iter()
            .map(|msg| msg["method"].as_str().unwrap())
            .collect();
        assert_eq!(methods, vec!["initialize", "session/load"]);
        assert_eq!(
            sent[0]["params"],
            json!({
                "protocolVersion": 1,
                "clientInfo": {"name": "hive", "version": "1"},
                "clientCapabilities": {},
            })
        );
        assert_eq!(
            sent[1]["params"],
            json!({"sessionId": SID, "cwd": CWD, "mcpServers": []})
        );
        teardown(&client, &proc);
    }

    #[test]
    fn test_handshake_stops_without_pane_session_file() {
        let _bed = setup();
        let (client, proc) = make(Some(responder(None, vec![])), None, "%19");
        assert!(!client.handshake());
        assert!(proc.sent().is_empty());
        teardown(&client, &proc);
    }

    #[test]
    fn test_handshake_false_when_load_errors() {
        let _bed = setup();
        let respond: Responder = Box::new(|msg: &Value| {
            if msg.get("method").and_then(Value::as_str) == Some("initialize") {
                return vec![_ok(msg, json!({"protocolVersion": 1}))];
            }
            vec![json!({
                "jsonrpc": "2.0",
                "id": msg["id"],
                "error": {"code": -32602, "message": "unknown session id"},
            })]
        });
        let (client, proc) = make(Some(respond), Some((SID, CWD)), "%19");
        assert!(!client.handshake());
        teardown(&client, &proc);
    }

    #[test]
    fn test_notifications_before_load_response_are_discarded() {
        let _bed = setup();
        let replay = vec![
            _update(
                "agent_message_chunk",
                json!({"content": {"type": "text", "text": "old turn"}}),
            ),
            _activity("working"),
        ];
        let (client, proc) = _loaded(None, replay);
        assert!(client.runtime().is_none()); // replay is not evidence of a live turn
        teardown(&client, &proc);
    }

    #[test]
    fn test_notification_right_behind_the_load_response_is_folded() {
        // A live turn queued behind the load response must not count as replay.
        let _bed = setup();
        let respond: Responder =
            Box::new(
                |msg: &Value| match msg.get("method").and_then(Value::as_str) {
                    Some("initialize") => vec![_ok(msg, json!({"protocolVersion": 1}))],
                    Some("session/load") => vec![
                        _ok(msg, json!({"models": {"currentModelId": "grok-4.6"}})),
                        _activity("working"),
                    ],
                    _ => vec![],
                },
            );
        let (client, proc) = make(Some(respond), Some((SID, CWD)), "%19");
        assert!(client.handshake());
        _settle(&client, |rt| rt.busy);
        teardown(&client, &proc);
    }

    #[test]
    fn test_handshake_fails_fast_when_the_child_dies() {
        let _bed = setup();
        let holder: Arc<Mutex<Option<Arc<FakeProc>>>> = Arc::new(Mutex::new(None));
        let respond_holder = holder.clone();
        let respond: Responder = Box::new(move |_msg: &Value| {
            // the stdio child dies instead of answering
            if let Some(proc) = respond_holder.lock().unwrap().as_ref() {
                proc.eof();
            }
            vec![]
        });
        let (client, proc) = make(Some(respond), Some((SID, CWD)), "%19");
        *holder.lock().unwrap() = Some(proc.clone());
        let started = Instant::now();
        assert!(!client.handshake());
        // death, not the initialize timeout
        assert!(started.elapsed() < Duration::from_secs(1));
        teardown(&client, &proc);
    }

    // ----------------------------------------------------------------------
    // notification folding
    // ----------------------------------------------------------------------

    #[test]
    fn test_activity_working_marks_busy_and_idle_closes_turn() {
        let _bed = setup();
        let (client, proc) = _loaded(None, vec![]);
        proc.feed(&_activity("working"));
        assert_eq!(
            _settle(&client, |rt| rt.busy).session_id.as_deref(),
            Some(SID)
        );
        proc.feed(&_activity("idle"));
        let runtime = _settle(&client, |rt| !rt.busy);
        assert_eq!(runtime.turn_phase, "turn_closed");
        assert_eq!(runtime.input_state, "ready");
        teardown(&client, &proc);
    }

    #[test]
    fn test_message_chunks_mark_user_prompt_pending() {
        let _bed = setup();
        let (client, proc) = _loaded(None, vec![]);
        proc.feed(&_update(
            "agent_thought_chunk",
            json!({"content": {"type": "text", "text": "The"}}),
        ));
        let runtime = _settle(&client, |rt| rt.busy);
        assert_eq!(runtime.turn_phase, "user_prompt_pending");
        teardown(&client, &proc);
    }

    #[test]
    fn test_tool_call_phases_survive_streamed_chunks() {
        let _bed = setup();
        let (client, proc) = _loaded(None, vec![]);
        proc.feed(&_update(
            "tool_call",
            json!({"toolCallId": "c1", "status": "pending"}),
        ));
        assert!(_settle(&client, |rt| rt.turn_phase == "tool_open").busy);
        proc.feed(&_update(
            "tool_call_update",
            json!({"toolCallId": "c1", "status": "completed"}),
        ));
        _settle(&client, |rt| rt.turn_phase == "tool_result_pending_reply");
        proc.feed(&_update(
            "agent_message_chunk",
            json!({"content": {"type": "text", "text": "done"}}),
        ));
        thread::sleep(Duration::from_millis(50));
        assert_eq!(
            client.runtime().unwrap().turn_phase,
            "tool_result_pending_reply"
        );
        teardown(&client, &proc);
    }

    #[test]
    fn test_late_joined_tool_call_update_marks_busy() {
        // attaching mid-tool: the opening tool_call was never seen, the update is
        // the only evidence that a turn is running
        let _bed = setup();
        let (client, proc) = _loaded(None, vec![]);
        proc.feed(&_update(
            "tool_call_update",
            json!({"toolCallId": "c1", "status": "in_progress"}),
        ));
        _settle(&client, |rt| rt.busy);
        teardown(&client, &proc);
    }

    #[test]
    fn test_tool_call_update_clears_a_decided_permission() {
        let _bed = setup();
        let (client, proc) = _loaded(None, vec![]);
        proc.feed(&json!({
            "jsonrpc": "2.0",
            "id": 78,
            "method": "session/request_permission",
            "params": {"sessionId": SID, "toolCall": {"toolCallId": "c1"}, "options": []},
        }));
        _settle(&client, |rt| rt.input_state == "waiting_user");
        // the human answered at the TUI: the tool ran, so nothing waits on input
        proc.feed(&_update(
            "tool_call_update",
            json!({"toolCallId": "c1", "status": "completed"}),
        ));
        _settle(&client, |rt| rt.input_state == "ready");
        teardown(&client, &proc);
    }

    #[test]
    fn test_turn_completed_clears_busy() {
        let _bed = setup();
        let (client, proc) = _loaded(None, vec![]);
        proc.feed(&_activity("working"));
        _settle(&client, |rt| rt.busy);
        proc.feed(&json!({
            "jsonrpc": "2.0",
            "method": "_x.ai/session_notification",
            "params": {
                "sessionId": SID,
                "update": {"sessionUpdate": "turn_completed", "stop_reason": "end_turn"},
            },
        }));
        let runtime = _settle(&client, |rt| !rt.busy);
        assert_eq!(runtime.turn_phase, "turn_closed");
        assert_eq!(runtime.input_state, "ready");
        teardown(&client, &proc);
    }

    #[test]
    fn test_queued_entries_mark_input_backlog() {
        let _bed = setup();
        let (client, proc) = _loaded(None, vec![]);
        proc.feed(&json!({
            "jsonrpc": "2.0",
            "method": "_x.ai/queue/changed",
            "params": {
                "sessionId": SID,
                "entries": [{"id": "p1", "kind": "prompt", "text": "next", "position": 0}],
            },
        }));
        _settle(&client, |rt| rt.turn_phase == "input_backlog");
        teardown(&client, &proc);
    }

    #[test]
    fn test_other_session_notifications_are_ignored() {
        let _bed = setup();
        let (client, proc) = _loaded(None, vec![]);
        proc.feed(&_update(
            "tool_call",
            json!({"toolCallId": "c1", "status": "pending"}),
        ));
        let baseline = _settle(&client, |rt| rt.turn_phase == "tool_open");
        proc.feed(&_activity_for("idle", "other-session"));
        proc.feed(&_update_for(
            "other-session",
            "agent_message_chunk",
            json!({"content": {"text": "hi"}}),
        ));
        // same-session no-op marker: the reader folds it only after the two lines
        // above, so its observed_at bump proves they were seen and dropped
        proc.feed(&_activity("working"));
        let runtime = _settle(&client, |rt| rt.observed_at > baseline.observed_at);
        assert!(runtime.busy);
        assert_eq!(runtime.turn_phase, "tool_open"); // the foreign idle never closed it
        assert_eq!(runtime.input_state, "");
        teardown(&client, &proc);
    }

    #[test]
    fn test_unknown_updates_are_ignored() {
        let _bed = setup();
        let (client, proc) = _loaded(None, vec![]);
        proc.feed(&_update(
            "available_commands_update",
            json!({"availableCommands": [{"name": "compact"}]}),
        ));
        let first = _settle(&client, |_rt| true);
        // the second ignored line is its own marker: an in-session notification
        // bumps observed_at even when nothing folds it
        proc.feed(&json!({
            "jsonrpc": "2.0",
            "method": "_x.ai/announcements/update",
            "params": {"sessionId": SID},
        }));
        let runtime = _settle(&client, |rt| rt.observed_at > first.observed_at);
        assert!(!runtime.busy);
        assert_eq!(runtime.turn_phase, "unknown_evidence");
        teardown(&client, &proc);
    }

    // ----------------------------------------------------------------------
    // prompt delivery
    // ----------------------------------------------------------------------

    fn on_prompt_queue_echo() -> Responder {
        Box::new(|msg: &Value| {
            if msg.get("method").and_then(Value::as_str) != Some("session/prompt") {
                return vec![];
            }
            let text = msg["params"]["prompt"][0]["text"].clone();
            vec![json!({
                "jsonrpc": "2.0",
                "method": "_x.ai/queue/changed",
                "params": {
                    "sessionId": SID,
                    "entries": [{"id": "p1", "kind": "prompt", "text": text, "position": 0}],
                },
            })]
        })
    }

    #[test]
    fn test_prompt_acks_on_queue_changed_echo() {
        let _bed = setup();
        let (client, proc) = _loaded(
            Some(responder(Some(on_prompt_queue_echo()), vec![])),
            vec![],
        );
        assert!(GrokStdioClient::prompt(&client, "hello grok"));
        let sent = proc.sent();
        let prompt_msg = sent.last().unwrap();
        assert_eq!(prompt_msg["method"], "session/prompt");
        assert_eq!(
            prompt_msg["params"],
            json!({
                "sessionId": SID,
                "prompt": [{"type": "text", "text": "hello grok"}],
            })
        );
        teardown(&client, &proc);
    }

    #[test]
    fn test_prompt_acks_on_running_text_echo() {
        let _bed = setup();
        let on_prompt: Responder = Box::new(|msg: &Value| {
            if msg.get("method").and_then(Value::as_str) != Some("session/prompt") {
                return vec![];
            }
            vec![json!({
                "jsonrpc": "2.0",
                "method": "_x.ai/queue/changed",
                "params": {
                    "sessionId": SID,
                    "entries": [],
                    "runningText": "hello grok",
                    "runningKind": "prompt",
                },
            })]
        });
        let (client, proc) = _loaded(Some(responder(Some(on_prompt), vec![])), vec![]);
        assert!(GrokStdioClient::prompt(&client, "hello grok"));
        teardown(&client, &proc);
    }

    #[test]
    fn test_prompt_acks_on_user_message_chunk() {
        let _bed = setup();
        let on_prompt: Responder = Box::new(|msg: &Value| {
            if msg.get("method").and_then(Value::as_str) != Some("session/prompt") {
                return vec![];
            }
            let text = msg["params"]["prompt"][0]["text"].clone();
            vec![_update(
                "user_message_chunk",
                json!({"content": {"type": "text", "text": text}}),
            )]
        });
        let (client, proc) = _loaded(Some(responder(Some(on_prompt), vec![])), vec![]);
        assert!(GrokStdioClient::prompt(&client, "hello grok"));
        teardown(&client, &proc);
    }

    #[test]
    fn test_prompt_false_on_error_response() {
        let _bed = setup();
        let on_prompt: Responder = Box::new(|msg: &Value| {
            if msg.get("method").and_then(Value::as_str) != Some("session/prompt") {
                return vec![];
            }
            vec![json!({
                "jsonrpc": "2.0",
                "id": msg["id"],
                "error": {"code": -32602, "message": "unknown session id"},
            })]
        });
        let (client, proc) = _loaded(Some(responder(Some(on_prompt), vec![])), vec![]);
        assert!(!GrokStdioClient::prompt(&client, "hello grok"));
        teardown(&client, &proc);
    }

    #[test]
    fn test_prompt_false_when_never_acked() {
        let _bed = setup();
        ACK_TIMEOUT_OVERRIDE.with(|slot| slot.set(Some(0.05)));
        let (client, proc) = _loaded(None, vec![]); // nothing answers session/prompt
        assert!(!GrokStdioClient::prompt(&client, "hello grok"));
        teardown(&client, &proc);
    }

    #[test]
    fn test_prompt_echo_of_another_text_does_not_ack() {
        let _bed = setup();
        ACK_TIMEOUT_OVERRIDE.with(|slot| slot.set(Some(0.05)));
        let on_prompt: Responder = Box::new(|msg: &Value| {
            if msg.get("method").and_then(Value::as_str) != Some("session/prompt") {
                return vec![];
            }
            vec![_update(
                "user_message_chunk",
                json!({"content": {"type": "text", "text": "someone else"}}),
            )]
        });
        let (client, proc) = _loaded(Some(responder(Some(on_prompt), vec![])), vec![]);
        assert!(!GrokStdioClient::prompt(&client, "hello grok"));
        teardown(&client, &proc);
    }

    // ----------------------------------------------------------------------
    // permission requests
    // ----------------------------------------------------------------------

    #[test]
    fn test_permission_request_is_cancelled_and_marks_waiting_user() {
        let _bed = setup();
        let (client, proc) = _loaded(None, vec![]);
        proc.feed(&json!({
            "jsonrpc": "2.0",
            "id": 77,
            "method": "session/request_permission",
            "params": {
                "sessionId": SID,
                "toolCall": {"toolCallId": "c1", "title": "rm -rf"},
                "options": [{"optionId": "a", "name": "Allow", "kind": "allow_once"}],
            },
        }));
        let answer = _settle_sent(&proc, |msg| {
            msg.get("id").and_then(Value::as_i64) == Some(77)
        });
        assert_eq!(
            answer["result"],
            json!({"outcome": {"outcome": "cancelled"}})
        );
        _settle(&client, |rt| rt.input_state == "waiting_user");
        teardown(&client, &proc);
    }

    // ----------------------------------------------------------------------
    // interrupt
    // ----------------------------------------------------------------------

    #[test]
    fn test_cancel_writes_a_bare_notification_for_the_session() {
        // ACP cancel is a notification: the leader answers a cancel carrying an
        // id with -32601 and keeps running the turn, so the write must have no id.
        let _bed = setup();
        let (client, proc) = _loaded(None, vec![]);
        assert!(GrokStdioClient::cancel(&client));
        let sent = proc.sent();
        let cancel = sent.last().unwrap();
        assert_eq!(cancel["method"], "session/cancel");
        assert_eq!(cancel["params"], json!({"sessionId": SID}));
        assert!(cancel.get("id").is_none());
        teardown(&client, &proc);
    }

    #[test]
    fn test_cancel_false_without_a_loaded_session() {
        let _bed = setup();
        // no handshake -> no session bound
        let (client, proc) = make(Some(responder(None, vec![])), Some((SID, CWD)), "%19");
        assert!(!GrokStdioClient::cancel(&client));
        assert!(!proc
            .sent()
            .iter()
            .any(|msg| msg.get("method").and_then(Value::as_str) == Some("session/cancel")));
        teardown(&client, &proc);
    }

    #[test]
    fn test_cancel_false_when_the_pipe_is_dead() {
        let _bed = setup();
        let (client, proc) = _loaded(None, vec![]);
        proc.set_write_fail();
        assert!(!GrokStdioClient::cancel(&client));
        teardown(&client, &proc);
    }

    // ----------------------------------------------------------------------
    // compaction
    // ----------------------------------------------------------------------

    fn on_compact_ok() -> Responder {
        Box::new(|msg: &Value| {
            if msg.get("method").and_then(Value::as_str) == Some("x.ai/compact_conversation") {
                vec![_ok(msg, json!({}))]
            } else {
                vec![]
            }
        })
    }

    #[test]
    fn test_compact_returns_compacted_when_idle() {
        let _bed = setup();
        let (client, proc) = _loaded(Some(responder(Some(on_compact_ok()), vec![])), vec![]);
        assert_eq!(GrokStdioClient::compact(&client), "compacted");
        let sent = proc.sent();
        assert_eq!(sent.last().unwrap()["params"], json!({"sessionId": SID}));
        teardown(&client, &proc);
    }

    #[test]
    fn test_compact_defers_while_busy() {
        let _bed = setup();
        let on_compact: Responder = Box::new(|msg: &Value| {
            if msg.get("method").and_then(Value::as_str) == Some("x.ai/compact_conversation") {
                panic!("must not compact a busy session");
            }
            vec![]
        });
        let (client, proc) = _loaded(Some(responder(Some(on_compact), vec![])), vec![]);
        proc.feed(&_activity("working"));
        _settle(&client, |rt| rt.busy);
        assert_eq!(GrokStdioClient::compact(&client), "busy");
        teardown(&client, &proc);
    }

    #[test]
    fn test_compact_unavailable_on_error() {
        let _bed = setup();
        let on_compact: Responder = Box::new(|msg: &Value| {
            if msg.get("method").and_then(Value::as_str) != Some("x.ai/compact_conversation") {
                return vec![];
            }
            vec![json!({
                "jsonrpc": "2.0",
                "id": msg["id"],
                "error": {"code": -32601, "message": "unsupported"},
            })]
        });
        let (client, proc) = _loaded(Some(responder(Some(on_compact), vec![])), vec![]);
        assert_eq!(GrokStdioClient::compact(&client), "unavailable");
        teardown(&client, &proc);
    }

    // ----------------------------------------------------------------------
    // process lifecycle
    // ----------------------------------------------------------------------

    #[test]
    fn test_client_close_terminates_the_subprocess() {
        let _bed = setup();
        let (client, proc) = make(Some(responder(None, vec![])), Some((SID, CWD)), "%19");
        assert!(client.is_alive());
        client.close();
        assert!(proc.terminated.load(Ordering::SeqCst));
        assert!(!client.is_alive());
        teardown(&client, &proc);
    }

    #[test]
    fn test_client_dies_on_stdout_eof() {
        let _bed = setup();
        let (client, proc) = _loaded(None, vec![]);
        proc.eof();
        let deadline = Instant::now() + Duration::from_secs(2);
        while client.is_alive() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!client.is_alive());
        teardown(&client, &proc);
    }

    #[test]
    fn test_stdio_argv_targets_the_pane_socket() {
        let _bed = setup();
        write_pane_session("%19", SID, CWD).unwrap();
        let proc = FakeProc::new(Some(responder(None, vec![])));
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_argv = seen.clone();
        let handout = proc.clone();
        set_stdio_spawn(move |argv| {
            *seen_argv.lock().unwrap() = argv.to_vec();
            Ok(handout.clone() as Arc<dyn LeaderProc>)
        });
        let client = GrokStdioClient::new(&resolve_pane_key("%19")).unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                "grok".to_string(),
                "agent".to_string(),
                "--leader".to_string(),
                "stdio".to_string(),
                "--leader-socket".to_string(),
                pane_socket_path("%19").to_string_lossy().into_owned(),
            ]
        );
        // Python also asserts Popen kwargs (text=True, bufsize=1,
        // stderr=DEVNULL); those are subprocess-construction details baked
        // into RealProc::spawn and not observable through the spawn seam.
        teardown(&client, &proc);
    }

    // ----------------------------------------------------------------------
    // paths and pane session records
    // ----------------------------------------------------------------------

    #[test]
    fn test_pane_socket_path_under_grok_home() {
        let _bed = setup();
        let path = pane_socket_path("%19");
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "hive");
        assert!(path.to_string_lossy().ends_with("hive/p19.sock"));
    }

    #[test]
    fn test_pane_socket_path_stays_under_unix_limit() {
        let _bed = setup();
        env::remove_var("GROK_HOME");
        assert!(pane_socket_path("%19").to_string_lossy().len() < 104);
    }

    #[test]
    fn test_sibling_paths_share_the_socket_stem() {
        let _bed = setup();
        assert_eq!(pane_pidfile_path("%19").file_name().unwrap(), "p19.pid");
        assert_eq!(pane_session_path("%19").file_name().unwrap(), "p19.session");
    }

    #[test]
    fn test_pane_session_round_trip() {
        let _bed = setup();
        write_pane_session("%19", SID, CWD).unwrap();
        assert_eq!(
            read_pane_session("%19"),
            Some((SID.to_string(), CWD.to_string()))
        );
        assert_eq!(session_id_for_pane("%19").as_deref(), Some(SID));
    }

    #[test]
    fn test_read_pane_session_none_when_missing_or_invalid() {
        let _bed = setup();
        assert_eq!(read_pane_session("%19"), None);
        assert_eq!(session_id_for_pane("%19"), None);
        let path = pane_session_path("%19");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not json").unwrap();
        assert_eq!(read_pane_session("%19"), None);
        fs::write(&path, json!({"sessionId": SID}).to_string()).unwrap();
        assert_eq!(read_pane_session("%19"), None);
        fs::write(&path, json!(["not", "a", "dict"]).to_string()).unwrap();
        assert_eq!(read_pane_session("%19"), None);
    }

    #[test]
    fn test_key_from_socket_name_roundtrip() {
        assert_eq!(_key_from_socket_name("p19.sock").as_deref(), Some("p19"));
        assert_eq!(
            _key_from_socket_name("m-honey.rex.sock").as_deref(),
            Some("m-honey.rex")
        );
        assert_eq!(
            _key_from_socket_name("m-honey.rex.dot.sock").as_deref(),
            Some("m-honey.rex.dot")
        );
        assert_eq!(_key_from_socket_name("pdefault.sock"), None);
        assert_eq!(_key_from_socket_name("m-noseparator.sock"), None);
        assert_eq!(_key_from_socket_name("p19.pid"), None);
        assert_eq!(_key_from_socket_name("leader.sock"), None);
    }

    #[test]
    fn test_member_key_roundtrip() {
        assert_eq!(member_key("honey", "rex"), "m-honey.rex");
        assert_eq!(
            member_from_key("m-honey.rex"),
            Some(("honey".to_string(), "rex".to_string()))
        );
        // member names may carry dots; team names are dot-free, so the first
        // dot is the separator.
        assert_eq!(
            member_from_key("m-honey.rex.two"),
            Some(("honey".to_string(), "rex.two".to_string()))
        );
        assert_eq!(member_from_key("p19"), None);
        assert_eq!(member_from_key("m-"), None);
    }

    #[test]
    fn test_resolve_pane_key_uses_member_tags() {
        let _bed = setup();
        let mut tags = HashMap::new();
        tags.insert(
            ("%9".to_string(), "hive-team".to_string()),
            "honey".to_string(),
        );
        tags.insert(
            ("%9".to_string(), "hive-agent".to_string()),
            "rex".to_string(),
        );
        set_pane_options(tags);
        assert_eq!(resolve_pane_key("%9"), "m-honey.rex");
        assert_eq!(resolve_pane_key("%7"), "p7"); // untagged: raw pane lifecycle
    }

    #[test]
    fn test_list_daemon_keys_filters_to_daemon_sockets() {
        let bed = setup();
        let hive_dir = bed.tmp.path().join("hive");
        fs::create_dir_all(&hive_dir).unwrap();
        for name in [
            "p19.sock",
            "p7.sock",
            "m-honey.rex.sock",
            "pdefault.sock",
            "p19.session",
        ] {
            fs::write(hive_dir.join(name), "").unwrap();
        }
        let mut keys = list_daemon_keys();
        keys.sort();
        assert_eq!(keys, vec!["m-honey.rex", "p19", "p7"]);
    }

    #[test]
    fn test_list_daemon_keys_missing_dir() {
        let _bed = setup();
        assert!(list_daemon_keys().is_empty());
    }

    // ----------------------------------------------------------------------
    // daemon lifecycle
    // ----------------------------------------------------------------------

    #[test]
    fn test_probe_socket_needs_socket_and_live_pid() {
        let _bed = setup();
        let sock = pane_socket_path("%19");
        fs::create_dir_all(sock.parent().unwrap()).unwrap();
        assert!(!probe_socket(&sock)); // no socket
        fs::write(&sock, "").unwrap();
        assert!(!probe_socket(&sock)); // no pidfile
        fs::write(pane_pidfile_path("%19"), std::process::id().to_string()).unwrap();
        assert!(probe_socket(&sock));
        // Python monkeypatches os.kill to raise; a guaranteed-dead pid is the
        // seamless equivalent (same convention as the dead-leader pool test).
        fs::write(pane_pidfile_path("%19"), "999999").unwrap();
        assert!(!probe_socket(&sock));
    }

    struct FakeDaemonChild {
        pid: u32,
        returncode: Option<i32>,
        panic_on_terminate: bool,
    }

    impl DaemonChild for FakeDaemonChild {
        fn pid(&self) -> u32 {
            self.pid
        }

        fn poll(&self) -> Option<i32> {
            self.returncode
        }

        fn terminate(&self) {
            if self.panic_on_terminate {
                panic!("must not terminate a healthy leader");
            }
        }
    }

    fn touch_leader_socket(argv: &[String]) {
        let sock = &argv[argv
            .iter()
            .position(|arg| arg == "--leader-socket")
            .unwrap()
            + 1];
        fs::write(sock, "").unwrap();
    }

    #[test]
    fn test_spawn_daemon_builds_leader_argv_and_pane_env() {
        let _bed = setup();
        env::set_var("TMUX_PANE", "%old");
        let seen: Arc<Mutex<Option<(Vec<String>, HashMap<String, String>)>>> =
            Arc::new(Mutex::new(None));
        let seen_spawn = seen.clone();
        set_daemon_spawn(move |argv, env| {
            *seen_spawn.lock().unwrap() = Some((argv.to_vec(), env.clone()));
            touch_leader_socket(argv);
            Ok(Box::new(FakeDaemonChild {
                pid: 7777,
                returncode: None,
                panic_on_terminate: true,
            }))
        });
        assert!(spawn_daemon("%19"));
        let seen = seen.lock().unwrap();
        let (argv, env) = seen.as_ref().unwrap();
        assert_eq!(
            *argv,
            vec![
                "grok".to_string(),
                "agent".to_string(),
                "leader".to_string(),
                "--leader-socket".to_string(),
                pane_socket_path("%19").to_string_lossy().into_owned(),
                "--no-auto-update".to_string(),
                "--no-exit-on-disconnect".to_string(),
            ]
        );
        assert_eq!(env.get("TMUX_PANE").map(String::as_str), Some("%19"));
        // Python also asserts Popen kwargs (start_new_session=True,
        // stdin=DEVNULL); those live in _spawn_leader_real and are not
        // observable through the spawn seam.
        assert_eq!(
            fs::read_to_string(pane_pidfile_path("%19")).unwrap(),
            "7777"
        );
    }

    #[test]
    fn test_spawn_daemon_false_when_leader_exits_early() {
        let _bed = setup();
        set_daemon_spawn(|_argv, _env| {
            Ok(Box::new(FakeDaemonChild {
                pid: 7778,
                returncode: Some(1),
                panic_on_terminate: false,
            }))
        });
        assert!(!spawn_daemon("%19"));
        assert!(!pane_pidfile_path("%19").exists());
    }

    #[test]
    fn test_spawn_daemon_reuses_a_live_daemon() {
        let _bed = setup();
        let sock = pane_socket_path("%19");
        fs::create_dir_all(sock.parent().unwrap()).unwrap();
        fs::write(&sock, "").unwrap();
        fs::write(pane_pidfile_path("%19"), std::process::id().to_string()).unwrap();
        set_daemon_spawn(|_argv, _env| panic!("must not respawn a live leader"));
        assert!(spawn_daemon("%19"));
    }

    #[test]
    fn test_spawn_daemon_clears_a_stale_socket() {
        let _bed = setup();
        let sock = pane_socket_path("%19");
        fs::create_dir_all(sock.parent().unwrap()).unwrap();
        fs::write(&sock, "").unwrap(); // stale: no pidfile, so no live daemon
        let existed: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
        let existed_spawn = existed.clone();
        let sock_spawn = sock.clone();
        set_daemon_spawn(move |argv, _env| {
            *existed_spawn.lock().unwrap() = Some(sock_spawn.exists());
            touch_leader_socket(argv);
            Ok(Box::new(FakeDaemonChild {
                pid: 7779,
                returncode: None,
                panic_on_terminate: false,
            }))
        });
        assert!(spawn_daemon("%19"));
        // stale socket unlinked before respawn
        assert_eq!(*existed.lock().unwrap(), Some(false));
    }

    #[test]
    fn test_kill_pane_daemon_removes_socket_pid_and_session() {
        let _bed = setup();
        write_pane_session("%19", SID, CWD).unwrap();
        fs::write(pane_socket_path("%19"), "").unwrap();
        fs::write(pane_pidfile_path("%19"), "4321").unwrap();
        let killed: Arc<Mutex<Vec<libc::pid_t>>> = Arc::new(Mutex::new(Vec::new()));
        let killed_record = killed.clone();
        set_terminate_pg(move |pid| killed_record.lock().unwrap().push(pid));
        kill_pane_daemon("%19");
        assert_eq!(*killed.lock().unwrap(), vec![4321]);
        assert!(!pane_socket_path("%19").exists());
        assert!(!pane_pidfile_path("%19").exists());
        assert!(!pane_session_path("%19").exists());
    }

    // ----------------------------------------------------------------------
    // pool
    // ----------------------------------------------------------------------

    struct FakePromptClient {
        sent: Arc<Mutex<Vec<String>>>,
    }

    impl LeaderClient for FakePromptClient {
        fn prompt(&self, text: &str) -> Result<bool> {
            self.sent.lock().unwrap().push(text.to_string());
            Ok(true)
        }
    }

    #[test]
    fn test_pool_send_to_pane_returns_prompt_queued() {
        let grok_pool = GrokClientPool::new();
        let sent: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sent_client = sent.clone();
        *grok_pool.client_override.lock().unwrap() = Some(Box::new(move |_key| {
            Some(Arc::new(FakePromptClient {
                sent: sent_client.clone(),
            }))
        }));
        assert_eq!(grok_pool.send_to_key("p19", "hi"), Some(PROMPT_QUEUED));
        assert_eq!(*sent.lock().unwrap(), vec!["hi"]);
    }

    #[test]
    fn test_pool_send_to_pane_none_without_client() {
        let grok_pool = GrokClientPool::new();
        *grok_pool.client_override.lock().unwrap() = Some(Box::new(|_key| None));
        assert_eq!(grok_pool.send_to_key("p19", "hi"), None);
    }

    struct FakeRaisingPromptClient;

    impl LeaderClient for FakeRaisingPromptClient {
        fn prompt(&self, _text: &str) -> Result<bool> {
            Err(anyhow::anyhow!("broken pipe"))
        }
    }

    #[test]
    fn test_pool_send_to_pane_none_when_client_raises() {
        let grok_pool = GrokClientPool::new();
        *grok_pool.client_override.lock().unwrap() =
            Some(Box::new(|_key| Some(Arc::new(FakeRaisingPromptClient))));
        assert_eq!(grok_pool.send_to_key("p19", "hi"), None);
    }

    struct FakeCancelClient {
        cancelled: Arc<Mutex<Vec<bool>>>,
        answer: Result<bool>,
    }

    impl LeaderClient for FakeCancelClient {
        fn cancel(&self) -> Result<bool> {
            self.cancelled.lock().unwrap().push(true);
            match &self.answer {
                Ok(value) => Ok(*value),
                Err(err) => Err(anyhow::anyhow!("{err}")),
            }
        }
    }

    #[test]
    fn test_pool_interrupt_pane_returns_cancel_sent() {
        let grok_pool = GrokClientPool::new();
        let cancelled: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let cancelled_client = cancelled.clone();
        *grok_pool.client_override.lock().unwrap() = Some(Box::new(move |_key| {
            Some(Arc::new(FakeCancelClient {
                cancelled: cancelled_client.clone(),
                answer: Ok(true),
            }))
        }));
        assert_eq!(grok_pool.interrupt_key("p19"), Some(CANCEL_SENT));
        assert_eq!(*cancelled.lock().unwrap(), vec![true]);
    }

    #[test]
    fn test_pool_interrupt_pane_none_without_client() {
        let grok_pool = GrokClientPool::new();
        *grok_pool.client_override.lock().unwrap() = Some(Box::new(|_key| None));
        assert_eq!(grok_pool.interrupt_key("p19"), None);
    }

    #[test]
    fn test_pool_interrupt_pane_none_when_the_write_fails() {
        let grok_pool = GrokClientPool::new();
        *grok_pool.client_override.lock().unwrap() = Some(Box::new(|_key| {
            Some(Arc::new(FakeCancelClient {
                cancelled: Arc::new(Mutex::new(Vec::new())),
                answer: Ok(false),
            }))
        }));
        assert_eq!(grok_pool.interrupt_key("p19"), None);
    }

    #[test]
    fn test_pool_interrupt_pane_none_when_client_raises() {
        let grok_pool = GrokClientPool::new();
        *grok_pool.client_override.lock().unwrap() = Some(Box::new(|_key| {
            Some(Arc::new(FakeCancelClient {
                cancelled: Arc::new(Mutex::new(Vec::new())),
                answer: Err(anyhow::anyhow!("broken pipe")),
            }))
        }));
        assert_eq!(grok_pool.interrupt_key("p19"), None);
    }

    #[test]
    fn test_pool_compact_pane_unavailable_without_client() {
        let grok_pool = GrokClientPool::new();
        *grok_pool.client_override.lock().unwrap() = Some(Box::new(|_key| None));
        assert_eq!(grok_pool.compact_key("p19"), "unavailable");
    }

    #[test]
    fn test_pool_runtime_for_pane_none_without_client() {
        let grok_pool = GrokClientPool::new();
        *grok_pool.client_override.lock().unwrap() = Some(Box::new(|_key| None));
        assert_eq!(grok_pool.runtime_for_key("p19"), None);
        assert!(!grok_pool.connect_key("p19"));
    }

    #[test]
    fn test_pool_skips_panes_without_socket_or_session() {
        let _bed = setup();
        set_stdio_spawn(|_argv| panic!("no client without a daemon"));
        let grok_pool = GrokClientPool::new();
        assert!(grok_pool._client_for_key("p19").is_none()); // no socket at all
        let sock = pane_socket_path("%19");
        fs::create_dir_all(sock.parent().unwrap()).unwrap();
        fs::write(&sock, "").unwrap();
        grok_pool.state.lock().unwrap().cooldown.clear();
        // socket but no session record
        assert!(grok_pool._client_for_key("p19").is_none());
    }

    #[test]
    fn test_pool_skips_a_pane_whose_leader_pid_is_dead() {
        // a socket file outlives the leader that bound it: connecting to it hangs
        let _bed = setup();
        write_pane_session("%19", SID, CWD).unwrap();
        fs::write(pane_socket_path("%19"), "").unwrap();
        fs::write(pane_pidfile_path("%19"), "999999").unwrap();
        set_stdio_spawn(|_argv| panic!("no client without a live leader"));
        assert!(GrokClientPool::new()._client_for_key("p19").is_none());
    }

    #[test]
    fn test_pool_rebinds_when_the_pane_session_record_rotates() {
        // grok relaunched in the same pane mints a new session id; the client bound
        // to the old one would report a stale session forever
        let _bed = setup();
        write_pane_session("%19", SID, CWD).unwrap();
        fs::write(pane_socket_path("%19"), "").unwrap();
        fs::write(pane_pidfile_path("%19"), std::process::id().to_string()).unwrap();
        let procs: Arc<Mutex<Vec<Arc<FakeProc>>>> = Arc::new(Mutex::new(Vec::new()));
        let procs_spawn = procs.clone();
        set_stdio_spawn(move |_argv| {
            let proc = FakeProc::new(Some(responder(None, vec![])));
            procs_spawn.lock().unwrap().push(proc.clone());
            Ok(proc as Arc<dyn LeaderProc>)
        });
        let grok_pool = GrokClientPool::new();
        let clients: Arc<Mutex<Vec<Arc<GrokStdioClient>>>> = Arc::new(Mutex::new(Vec::new()));

        let bind = |grok_pool: &GrokClientPool| -> Option<Arc<GrokStdioClient>> {
            let client = grok_pool._client_for_key("p19");
            if let Some(client) = client.as_ref() {
                let mut known = clients.lock().unwrap();
                if !known.iter().any(|c| Arc::ptr_eq(c, client)) {
                    known.push(client.clone());
                }
            }
            client
        };

        let first = bind(&grok_pool).unwrap();
        assert_eq!(first.session_id().as_deref(), Some(SID));
        // stable while the record holds
        assert!(Arc::ptr_eq(&bind(&grok_pool).unwrap(), &first));

        let rotated = "99999999-8888-7777-6666-555555555555";
        write_pane_session("%19", rotated, CWD).unwrap();
        let second = bind(&grok_pool).unwrap();
        assert!(!Arc::ptr_eq(&second, &first));
        assert_eq!(second.session_id().as_deref(), Some(rotated));
        assert!(!first.is_alive()); // the stale client is closed, not leaked

        grok_pool.drop("%19");
        for proc in procs.lock().unwrap().iter() {
            proc.eof();
        }
        for client in clients.lock().unwrap().iter() {
            if let Some(handle) = client.reader.lock().unwrap().take() {
                let _ = handle.join();
            }
        }
    }

    #[test]
    fn test_daemon_env_washes_inherited_identity_markers() {
        // Regression: a leader spawned from inside another member's engine
        // inherited that engine's CLAUDE_CODE_MESSAGING_SOCKET, so every hive call
        // in this grok member resolved to the orch's pane (replies came from=orch).
        let _bed = setup();
        env::set_var("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/cc-socks/999.sock");
        env::set_var("CLAUDE_CONFIG_DIR", "/tmp/elsewhere");
        env::set_var("CODEX_THREAD_ID", "tid-1");
        env::set_var("TMUX_PANE", "%stale");

        let env_map = _daemon_env_for_pane("%42");

        assert_eq!(env_map.get("TMUX_PANE").map(String::as_str), Some("%42"));
        assert!(!env_map.contains_key("CLAUDE_CODE_MESSAGING_SOCKET"));
        assert!(!env_map.contains_key("CLAUDE_CONFIG_DIR"));
        assert!(!env_map.contains_key("CODEX_THREAD_ID"));

        env::remove_var("CLAUDE_CODE_MESSAGING_SOCKET");
        env::remove_var("CLAUDE_CONFIG_DIR");
        env::remove_var("CODEX_THREAD_ID");
    }

    #[test]
    fn test_spawn_daemon_member_pane_gets_member_socket_and_identity_env() {
        // A tagged member pane spawns a member-keyed daemon whose env carries the
        // member identity — and never the spawner's inherited one.
        let bed = setup();
        env::set_var("HIVE_TEAM", "spawner-team");
        env::set_var("HIVE_MEMBER", "spawner");
        let mut tags = HashMap::new();
        tags.insert(
            ("%19".to_string(), "hive-team".to_string()),
            "honey".to_string(),
        );
        tags.insert(
            ("%19".to_string(), "hive-agent".to_string()),
            "rex".to_string(),
        );
        set_pane_options(tags);
        let seen: Arc<Mutex<Option<(Vec<String>, HashMap<String, String>)>>> =
            Arc::new(Mutex::new(None));
        let seen_spawn = seen.clone();
        set_daemon_spawn(move |argv, env| {
            *seen_spawn.lock().unwrap() = Some((argv.to_vec(), env.clone()));
            touch_leader_socket(argv);
            Ok(Box::new(FakeDaemonChild {
                pid: 7777,
                returncode: None,
                panic_on_terminate: false,
            }))
        });
        assert!(spawn_daemon("%19"));
        let seen = seen.lock().unwrap();
        let (argv, env_map) = seen.as_ref().unwrap();
        let sock = &argv[argv
            .iter()
            .position(|arg| arg == "--leader-socket")
            .unwrap()
            + 1];
        assert!(sock.ends_with("m-honey.rex.sock"));
        assert_eq!(env_map.get("HIVE_TEAM").map(String::as_str), Some("honey"));
        assert_eq!(env_map.get("HIVE_MEMBER").map(String::as_str), Some("rex"));
        assert_eq!(env_map.get("TMUX_PANE").map(String::as_str), Some("%19"));
        assert_eq!(
            fs::read_to_string(bed.tmp.path().join("hive").join("m-honey.rex.pid")).unwrap(),
            "7777"
        );
        assert_eq!(
            *sock,
            socket_path_for_key("m-honey.rex")
                .to_string_lossy()
                .into_owned()
        );
        env::remove_var("HIVE_TEAM");
        env::remove_var("HIVE_MEMBER");
    }

    #[test]
    fn test_kill_daemon_key_removes_socket_pid_and_session() {
        let _bed = setup();
        let sock = socket_path_for_key("m-honey.rex");
        fs::create_dir_all(sock.parent().unwrap()).unwrap();
        fs::write(&sock, "").unwrap();
        fs::write(sock.with_extension("pid"), "4321").unwrap();
        fs::write(
            sock.with_extension("session"),
            "{\"sessionId\": \"s\", \"cwd\": \"/c\"}",
        )
        .unwrap();
        let killed: Arc<Mutex<Vec<libc::pid_t>>> = Arc::new(Mutex::new(Vec::new()));
        let killed_record = killed.clone();
        set_terminate_pg(move |pid| killed_record.lock().unwrap().push(pid));

        kill_daemon_key("m-honey.rex");

        assert_eq!(*killed.lock().unwrap(), vec![4321]);
        assert!(!sock.exists());
        assert!(!sock.with_extension("pid").exists());
        assert!(!sock.with_extension("session").exists());
    }
}
