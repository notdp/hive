use super::*;
use crate::registry::TEST_ENV_LOCK;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};

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

fn set_stdio_spawn(factory: impl FnMut(&[String]) -> io::Result<Arc<dyn LeaderProc>> + 'static) {
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
fn test_spawn_daemon_reclaims_a_key_a_live_leader_still_locks() {
    // The state a pane rebuild leaves behind: grok's flock file names a
    // live leader, our pidfile was never written because that leader
    // never bound, and no socket exists. Without reclaiming the holder
    // every later spawn times out and the member falls back to plain
    // grok — reachable outward, deaf inward.
    let _bed = setup();
    let sock = pane_socket_path("%19");
    fs::create_dir_all(sock.parent().unwrap()).unwrap();
    fs::write(sock.with_extension("lock"), std::process::id().to_string()).unwrap();
    let killed: Arc<Mutex<Vec<libc::pid_t>>> = Arc::new(Mutex::new(Vec::new()));
    let killed_record = killed.clone();
    set_terminate_pg(move |pid| killed_record.lock().unwrap().push(pid));
    let lock_at_spawn: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
    let lock_probe = lock_at_spawn.clone();
    let lock_path = sock.with_extension("lock");
    set_daemon_spawn(move |argv, _env| {
        *lock_probe.lock().unwrap() = Some(lock_path.exists());
        touch_leader_socket(argv);
        Ok(Box::new(FakeDaemonChild {
            pid: 4242,
            returncode: None,
            panic_on_terminate: true,
        }))
    });
    assert!(spawn_daemon("%19"));
    assert_eq!(
        *killed.lock().unwrap(),
        vec![std::process::id() as libc::pid_t],
        "the lock holder must be terminated before respawning"
    );
    assert_eq!(
        *lock_at_spawn.lock().unwrap(),
        Some(false),
        "the stale lock must be gone before the new leader tries to bind"
    );
}

#[test]
fn test_spawn_daemon_leaves_a_dead_holder_alone() {
    let _bed = setup();
    let sock = pane_socket_path("%19");
    fs::create_dir_all(sock.parent().unwrap()).unwrap();
    // pid 0 is never a live process: nothing to reclaim.
    fs::write(sock.with_extension("lock"), "0").unwrap();
    set_terminate_pg(|pid| panic!("terminated {pid} for a dead holder"));
    set_daemon_spawn(|argv, _env| {
        touch_leader_socket(argv);
        Ok(Box::new(FakeDaemonChild {
            pid: 4243,
            returncode: None,
            panic_on_terminate: true,
        }))
    });
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
