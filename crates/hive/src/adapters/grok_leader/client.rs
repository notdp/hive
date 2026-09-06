use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::keys::{read_session_key, socket_path_for_key, SessionRecord};
use super::{ACK_TIMEOUT, CALL_TIMEOUT, INIT_TIMEOUT, LOAD_TIMEOUT, MESSAGE_CHUNKS};

// --------------------------------------------------------------------------
// per-session runtime state, kept current by the reader thread
// --------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct SessionRuntime {
    pub busy: bool,
    pub input_state: String,
    pub session_id: Option<String>,
    pub observed_at: f64,
    /// Turn evidence for the workflow runner's dispatch gate, distinct from the
    /// display `busy` flag: `busy` defaults to false and `observed_at` is
    /// bumped by any in-session notification, so a client that has only
    /// seen a command table, an announcement, or a permission prompt would
    /// otherwise read as positively idle. `Some(true)` only on turn activity
    /// (message chunks, tool calls, `activity: working`) or a queue with a
    /// prompt queued or running — a backlog counts as open because the
    /// leader runs it FIFO, so nothing dispatched now runs between turns;
    /// `Some(false)` only on `turn_completed` / `activity: idle`; `None`
    /// until one of those has been seen. The `session/load` replay counts:
    /// it is the engine's own turn history, and its last turn event says
    /// whether a turn was open at load time, so a client (re)connected to
    /// an idle session answers `Some(false)` without waiting for the next
    /// turn — while `busy` and the other display fields ignore the replay.
    /// Read through `GrokStdioClient::turn_open`, not `runtime()`, which
    /// stays None until a live notification. Not a runtime field.
    pub turn_open: Option<bool>,
}

impl Default for SessionRuntime {
    fn default() -> Self {
        SessionRuntime {
            busy: false,
            input_state: String::new(),
            session_id: None,
            observed_at: 0.0,
            turn_open: None,
        }
    }
}

fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// --------------------------------------------------------------------------
// the leader subprocess seam
// --------------------------------------------------------------------------

/// The stdio child as the client sees it. Production wraps a real
/// `grok agent --leader stdio` child; tests substitute a scripted fake.
pub(super) trait LeaderProc: Send + Sync {
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
        // No-op once the child was reaped: its pid may already be someone else's.
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

fn spawn_stdio_proc(argv: &[String]) -> io::Result<Arc<dyn LeaderProc>> {
    #[cfg(test)]
    {
        super::tests::stdio_spawn_override(argv)
    }
    #[cfg(not(test))]
    {
        Ok(Arc::new(RealProc::spawn(argv)?))
    }
}

// --------------------------------------------------------------------------
// one stdio client attached to one key's leader
// --------------------------------------------------------------------------

/// A one-shot flag with a timed wait.
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
    /// Session id of the `session/load` in flight: the one whose replayed
    /// history is turn evidence until the load response binds it.
    loading: Option<String>,
}

pub(super) struct ClientInner {
    proc: Arc<dyn LeaderProc>,
    /// Held across each stdin write: one message per line, never interleaved.
    io_lock: Mutex<()>,
    next_id: Mutex<u64>,
    state: Mutex<ClientShared>,
    pending: Mutex<HashMap<u64, Arc<Slot>>>,
    pub(super) closed: AtomicBool,
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
fn fail_pending(inner: &ClientInner) {
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
fn on_request(inner: &ClientInner, rid: &Value, method: &str, params: &Value) {
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
    state.runtime.observed_at = now_epoch();
}

fn on_notification(inner: &ClientInner, method: &str, params: &Value) {
    let mut state = inner.state.lock().unwrap();
    if !state.loaded {
        // session/load replays past updates: no live turn for the display
        // (`busy`, `input_state`, `observed_at` stay put), only turn
        // evidence — the history's last turn event is the session's state
        // at load time.
        fold_replayed_turn(&mut state, method, params);
        return;
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
                apply_activity(&mut state, entry.get("activity"));
            }
        }
        return;
    }
    if params.get("sessionId").and_then(Value::as_str) != state.runtime.session_id.as_deref() {
        return;
    }
    state.runtime.observed_at = now_epoch();
    match method {
        "session/update" => {
            let update = params.get("update").cloned().unwrap_or_else(|| json!({}));
            apply_update(&mut state, &update);
        }
        "_x.ai/session_notification" => {
            let kind = params
                .get("update")
                .and_then(|update| update.get("sessionUpdate"))
                .and_then(Value::as_str);
            if kind == Some("turn_completed") {
                state.runtime.busy = false;
                state.runtime.turn_open = Some(false);
                state.runtime.input_state = "ready".to_string();
            }
        }
        "_x.ai/queue/changed" => apply_queue(&mut state, params),
        _ => {}
    }
}

/// Fold `activity` — the leader's busy authority — into the runtime.
fn apply_activity(state: &mut ClientShared, activity: Option<&Value>) {
    state.runtime.observed_at = now_epoch();
    match activity.and_then(Value::as_str) {
        Some("working") => {
            state.runtime.busy = true;
            state.runtime.turn_open = Some(true);
        }
        Some("idle") => {
            state.runtime.busy = false;
            state.runtime.turn_open = Some(false);
            state.runtime.input_state = "ready".to_string();
        }
        _ => {}
    }
}

/// Fold one replayed notification of the loading session into `turn_open`
/// and nothing else.
fn fold_replayed_turn(state: &mut ClientShared, method: &str, params: &Value) {
    let Some(loading) = state.loading.as_deref() else {
        return;
    };
    if params.get("sessionId").and_then(Value::as_str) != Some(loading) {
        return;
    }
    let kind = params
        .get("update")
        .and_then(|update| update.get("sessionUpdate"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match method {
        "session/update" if update_opens_turn(kind) => state.runtime.turn_open = Some(true),
        "_x.ai/session_notification" if kind == "turn_completed" => {
            state.runtime.turn_open = Some(false)
        }
        _ => {}
    }
}

/// A `session/update` kind that only a running turn produces.
fn update_opens_turn(kind: &str) -> bool {
    kind == "tool_call" || kind == "tool_call_update" || MESSAGE_CHUNKS.contains(&kind)
}

fn apply_update(state: &mut ClientShared, update: &Value) {
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "tool_call" => {
            state.runtime.busy = true;
            state.runtime.turn_open = Some(true);
        }
        "tool_call_update" => {
            // An update on a tool call means the turn is running and any
            // permission it was blocked on has been decided.
            state.runtime.busy = true;
            state.runtime.turn_open = Some(true);
            state.runtime.input_state = "ready".to_string();
        }
        kind if MESSAGE_CHUNKS.contains(&kind) => {
            state.runtime.busy = true;
            state.runtime.turn_open = Some(true);
            if kind == "user_message_chunk" {
                let text = update
                    .get("content")
                    .and_then(|content| content.get("text"));
                note_ack(state, text);
            }
        }
        _ => {}
    }
}

/// Fold a queue snapshot: a queued entry or a running prompt is turn
/// evidence (the backlog runs FIFO behind the current turn), an empty
/// snapshot says nothing — the turn that drained it may still be running.
/// The ack match is separate and never touches the turn evidence.
fn apply_queue(state: &mut ClientShared, params: &Value) {
    let entries: Vec<&Value> = params
        .get("entries")
        .and_then(Value::as_array)
        .map(|entries| entries.iter().filter(|entry| entry.is_object()).collect())
        .unwrap_or_default();
    let running = params
        .get("runningText")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.is_empty());
    if !entries.is_empty() || running {
        state.runtime.turn_open = Some(true);
    }
    for entry in &entries {
        note_ack(state, entry.get("text"));
    }
    note_ack(state, params.get("runningText"));
}

fn note_ack(state: &ClientShared, text: Option<&Value>) {
    if let (Some(ack), Some(text)) = (state.ack.as_ref(), text) {
        if text.as_str() == Some(ack.text.as_str()) {
            ack.event.set();
        }
    }
}

fn reader_loop(inner: Arc<ClientInner>, stdout: Box<dyn Read + Send>) {
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
            (Some(method), Some(rid)) => on_request(&inner, &rid, &method, &params),
            (Some(method), None) => on_notification(&inner, &method, &params),
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
                        let mut state = inner.state.lock().unwrap();
                        state.loading = None;
                        if msg.get("error").is_none() {
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
    fail_pending(&inner);
}

/// `grok agent --leader stdio` subprocess bound to one daemon key's session.
pub struct GrokStdioClient {
    pub key: String,
    pub socket_path: String,
    pub(super) inner: Arc<ClientInner>,
    pub(super) reader: Mutex<Option<thread::JoinHandle<()>>>,
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
        let proc = spawn_stdio_proc(&argv)?;
        let stdout = proc
            .take_stdout()
            .ok_or_else(|| io::Error::other("stdout unavailable"))?;
        let inner = Arc::new(ClientInner {
            proc,
            io_lock: Mutex::new(()),
            next_id: Mutex::new(0),
            state: Mutex::new(ClientShared::default()),
            pending: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        });
        let reader_inner = inner.clone();
        let handle = thread::spawn(move || reader_loop(reader_inner, stdout));
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
        if let Some(session_id) = loads {
            self.inner.state.lock().unwrap().loading = Some(session_id.to_string());
        }
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
        let SessionRecord { session_id, cwd } = match read_session_key(&self.key) {
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
            INIT_TIMEOUT,
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
            LOAD_TIMEOUT,
            Some(&session_id),
        );
        loaded.get("result").is_some()
    }

    /// `initialize` then `session/new` with hive's minted id.
    ///
    /// The engine-first mint primitive: the leader materializes the session
    /// before any pane exists (spike-verified: the id must ride
    /// `_meta.sessionId` — a top-level `sessionId` is silently ignored and
    /// the server mints its own). Binds this client to the new session on
    /// success.
    pub fn new_session(&self, session_id: &str, cwd: &str) -> bool {
        let initialized = self.call(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientInfo": {"name": "hive", "version": "1"},
                "clientCapabilities": {},
            }),
            INIT_TIMEOUT,
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
            LOAD_TIMEOUT,
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
    ///
    /// The echo is also the turn evidence: both shapes open `turn_open` on
    /// the reader thread, in order with everything after them. This method
    /// writes none itself — by the time the ack wakes it the turn may already
    /// have completed (a short turn's `turn_completed`, or the prompt
    /// response landing first), and a late `Some(true)` would outlive it.
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
            if !sent || !done.wait(ack_timeout()) {
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
            CALL_TIMEOUT,
            None,
        );
        if result.get("result").is_some() {
            "compacted"
        } else {
            "unavailable"
        }
    }

    /// Turn evidence, replay included: `Some(false)` for a session whose
    /// history ends in a completed turn, `Some(true)` for one mid-turn,
    /// `None` while no turn event has been seen at all.
    pub fn turn_open(&self) -> Option<bool> {
        self.inner.state.lock().unwrap().runtime.turn_open
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
        fail_pending(&self.inner);
        self.inner.proc.close_stdin();
        self.inner.proc.terminate();
        self.inner.proc.wait(1.0);
    }
}

impl Drop for GrokStdioClient {
    /// A short-lived CLI (`hive compact`) drops without close(); without
    /// this the stdio child would outlive it and hold a leader connection
    /// forever.
    fn drop(&mut self) {
        self.inner.proc.terminate();
    }
}

fn ack_timeout() -> f64 {
    #[cfg(test)]
    {
        if let Some(timeout) = super::tests::ack_timeout_override() {
            return timeout;
        }
    }
    ACK_TIMEOUT
}

// --------------------------------------------------------------------------
// prompt results: what a prompt this client sent came back with
// --------------------------------------------------------------------------

/// One `session/prompt`'s outcome, read by the client that sent it. The
/// prompt request's own response is the turn's end (ACP: it returns when
/// the turn ends, with `stopReason`); the text is not in that response but
/// in the `session/update` `agent_message_chunk`s whose `_meta.promptId`
/// matches the response's. A turn with tool calls says something before
/// the tool and something after it: the chunks are split into segments at
/// each `tool_call`, and the result is the last non-empty segment — the
/// last thing the member said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptResult {
    Running,
    /// `stop_reason` is the response's `stopReason` (`end_turn`,
    /// `cancelled`, `max_tokens`, `max_turn_requests`, `refusal`), or
    /// `error` with `error` set when the response was a JSON-RPC error or
    /// the leader went away first.
    Ended {
        stop_reason: String,
        text: String,
        error: Option<String>,
    },
}

impl GrokStdioClient {
    /// Send `session/prompt` and keep its request id registered until the
    /// response lands, collecting the turn's text meanwhile; no echo wait —
    /// the response is the accept and the end in one. `Err` is nothing
    /// written (no loaded session, dead leader).
    pub fn prompt_tracked(&self, text: &str) -> Result<u64, String> {
        let _ = text;
        unimplemented!("prompt_tracked")
    }

    /// What this client has seen of the prompt sent as *rid*; None when
    /// it never sent it (a fresh client since).
    pub fn prompt_result(&self, rid: u64) -> Option<PromptResult> {
        let _ = rid;
        unimplemented!("prompt_result")
    }
}
