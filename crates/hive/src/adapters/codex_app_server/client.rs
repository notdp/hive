// --------------------------------------------------------------------------
// per-thread runtime state, kept current by the reader thread
// --------------------------------------------------------------------------

use std::collections::HashMap;
use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{json, Map, Value};

use super::transport::{ws_send_frame, WsConn};
use super::{CALL_TIMEOUT, HANDSHAKE_TIMEOUT, RESUME_COOLDOWN};

#[derive(Debug, Clone, PartialEq)]
pub struct ThreadRuntime {
    pub busy: bool,
    pub turn_phase: String,
    pub input_state: String,
    pub observed_at: f64,
}

impl Default for ThreadRuntime {
    fn default() -> Self {
        ThreadRuntime {
            busy: false,
            turn_phase: "unknown_evidence".to_string(),
            input_state: String::new(),
            observed_at: 0.0,
        }
    }
}

fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub(crate) fn apply_status(rt: &mut ThreadRuntime, status: &Value) {
    match status.get("type").and_then(Value::as_str) {
        Some("active") => {
            rt.busy = true;
            rt.turn_phase = "tool_open".to_string();
            let waiting = status
                .get("activeFlags")
                .and_then(Value::as_array)
                .is_some_and(|flags| {
                    flags.iter().any(|flag| {
                        matches!(
                            flag.as_str(),
                            Some("waitingOnApproval") | Some("waitingOnUserInput")
                        )
                    })
                });
            rt.input_state = if waiting { "waiting_user" } else { "ready" }.to_string();
        }
        Some("idle") => {
            rt.busy = false;
            rt.turn_phase = "turn_closed".to_string();
            rt.input_state = "ready".to_string();
        }
        // notLoaded / systemError: leave prior fields, only observed_at advanced
        _ => {}
    }
}

// --------------------------------------------------------------------------
// one connection to the shared daemon
// --------------------------------------------------------------------------

type CallOverride = Box<dyn Fn(&str, &Value) -> Value + Send>;

struct Slot {
    msg: Mutex<Option<Value>>,
    cv: Condvar,
}

#[derive(Default)]
struct ClientState {
    threads: HashMap<String, ThreadRuntime>,
    resume_cooldown: HashMap<String, Instant>,
}

struct Inner {
    state: Mutex<ClientState>,
    pending: Mutex<HashMap<u64, Arc<Slot>>>,
    /// Held across id mint, pending insert and the frame write, so ids reach
    /// the wire in mint order.
    next_id: Mutex<u64>,
    stream: Option<Arc<UnixStream>>,
    closed: AtomicBool,
    reader: Mutex<Option<thread::JoinHandle<()>>>,
    /// Test seam: a scripted `call` in place of the socket round trip;
    /// always None in production.
    call_override: Mutex<Option<CallOverride>>,
}

#[derive(Clone)]
pub struct CodexDaemonClient {
    inner: Arc<Inner>,
}

fn on_notification_state(inner: &Inner, method: &str, params: &Value) {
    // thread/status/changed is the only busy-relevant notification a
    // non-turn-owning client receives on the shared daemon (turn/* and item/*
    // go to the turn's own client only).
    if method != "thread/status/changed" {
        return;
    }
    let tid = match params.get("threadId").and_then(Value::as_str) {
        Some(tid) if !tid.is_empty() => tid,
        _ => return,
    };
    let mut state = inner.state.lock().unwrap();
    let rt = state.threads.entry(tid.to_string()).or_default();
    rt.observed_at = now_epoch();
    apply_status(rt, params.get("status").unwrap_or(&Value::Null));
}

fn reader_loop(inner: Arc<Inner>, mut conn: WsConn) {
    while !inner.closed.load(Ordering::SeqCst) {
        let txt = match conn.recv_text() {
            Ok(txt) => txt,
            Err(_) => break,
        };
        let msg: Value = match serde_json::from_str(&txt) {
            Ok(msg) => msg,
            Err(_) => continue,
        };
        // Pop atomically: a `call()` that timed out concurrently may have
        // already removed this rid. A missing slot just means the waiter is
        // gone (timed out) — drop the late response silently.
        let slot = msg
            .get("id")
            .and_then(Value::as_u64)
            .and_then(|rid| inner.pending.lock().unwrap().remove(&rid));
        if let Some(slot) = slot {
            *slot.msg.lock().unwrap() = Some(msg);
            slot.cv.notify_all();
        } else if let Some(method) = msg.get("method").and_then(Value::as_str) {
            if !method.is_empty() {
                let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
                on_notification_state(&inner, method, &params);
            }
        }
    }
    inner.closed.store(true, Ordering::SeqCst);
}

impl CodexDaemonClient {
    pub fn new(socket_path: &Path) -> io::Result<CodexDaemonClient> {
        let conn = WsConn::connect(socket_path, Duration::from_secs_f64(HANDSHAKE_TIMEOUT))?;
        let inner = Arc::new(Inner {
            state: Mutex::new(ClientState::default()),
            pending: Mutex::new(HashMap::new()),
            next_id: Mutex::new(0),
            stream: Some(conn.stream.clone()),
            closed: AtomicBool::new(false),
            reader: Mutex::new(None),
            call_override: Mutex::new(None),
        });
        let reader_inner = inner.clone();
        let handle = thread::spawn(move || reader_loop(reader_inner, conn));
        *inner.reader.lock().unwrap() = Some(handle);
        Ok(CodexDaemonClient { inner })
    }

    // ---- request/response ----
    pub fn call(&self, method: &str, params: Value) -> Value {
        {
            let overridden = self.inner.call_override.lock().unwrap();
            if let Some(call) = overridden.as_ref() {
                return call(method, &params);
            }
        }
        let stream = match self.inner.stream.as_ref() {
            Some(stream) => stream,
            None => return json!({"__error__": "closed"}),
        };
        let slot = Arc::new(Slot {
            msg: Mutex::new(None),
            cv: Condvar::new(),
        });
        let rid;
        {
            let mut next_id = self.inner.next_id.lock().unwrap();
            if self.inner.closed.load(Ordering::SeqCst) {
                return json!({"__error__": "closed"});
            }
            *next_id += 1;
            rid = *next_id;
            self.inner.pending.lock().unwrap().insert(rid, slot.clone());
            let payload = json!({"id": rid, "method": method, "params": params});
            if let Err(err) = ws_send_frame(stream, 0x1, payload.to_string().as_bytes()) {
                self.inner.pending.lock().unwrap().remove(&rid);
                return json!({"__error__": err.to_string()});
            }
        }
        let guard = slot.msg.lock().unwrap();
        let (mut guard, _timeout) = slot
            .cv
            .wait_timeout_while(guard, Duration::from_secs_f64(CALL_TIMEOUT), |msg| {
                msg.is_none()
            })
            .unwrap();
        let msg = match guard.take() {
            Some(msg) => msg,
            None => {
                self.inner.pending.lock().unwrap().remove(&rid);
                return json!({"__timeout__": true});
            }
        };
        if let Some(err) = msg.get("error") {
            return json!({"__error__": err.clone()});
        }
        json!({"result": msg.get("result").cloned().unwrap_or(Value::Null)})
    }

    // ---- notification -> state ----
    #[cfg(test)]
    pub(crate) fn on_notification(&self, method: &str, params: &Value) {
        on_notification_state(&self.inner, method, params);
    }

    fn seed_status(&self, thread_id: &str, status: &Value) {
        if !status.is_object() {
            return;
        }
        let mut state = self.inner.state.lock().unwrap();
        let rt = state.threads.entry(thread_id.to_string()).or_default();
        rt.observed_at = now_epoch();
        apply_status(rt, status);
    }

    pub fn runtime_for(&self, thread_id: &str) -> Option<ThreadRuntime> {
        self.inner
            .state
            .lock()
            .unwrap()
            .threads
            .get(thread_id)
            .cloned()
    }

    /// Runtime for *thread_id*, resuming once to recover missing state.
    ///
    /// A client connected before the thread existed has no state for it until
    /// the first status broadcast; `thread/resume` returns the thread's
    /// current status and backfills it. Rate-limited per thread so a
    /// never-resolving id does not storm resumes.
    pub fn runtime_or_backfill(&self, thread_id: &str) -> Option<ThreadRuntime> {
        if let Some(rt) = self.runtime_for(thread_id) {
            return Some(rt);
        }
        {
            let mut state = self.inner.state.lock().unwrap();
            let now = Instant::now();
            if let Some(until) = state.resume_cooldown.get(thread_id) {
                if now < *until {
                    return None;
                }
            }
            state.resume_cooldown.insert(
                thread_id.to_string(),
                now + Duration::from_secs_f64(RESUME_COOLDOWN),
            );
        }
        self.resume(thread_id);
        self.runtime_for(thread_id)
    }

    // ---- protocol helpers ----
    pub fn initialize(&self) -> bool {
        let res = self.call(
            "initialize",
            json!({
                "clientInfo": {"name": "hive", "title": "hive", "version": "1"},
                "capabilities": {"experimentalApi": true},
            }),
        );
        res.get("result").is_some()
    }

    /// Recover state for already-active threads (busy late-join).
    ///
    /// A client online when a status edge fires gets the broadcast; this
    /// covers the late-join case by resuming each loaded thread once — the
    /// resume response carries the thread's current status.
    pub fn attach(&self) {
        for tid in self.loaded_list() {
            self.resume(&tid);
        }
    }

    pub fn loaded_list(&self) -> Vec<String> {
        let res = self.call("thread/loaded/list", json!({}));
        if res.get("result").is_none() {
            return Vec::new();
        }
        res.get("result")
            .and_then(|result| result.get("data"))
            .and_then(Value::as_array)
            .map(|data| {
                data.iter()
                    .filter_map(|item| item.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Backfill a thread's current status from `thread/resume`.
    pub fn resume(&self, thread_id: &str) -> bool {
        let res = self.call(
            "thread/resume",
            json!({"threadId": thread_id, "excludeTurns": true}),
        );
        let result = match res.get("result").and_then(Value::as_object) {
            Some(result) => result,
            None => return false,
        };
        if let Some(thread) = result.get("thread").filter(|t| t.is_object()) {
            self.seed_status(thread_id, thread.get("status").unwrap_or(&Value::Null));
        }
        true
    }

    /// Mint a new thread for *cwd*; return its threadId (== sessionId).
    ///
    /// `thread/start` alone leaves the thread unpersisted: the daemon writes
    /// the rollout lazily, and the pane TUI resumes in paginated-history mode
    /// (`thread/resume {excludeTurns}`), which reads the source rollout from
    /// disk and fails on a thread that has none. The name write is metadata
    /// only (state DB) and never materializes the file, so the flush is
    /// `flush_thread`, and the rollout's presence on disk is the oracle.
    /// *name* must be non-empty (the daemon rejects empty names).
    pub fn start_thread(&self, cwd: &str, name: &str, model: &str) -> Option<String> {
        let mut params = Map::new();
        params.insert("cwd".to_string(), json!(cwd));
        if !model.is_empty() {
            params.insert("model".to_string(), json!(model));
        }
        let res = self.call("thread/start", Value::Object(params));
        let thread = res
            .get("result")
            .and_then(Value::as_object)?
            .get("thread")
            .and_then(Value::as_object)?;
        let tid = thread_id_from(thread)?;
        self.seed_status(&tid, thread.get("status").unwrap_or(&Value::Null));
        let rollout = thread_path_from(thread);
        self.flush_thread(&tid, name, rollout.as_deref())
            .then_some(tid)
    }

    /// Name the thread and force its rollout onto disk; false when any step
    /// fails, because an unflushed thread is not attachable and must be
    /// treated as a spawn failure.
    ///
    /// `thread/section/move` with a null section is the materialization
    /// trigger: the daemon materializes and flushes the rollout before a
    /// placement so it works ahead of the first turn, and a null section is a
    /// no-op placement that leaves only the session header in the file —
    /// nothing the model sees (0.153.2 verified). The check is the file
    /// itself, at the path `thread/start` reported: which call materializes
    /// has changed across codex versions, the file's presence is what the
    /// TUI's resume actually needs.
    fn flush_thread(&self, tid: &str, name: &str, rollout: Option<&Path>) -> bool {
        let named = self.call("thread/name/set", json!({"threadId": tid, "name": name}));
        if named.get("result").is_none() {
            return false;
        }
        let placed = self.call(
            "thread/section/move",
            json!({"threadId": tid, "sectionId": Value::Null}),
        );
        if placed.get("result").is_none() {
            return false;
        }
        rollout.is_some_and(Path::is_file)
    }

    /// Fork a rolled-out thread server-side; return the fork's threadId.
    pub fn fork_thread(&self, thread_id: &str, name: &str) -> Option<String> {
        let res = self.call("thread/fork", json!({"threadId": thread_id}));
        let thread = res
            .get("result")
            .and_then(Value::as_object)?
            .get("thread")
            .and_then(Value::as_object)?;
        let tid = thread_id_from(thread)?;
        self.seed_status(&tid, thread.get("status").unwrap_or(&Value::Null));
        let rollout = thread_path_from(thread);
        self.flush_thread(&tid, name, rollout.as_deref())
            .then_some(tid)
    }

    pub fn turn_start(&self, thread_id: &str, text: &str) -> Value {
        self.call(
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{"type": "text", "text": text}],
            }),
        )
    }

    /// Id of the thread's in-progress turn, read from the daemon: `Ok(None)`
    /// is the daemon answering that no turn is open, `Err` is no answer
    /// (RPC error, closed connection).
    ///
    /// `turn/interrupt` requires the turnId and `ThreadStatus::Active`
    /// carries none, so the id has to be read back — hive never owns the turn
    /// (the pane's TUI started it) and only the starting client gets `turn/*`
    /// notifications. `thread/read` with `includeTurns` is the one route.
    pub fn active_turn_id(&self, thread_id: &str) -> Result<Option<String>, String> {
        let res = self.call(
            "thread/read",
            json!({"threadId": thread_id, "includeTurns": true}),
        );
        let Some(result) = res.get("result").and_then(Value::as_object) else {
            return Err(res
                .get("__error__")
                .or_else(|| res.get("error"))
                .map(Value::to_string)
                .unwrap_or_else(|| "thread/read answered without a result".to_string()));
        };
        let turns = result
            .get("thread")
            .and_then(|thread| thread.get("turns"))
            .and_then(Value::as_array);
        for turn in turns.map(Vec::as_slice).unwrap_or(&[]).iter().rev() {
            if turn.get("status").and_then(Value::as_str) == Some("inProgress") {
                let id = turn.get("id").and_then(Value::as_str).unwrap_or("");
                return Ok(if id.is_empty() {
                    None
                } else {
                    Some(id.to_string())
                });
            }
        }
        Ok(None)
    }

    /// Abort *turn_id* on *thread_id*.
    ///
    /// 0.149.1 verified: the turnId is mandatory (omitting it answers
    /// `-32600 Invalid request: missing field turnId`) and is checked against
    /// the live turn, so a stale id can never abort a turn that started since.
    pub fn turn_interrupt(&self, thread_id: &str, turn_id: &str) -> Value {
        self.call(
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
        )
    }

    /// Start a context-compaction turn (the `/compact` slash equivalent).
    ///
    /// This is the dedicated RPC the codex TUI fires for `/compact`; sending
    /// `/compact` as `turn/start` text only feeds the model a literal prompt
    /// and never compacts.
    pub fn compact_start(&self, thread_id: &str) -> Value {
        self.call("thread/compact/start", json!({"threadId": thread_id}))
    }

    pub fn is_alive(&self) -> bool {
        if self.inner.closed.load(Ordering::SeqCst) {
            return false;
        }
        self.inner
            .reader
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }

    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        if let Some(stream) = self.inner.stream.as_ref() {
            let _ = ws_send_frame(stream, 0x8, b"");
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

/// The rollout path the daemon precomputed for the thread (`Thread.path`),
/// present before the file exists.
fn thread_path_from(thread: &Map<String, Value>) -> Option<std::path::PathBuf> {
    match thread.get("path") {
        Some(Value::String(path)) if !path.is_empty() => Some(std::path::PathBuf::from(path)),
        _ => None,
    }
}

fn thread_id_from(thread: &Map<String, Value>) -> Option<String> {
    match thread.get("id") {
        Some(Value::String(id)) if !id.is_empty() => Some(id.clone()),
        Some(Value::Number(id)) => Some(id.to_string()),
        _ => None,
    }
}

/// The client surface the pane-keyed API dials through `shared_client`,
/// and the seam tests substitute fakes through. `Err` is a transport
/// failure a fake raises; the real client never errs here — `call` folds
/// its failures into an `__error__` payload.
pub trait DaemonClient: Send + Sync {
    fn turn_start(&self, _thread_id: &str, _text: &str) -> Result<Value, String> {
        unimplemented!("turn_start")
    }
    fn active_turn_id(&self, _thread_id: &str) -> Result<Option<String>, String> {
        unimplemented!("active_turn_id")
    }
    fn turn_interrupt(&self, _thread_id: &str, _turn_id: &str) -> Result<Value, String> {
        unimplemented!("turn_interrupt")
    }
    fn runtime_or_backfill(&self, _thread_id: &str) -> Option<ThreadRuntime> {
        unimplemented!("runtime_or_backfill")
    }
    fn compact_start(&self, _thread_id: &str) -> Value {
        unimplemented!("compact_start")
    }
    fn start_thread(&self, _cwd: &str, _name: &str, _model: &str) -> Option<String> {
        unimplemented!("start_thread")
    }
    fn fork_thread(&self, _thread_id: &str, _name: &str) -> Option<String> {
        unimplemented!("fork_thread")
    }
}

impl DaemonClient for CodexDaemonClient {
    fn turn_start(&self, thread_id: &str, text: &str) -> Result<Value, String> {
        Ok(CodexDaemonClient::turn_start(self, thread_id, text))
    }
    fn active_turn_id(&self, thread_id: &str) -> Result<Option<String>, String> {
        CodexDaemonClient::active_turn_id(self, thread_id)
    }
    fn turn_interrupt(&self, thread_id: &str, turn_id: &str) -> Result<Value, String> {
        Ok(CodexDaemonClient::turn_interrupt(self, thread_id, turn_id))
    }
    fn runtime_or_backfill(&self, thread_id: &str) -> Option<ThreadRuntime> {
        CodexDaemonClient::runtime_or_backfill(self, thread_id)
    }
    fn compact_start(&self, thread_id: &str) -> Value {
        CodexDaemonClient::compact_start(self, thread_id)
    }
    fn start_thread(&self, cwd: &str, name: &str, model: &str) -> Option<String> {
        CodexDaemonClient::start_thread(self, cwd, name, model)
    }
    fn fork_thread(&self, thread_id: &str, name: &str) -> Option<String> {
        CodexDaemonClient::fork_thread(self, thread_id, name)
    }
}

// --------------------------------------------------------------------------
// test seams
// --------------------------------------------------------------------------

#[cfg(test)]
impl CodexDaemonClient {
    /// A client without a socket connection, for state-logic tests.
    pub(super) fn bare() -> CodexDaemonClient {
        CodexDaemonClient {
            inner: Arc::new(Inner {
                state: Mutex::new(ClientState::default()),
                pending: Mutex::new(HashMap::new()),
                next_id: Mutex::new(0),
                stream: None,
                closed: AtomicBool::new(false),
                reader: Mutex::new(None),
                call_override: Mutex::new(None),
            }),
        }
    }

    /// Script `call`, so state logic runs without a daemon.
    pub(super) fn set_call_override(&self, call: impl Fn(&str, &Value) -> Value + Send + 'static) {
        *self.inner.call_override.lock().unwrap() = Some(Box::new(call));
    }

    pub(super) fn threads_is_empty(&self) -> bool {
        self.inner.state.lock().unwrap().threads.is_empty()
    }
}
