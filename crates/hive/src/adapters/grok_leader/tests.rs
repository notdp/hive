use super::*;
use crate::testenv::EnvGuard;
use anyhow::Result;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn pane_pidfile_path(pane: &str) -> PathBuf {
    pane_socket_path(pane).with_extension("pid")
}

/// The (argv, env) one faked daemon spawn was called with.
type SeenDaemonSpawn = Arc<Mutex<Option<(Vec<String>, HashMap<String, String>)>>>;

const SID: &str = "11111111-2222-3333-4444-555555555555";
const CWD: &str = "/w/project";

fn binding(team: &str, created_at: &str, member: &str) -> RecordBinding {
    RecordBinding {
        team: team.to_string(),
        created_at: created_at.to_string(),
        member: member.to_string(),
    }
}

// ---- test seams ------------------------------------------------------

type StdioSpawn = Box<dyn FnMut(&[String]) -> io::Result<Arc<dyn LeaderProc>>>;
type DaemonSpawn =
    Box<dyn FnMut(&[String], &HashMap<String, String>) -> io::Result<Box<dyn DaemonChild>>>;
type ProcessListing = Box<dyn Fn() -> Vec<(libc::pid_t, String)>>;
type PaneOption = Box<dyn Fn(&str, &str) -> Option<String>>;
type TerminatePg = Box<dyn FnMut(libc::pid_t)>;
type ProcessArgs = Box<dyn Fn(libc::pid_t) -> Option<String>>;

thread_local! {
    static PANE_OPTION_OVERRIDE: RefCell<Option<PaneOption>> = RefCell::new(None);
    static STDIO_SPAWN_OVERRIDE: RefCell<Option<StdioSpawn>> = RefCell::new(None);
    static DAEMON_SPAWN_OVERRIDE: RefCell<Option<DaemonSpawn>> = RefCell::new(None);
    static TERMINATE_PG_OVERRIDE: RefCell<Option<TerminatePg>> = RefCell::new(None);
    static PROCESS_ARGS_OVERRIDE: RefCell<Option<ProcessArgs>> = RefCell::new(None);
    static PROCESS_LISTING_OVERRIDE: RefCell<Option<ProcessListing>> = RefCell::new(None);
    static ACK_TIMEOUT_OVERRIDE: Cell<Option<f64>> = const { Cell::new(None) };
}

/// Panes resolve to their raw pane key unless a test tags them; the real
/// tmux is never asked.
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

/// Outer None: no override, the real `ps` answers.
pub(super) fn process_args_override(pid: libc::pid_t) -> Option<Option<String>> {
    PROCESS_ARGS_OVERRIDE.with(|slot| slot.borrow().as_ref().map(|f| f(pid)))
}

/// The whole process table as the reap sees it — empty unless a test
/// scripts one, so no test ever reads the machine's real `ps`.
pub(super) fn process_listing_override() -> Vec<(libc::pid_t, String)> {
    PROCESS_LISTING_OVERRIDE.with(|slot| match slot.borrow().as_ref() {
        Some(listing) => listing(),
        None => Vec::new(),
    })
}

pub(super) fn ack_timeout_override() -> Option<f64> {
    ACK_TIMEOUT_OVERRIDE.with(|slot| slot.get())
}

thread_local! {
    static PANE_WRITE_INTERLEAVE: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
}

/// Runs once between a pane write's alias resolve and its launch lock.
pub(super) fn pane_write_interleave() {
    if let Some(hook) = PANE_WRITE_INTERLEAVE.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}

thread_local! {
    static RECORD_UPDATE_INTERLEAVE: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
}

/// Runs once inside a record update, after its read and before its write.
pub(super) fn record_update_interleave() {
    if let Some(hook) = RECORD_UPDATE_INTERLEAVE.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}

fn set_record_update_interleave(hook: impl FnOnce() + 'static) {
    RECORD_UPDATE_INTERLEAVE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

fn set_pane_write_interleave(hook: impl FnOnce() + 'static) {
    PANE_WRITE_INTERLEAVE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
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

fn set_process_args(args: impl Fn(libc::pid_t) -> Option<String> + 'static) {
    PROCESS_ARGS_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(Box::new(args)));
}

fn set_process_listing(listing: impl Fn() -> Vec<(libc::pid_t, String)> + 'static) {
    PROCESS_LISTING_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(Box::new(listing)));
}

/// A fake process table: `ps` lists it, a terminate logs the pid and drops
/// it, and an identity read answers from it. Returns the table and the log.
type ProcessTable = Arc<Mutex<Vec<(libc::pid_t, String)>>>;

fn set_process_table(procs: Vec<(libc::pid_t, String)>) -> (ProcessTable, KillLog) {
    let procs: ProcessTable = Arc::new(Mutex::new(procs));
    let killed: KillLog = Arc::new(Mutex::new(Vec::new()));
    let listed = procs.clone();
    set_process_listing(move || listed.lock().unwrap().clone());
    let named = procs.clone();
    set_process_args(move |pid| {
        named
            .lock()
            .unwrap()
            .iter()
            .find(|(seen, _args)| *seen == pid)
            .map(|(_pid, args)| args.clone())
    });
    let reaped = procs.clone();
    let log = killed.clone();
    set_terminate_pg(move |pid| {
        log.lock().unwrap().push(pid);
        reaped.lock().unwrap().retain(|(seen, _args)| *seen != pid);
    });
    (procs, killed)
}

pub(crate) type KillLog = Arc<Mutex<Vec<libc::pid_t>>>;

/// The command line of the leader that binds *sock*, as `ps` would print it.
fn leader_args(sock: &std::path::Path) -> String {
    format!(
        "grok agent leader --leader-socket {} --no-auto-update",
        sock.display()
    )
}

/// Make *pid* look like the leader of *sock* to the identity check.
fn set_leader_identity(pid: libc::pid_t, sock: &std::path::Path) {
    let args = leader_args(sock);
    set_process_args(move |seen| (seen == pid).then(|| args.clone()));
}

/// The pane TUI's command line: a leader client, not a leader.
fn tui_args(sock: &std::path::Path) -> String {
    format!(
        "grok --leader --leader-socket {} --session-id {SID}",
        sock.display()
    )
}

/// A stdio client's command line (hive's own, the hived's pool client).
fn stdio_args(sock: &std::path::Path) -> String {
    format!(
        "grok agent --leader stdio --leader-socket {}",
        sock.display()
    )
}

/// A live listener on *sock*, dropped when the guard goes.
/// A fake leader on *sock*: the listener and the flock a real leader holds
/// on `<key>.lock` for its lifetime (`daemon::probe_socket`); dropping it
/// releases both, the way a leader's exit does.
struct FakeLeader {
    listener: UnixListener,
    _lock: fs::File,
}

impl std::ops::Deref for FakeLeader {
    type Target = UnixListener;
    fn deref(&self) -> &UnixListener {
        &self.listener
    }
}

fn bind_leader_socket(sock: &std::path::Path) -> FakeLeader {
    fs::create_dir_all(sock.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(sock).unwrap();
    FakeLeader {
        listener,
        _lock: hold_leader_lock(sock),
    }
}

/// Take the leader's flock on `<key>.lock` beside *sock*, the pid inside
/// as grok writes it; held while the returned file lives.
pub(crate) fn hold_leader_lock(sock: &std::path::Path) -> fs::File {
    use std::os::unix::io::AsRawFd;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(sock.with_extension("lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    fs::write(sock.with_extension("lock"), std::process::id().to_string()).unwrap();
    file
}

thread_local! {
    /// Locks `touch_leader_socket` took, held for the rest of the test.
    static TOUCHED_LOCKS: RefCell<Vec<fs::File>> = const { RefCell::new(Vec::new()) };
}

// ---- fixtures for tests outside this module --------------------------

/// Watch the adapter's process seams from elsewhere in the crate: *procs*
/// is the table a reap reads, the returned log every pid it signalled. A
/// path that must not collect a member's engine proves it against a table
/// that gives it something to kill.
pub(crate) fn watch_process_signals(procs: Vec<(libc::pid_t, String)>) -> KillLog {
    set_process_table(procs).1
}

/// How `ps` prints the two processes on *key*'s socket: the leader grok
/// raised, and the TUI in the member's pane — what a reap of the key
/// signals, TUI first.
pub(crate) fn socket_process_args(key: &str) -> (String, String) {
    let sock = socket_path_for_key(key);
    (leader_args(&sock), tui_args(&sock))
}

/// The desk's own stdio client on *key*, pooled and idle: a fake leader
/// handshaken against a written session record — what a hived holds for a
/// member it has sent to. The handle says whether the child was signalled.
pub(crate) fn pool_idle_fake_client(key: &str) -> Arc<FakeProc> {
    write_session_key(key, SID, CWD, None).unwrap();
    let proc = FakeProc::new(Some(responder(None, Vec::new())));
    let handout = Arc::clone(&proc);
    set_stdio_spawn(move |_argv| Ok(handout.clone() as Arc<dyn LeaderProc>));
    let client = Arc::new(GrokStdioClient::new(key).unwrap());
    assert!(client.handshake());
    pool().hold_for_test(key, client);
    proc
}

/// Feed the pooled client on *key* one activity notification and wait for
/// its reader thread to take it.
pub(crate) fn feed_turn_open(proc: &FakeProc, key: &str, open: bool) {
    proc.feed(&activity(if open { "working" } else { "idle" }));
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if pool().turn_open_for_key(key) == Some(Some(open)) {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("turn evidence never became {open} on {key}");
}

/// Serialized test bed: env guard held, GROK_HOME pinned to a tempdir,
/// key cache and every thread-local seam reset.
struct TestBed {
    env: EnvGuard,
    tmp: tempfile::TempDir,
}

fn setup() -> TestBed {
    let mut env = EnvGuard::new();
    key_cache().lock().unwrap().clear();
    PANE_OPTION_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    STDIO_SPAWN_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    DAEMON_SPAWN_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    TERMINATE_PG_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    PROCESS_ARGS_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    PROCESS_LISTING_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    ACK_TIMEOUT_OVERRIDE.with(|slot| slot.set(None));
    let tmp = tempfile::tempdir().unwrap();
    env.set("GROK_HOME", tmp.path());
    TestBed { env, tmp }
}

// ---- fake subprocess -------------------------------------------------

type Responder = Box<dyn Fn(&Value) -> Vec<Value> + Send + Sync>;

pub(crate) struct FakeProc {
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

    /// Whether the client ever signalled this child.
    pub(crate) fn terminated(&self) -> bool {
        self.terminated.load(Ordering::SeqCst)
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

fn ok(msg: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": msg["id"], "result": result})
}

/// Answers the handshake; `extra` handles everything else.
fn responder(extra: Option<Responder>, replay: Vec<Value>) -> Responder {
    Box::new(
        move |msg: &Value| match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => vec![ok(msg, json!({"protocolVersion": 1}))],
            Some("session/load") => {
                let mut replies = replay.clone();
                replies.push(ok(msg, json!({"models": {"currentModelId": "grok-4.6"}})));
                replies
            }
            _ => extra.as_ref().map(|e| e(msg)).unwrap_or_default(),
        },
    )
}

/// A client on a fake leader for *pane*, its session record written first
/// when *session* is given.
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

fn loaded(respond: Option<Responder>, replay: Vec<Value>) -> (Arc<GrokStdioClient>, Arc<FakeProc>) {
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

fn settle(client: &GrokStdioClient, predicate: impl Fn(&SessionRuntime) -> bool) -> SessionRuntime {
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

fn settle_sent(proc: &FakeProc, predicate: impl Fn(&Value) -> bool) -> Value {
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

fn update_for(session_id: &str, kind: &str, fields: Value) -> Value {
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

fn update(kind: &str, fields: Value) -> Value {
    update_for(SID, kind, fields)
}

fn activity_for(activity: &str, session_id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/sessions/changed",
        "params": {"upserted": [
            {"sessionId": session_id, "activity": activity, "resident": true},
        ]},
    })
}

fn activity(activity: &str) -> Value {
    activity_for(activity, SID)
}

// ----------------------------------------------------------------------
// handshake
// ----------------------------------------------------------------------

#[test]
fn test_handshake_sends_initialize_then_session_load() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
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
            return vec![ok(msg, json!({"protocolVersion": 1}))];
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
        update(
            "agent_message_chunk",
            json!({"content": {"type": "text", "text": "old turn"}}),
        ),
        activity("working"),
    ];
    let (client, proc) = loaded(None, replay);
    assert!(client.runtime().is_none()); // replay is not evidence of a live turn
    teardown(&client, &proc);
}

fn turn_completed_for(session_id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/session/update",
        "params": {
            "sessionId": session_id,
            "update": {"sessionUpdate": "turn_completed", "stop_reason": "end_turn"},
        },
    })
}

#[test]
fn test_replayed_history_is_turn_evidence_but_not_display_state() {
    // The load replay is the engine's own turn history: its last turn
    // event says whether a turn is open at load time. The display fields
    // still ignore it.
    let _bed = setup();
    let chunk = || {
        update(
            "agent_message_chunk",
            json!({"content": {"type": "text", "text": "old turn"}}),
        )
    };

    // A completed turn: idle at load time.
    let (client, proc) = loaded(None, vec![turn_completed_for(SID)]);
    assert_eq!(client.turn_open(), Some(false));
    assert!(client.runtime().is_none());
    teardown(&client, &proc);

    // A turn that ran and completed.
    let (client, proc) = loaded(None, vec![chunk(), turn_completed_for(SID)]);
    assert_eq!(client.turn_open(), Some(false));
    assert!(client.runtime().is_none());
    teardown(&client, &proc);

    // A turn still running when the load happened.
    let (client, proc) = loaded(None, vec![chunk()]);
    assert_eq!(client.turn_open(), Some(true));
    assert!(client.runtime().is_none());
    teardown(&client, &proc);

    // Nothing replayed, or another session's history: no evidence.
    let (client, proc) = loaded(None, vec![]);
    assert_eq!(client.turn_open(), None);
    teardown(&client, &proc);
    let (client, proc) = loaded(
        None,
        vec![
            update_for("other-session", "agent_message_chunk", json!({})),
            turn_completed_for("other-session"),
        ],
    );
    assert_eq!(client.turn_open(), None);
    teardown(&client, &proc);
}

/// A turn end as the leader's `session/load` replay carries it, captured
/// from a real grok 1.0.30 leader during the install acceptance of the
/// lifecycle fixes (`_meta.isReplay`, usage, elapsed).
fn replayed_turn_completed(session_id: &str, prompt_id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "turn_completed",
                "prompt_id": prompt_id,
                "stop_reason": "end_turn",
                "usage": {"inputTokens": 125335, "outputTokens": 1403, "numTurns": 5},
                "elapsed_ms": 30327,
            },
            "_meta": {
                "eventId": format!("{session_id}-412"),
                "agentTimestampMs": 1789283323845u64,
                "isReplay": true,
                "x.ai/leaderClientId": 9,
            },
        },
    })
}

#[test]
fn test_replayed_turn_end_with_its_wire_shape_closes_the_turn() {
    // A desk woken from sleep reloads the member's history: a turn that
    // ran (message chunk, tool calls) and ended is a closed turn, so the
    // next dispatch is not `member_busy`.
    let _bed = setup();
    let history = vec![
        update_for(
            SID,
            "user_message_chunk",
            json!({"content": {"type": "text", "text": "task"}}),
        ),
        update_for(
            SID,
            "agent_message_chunk",
            json!({"content": {"type": "text", "text": "ok"}}),
        ),
        update_for(
            SID,
            "tool_call",
            json!({"toolCallId": "c1", "title": "write", "status": "completed"}),
        ),
        replayed_turn_completed(SID, "412fdb07-277a-4f23-9206-5689d6688efa"),
        json!({
            "jsonrpc": "2.0",
            "method": "_x.ai/session/update",
            "params": {"sessionId": SID, "update": {"sessionUpdate": "background_tasks", "tasks": []}},
        }),
    ];
    let (client, proc) = loaded(None, history);
    assert_eq!(client.turn_open(), Some(false));
    assert!(client.runtime().is_none());
    teardown(&client, &proc);

    // Another session's turn end is still not ours.
    let (client, proc) = loaded(
        None,
        vec![
            update_for(SID, "agent_message_chunk", json!({})),
            replayed_turn_completed("other-session", "p-other"),
        ],
    );
    assert_eq!(client.turn_open(), Some(true));
    teardown(&client, &proc);

    // Live, the same frame ends the turn the display shows.
    let (client, proc) = loaded(None, vec![]);
    proc.feed(&update(
        "agent_message_chunk",
        json!({"content": {"type": "text", "text": "new turn"}}),
    ));
    settle(&client, |rt| rt.busy);
    proc.feed(&replayed_turn_completed(SID, "p-live"));
    let runtime = settle(&client, |rt| !rt.busy);
    assert_eq!(runtime.turn_open, Some(false));
    assert_eq!(runtime.input_state, "ready");
    teardown(&client, &proc);
}

#[test]
fn test_live_turn_evidence_overrides_the_replayed_history() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![turn_completed_for(SID)]);
    assert_eq!(client.turn_open(), Some(false));
    proc.feed(&update(
        "agent_message_chunk",
        json!({"content": {"type": "text", "text": "new turn"}}),
    ));
    let runtime = settle(&client, |rt| rt.busy);
    assert_eq!(runtime.turn_open, Some(true));
    assert_eq!(client.turn_open(), Some(true));
    teardown(&client, &proc);
}

#[test]
fn test_notification_right_behind_the_load_response_is_folded() {
    // A live turn queued behind the load response must not count as replay.
    let _bed = setup();
    let respond: Responder =
        Box::new(
            |msg: &Value| match msg.get("method").and_then(Value::as_str) {
                Some("initialize") => vec![ok(msg, json!({"protocolVersion": 1}))],
                Some("session/load") => vec![
                    ok(msg, json!({"models": {"currentModelId": "grok-4.6"}})),
                    activity("working"),
                ],
                _ => vec![],
            },
        );
    let (client, proc) = make(Some(respond), Some((SID, CWD)), "%19");
    assert!(client.handshake());
    settle(&client, |rt| rt.busy);
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
    let (client, proc) = loaded(None, vec![]);
    proc.feed(&activity("working"));
    let runtime = settle(&client, |rt| rt.busy);
    assert_eq!(runtime.session_id.as_deref(), Some(SID));
    assert_eq!(runtime.turn_open, Some(true));
    proc.feed(&activity("idle"));
    let runtime = settle(&client, |rt| !rt.busy);
    assert_eq!(runtime.input_state, "ready");
    assert_eq!(runtime.turn_open, Some(false));
    teardown(&client, &proc);
}

#[test]
fn test_message_chunks_mark_busy() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    proc.feed(&update(
        "agent_thought_chunk",
        json!({"content": {"type": "text", "text": "The"}}),
    ));
    let runtime = settle(&client, |rt| rt.busy);
    assert_eq!(runtime.turn_open, Some(true));
    teardown(&client, &proc);
}

#[test]
fn test_tool_call_marks_busy() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    proc.feed(&update(
        "tool_call",
        json!({"toolCallId": "c1", "status": "pending"}),
    ));
    let runtime = settle(&client, |rt| rt.busy);
    assert_eq!(runtime.turn_open, Some(true));
    teardown(&client, &proc);
}

#[test]
fn test_late_joined_tool_call_update_marks_busy() {
    // attaching mid-tool: the opening tool_call was never seen, the update is
    // the only evidence that a turn is running
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    proc.feed(&update(
        "tool_call_update",
        json!({"toolCallId": "c1", "status": "in_progress"}),
    ));
    let runtime = settle(&client, |rt| rt.busy);
    assert_eq!(runtime.turn_open, Some(true));
    teardown(&client, &proc);
}

#[test]
fn test_tool_call_update_clears_a_decided_permission() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    proc.feed(&json!({
        "jsonrpc": "2.0",
        "id": 78,
        "method": "session/request_permission",
        "params": {"sessionId": SID, "toolCall": {"toolCallId": "c1"}, "options": []},
    }));
    settle(&client, |rt| rt.input_state == "waiting_user");
    // the human answered at the TUI: the tool ran, so nothing waits on input
    proc.feed(&update(
        "tool_call_update",
        json!({"toolCallId": "c1", "status": "completed"}),
    ));
    settle(&client, |rt| rt.input_state == "ready");
    teardown(&client, &proc);
}

#[test]
fn test_turn_completed_clears_busy() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    proc.feed(&activity("working"));
    settle(&client, |rt| rt.busy);
    proc.feed(&json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/session/update",
        "params": {
            "sessionId": SID,
            "update": {"sessionUpdate": "turn_completed", "stop_reason": "end_turn"},
        },
    }));
    let runtime = settle(&client, |rt| !rt.busy);
    assert_eq!(runtime.input_state, "ready");
    assert_eq!(runtime.turn_open, Some(false));
    teardown(&client, &proc);
}

#[test]
fn test_other_session_notifications_are_ignored() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    proc.feed(&update(
        "tool_call",
        json!({"toolCallId": "c1", "status": "pending"}),
    ));
    let baseline = settle(&client, |rt| rt.busy);
    proc.feed(&activity_for("idle", "other-session"));
    proc.feed(&update_for(
        "other-session",
        "agent_message_chunk",
        json!({"content": {"text": "hi"}}),
    ));
    // same-session no-op marker: the reader folds it only after the two lines
    // above, so its observed_at bump proves they were seen and dropped
    proc.feed(&activity("working"));
    let runtime = settle(&client, |rt| rt.observed_at > baseline.observed_at);
    assert!(runtime.busy);
    assert_eq!(runtime.input_state, ""); // the foreign idle never closed it
    teardown(&client, &proc);
}

#[test]
fn test_unknown_updates_are_ignored() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    proc.feed(&update(
        "available_commands_update",
        json!({"availableCommands": [{"name": "compact"}]}),
    ));
    let first = settle(&client, |_rt| true);
    // the second ignored line is its own marker: an in-session notification
    // bumps observed_at even when nothing folds it
    proc.feed(&json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/announcements/update",
        "params": {"sessionId": SID},
    }));
    let runtime = settle(&client, |rt| rt.observed_at > first.observed_at);
    assert!(!runtime.busy);
    // seen, but neither is turn evidence: the runtime still has no answer
    // about the turn, not a positive idle
    assert_eq!(runtime.turn_open, None);
    teardown(&client, &proc);
}

#[test]
fn test_queue_backlog_opens_the_turn() {
    // a queued entry runs FIFO behind whatever is running: not between turns
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    proc.feed(&json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/queue/changed",
        "params": {
            "sessionId": SID,
            "entries": [{"id": "p1", "kind": "prompt", "text": "someone else", "position": 0}],
        },
    }));
    let runtime = settle(&client, |rt| rt.turn_open.is_some());
    assert_eq!(runtime.turn_open, Some(true));
    assert!(!runtime.busy); // display busy is the activity authority's
    teardown(&client, &proc);
}

#[test]
fn test_queue_running_text_opens_the_turn() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    proc.feed(&json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/queue/changed",
        "params": {
            "sessionId": SID,
            "entries": [],
            "runningText": "someone else",
            "runningKind": "prompt",
        },
    }));
    let runtime = settle(&client, |rt| rt.turn_open.is_some());
    assert_eq!(runtime.turn_open, Some(true));
    teardown(&client, &proc);
}

#[test]
fn test_empty_queue_says_nothing_about_the_turn() {
    // the queue draining is not the turn ending: the entry it handed over
    // may still be running
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    proc.feed(&json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/queue/changed",
        "params": {"sessionId": SID, "entries": [], "runningText": null},
    }));
    let runtime = settle(&client, |_rt| true);
    assert_eq!(runtime.turn_open, None);
    proc.feed(&activity("working"));
    let before = settle(&client, |rt| rt.turn_open == Some(true));
    proc.feed(&json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/queue/changed",
        "params": {"sessionId": SID, "entries": []},
    }));
    // same-session marker after the empty snapshot proves it was folded
    proc.feed(&update(
        "available_commands_update",
        json!({"availableCommands": []}),
    ));
    let runtime = settle(&client, |rt| rt.observed_at > before.observed_at);
    assert_eq!(runtime.turn_open, Some(true));
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
    let (client, proc) = loaded(
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
    // the echo that accepted hive's own prompt is the turn evidence
    assert_eq!(client.runtime().unwrap().turn_open, Some(true));
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
    let (client, proc) = loaded(Some(responder(Some(on_prompt), vec![])), vec![]);
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
        vec![update(
            "user_message_chunk",
            json!({"content": {"type": "text", "text": text}}),
        )]
    });
    let (client, proc) = loaded(Some(responder(Some(on_prompt), vec![])), vec![]);
    assert!(GrokStdioClient::prompt(&client, "hello grok"));
    assert_eq!(client.runtime().unwrap().turn_open, Some(true));
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
    let (client, proc) = loaded(Some(responder(Some(on_prompt), vec![])), vec![]);
    assert!(!GrokStdioClient::prompt(&client, "hello grok"));
    teardown(&client, &proc);
}

#[test]
fn test_prompt_false_when_never_acked() {
    let _bed = setup();
    ACK_TIMEOUT_OVERRIDE.with(|slot| slot.set(Some(0.05)));
    let (client, proc) = loaded(None, vec![]); // nothing answers session/prompt
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
        vec![update(
            "user_message_chunk",
            json!({"content": {"type": "text", "text": "someone else"}}),
        )]
    });
    let (client, proc) = loaded(Some(responder(Some(on_prompt), vec![])), vec![]);
    assert!(!GrokStdioClient::prompt(&client, "hello grok"));
    teardown(&client, &proc);
}

// ----------------------------------------------------------------------
// tracked prompts
// ----------------------------------------------------------------------

const P: &str = "prompt-ours";
const P_OTHER: &str = "prompt-theirs";

/// A `session/update` of the session, stamped `_meta.promptId` when given.
fn tracked_update(prompt_id: Option<&str>, kind: &str, fields: Value) -> Value {
    let mut msg = update(kind, fields);
    if let Some(prompt_id) = prompt_id {
        msg["params"]["_meta"] = json!({"promptId": prompt_id});
    }
    msg
}

fn agent_chunk(prompt_id: Option<&str>, text: &str) -> Value {
    tracked_update(
        prompt_id,
        "agent_message_chunk",
        json!({"content": {"type": "text", "text": text}}),
    )
}

fn thought_chunk(prompt_id: &str, text: &str) -> Value {
    tracked_update(
        Some(prompt_id),
        "agent_thought_chunk",
        json!({"content": {"type": "text", "text": text}}),
    )
}

fn tool_call(prompt_id: &str, id: &str) -> Value {
    tracked_update(
        Some(prompt_id),
        "tool_call",
        json!({"toolCallId": id, "title": "run_terminal_command", "status": "pending"}),
    )
}

fn turn_completed(prompt_id: &str, stop_reason: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/session/update",
        "params": {
            "sessionId": SID,
            "update": {
                "sessionUpdate": "turn_completed",
                "prompt_id": prompt_id,
                "stop_reason": stop_reason,
            },
        },
    })
}

fn prompt_complete(prompt_id: &str, stop_reason: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/session/prompt_complete",
        "params": {"sessionId": SID, "promptId": prompt_id, "stopReason": stop_reason},
    })
}

/// The `session/prompt` response: the turn's end, carrying the prompt id.
fn prompt_response(rid: u64, prompt_id: &str, stop_reason: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": rid,
        "result": {
            "stopReason": stop_reason,
            "_meta": {"sessionId": SID, "requestId": prompt_id, "promptId": prompt_id},
        },
    })
}

fn ended(stop_reason: &str, text: &str) -> PromptResult {
    PromptResult::Ended {
        stop_reason: stop_reason.to_string(),
        text: text.to_string(),
        error: None,
    }
}

/// The frames of one prompt's turn, the echo through the completion — no
/// response, so the test decides when the turn ends.
fn tool_turn_frames(text: &str) -> Vec<Value> {
    vec![
        json!({
            "jsonrpc": "2.0",
            "method": "_x.ai/queue/changed",
            "params": {"sessionId": SID, "entries": [{"id": "q1", "kind": "prompt", "text": text}]},
        }),
        json!({
            "jsonrpc": "2.0",
            "method": "_x.ai/queue/changed",
            "params": {"sessionId": SID, "entries": [], "runningText": text},
        }),
        tracked_update(
            None,
            "user_message_chunk",
            json!({"content": {"type": "text", "text": text}}),
        ),
        thought_chunk(P, "The"),
        thought_chunk(P, " task"),
        agent_chunk(Some(P), "SAGE_"),
        agent_chunk(Some(P), "PREAMBLE"),
        tracked_update(
            Some(P),
            "available_commands_update",
            json!({"availableCommands": []}),
        ),
        tool_call(P, "call-1"),
        tracked_update(
            None,
            "tool_call_update",
            json!({"toolCallId": "call-1", "status": "in_progress"}),
        ),
        tracked_update(
            Some(P),
            "tool_call_update",
            json!({"toolCallId": "call-1", "status": "completed"}),
        ),
        thought_chunk(P, "Done"),
        agent_chunk(Some(P), "SAGE_"),
        agent_chunk(Some(P), "FINAL"),
        turn_completed(P, "end_turn"),
        prompt_complete(P, "end_turn"),
    ]
}

fn feed_all(proc: &FakeProc, frames: Vec<Value>) {
    for frame in frames {
        proc.feed(&frame);
    }
}

fn settle_result(
    client: &GrokStdioClient,
    rid: u64,
    predicate: impl Fn(&Option<PromptResult>) -> bool,
) -> Option<PromptResult> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let result = client.prompt_result(rid);
        if predicate(&result) {
            return result;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "prompt result never matched: {:?}",
        client.prompt_result(rid)
    );
}

fn settle_ended(client: &GrokStdioClient, rid: u64) -> PromptResult {
    settle_result(client, rid, |result| {
        matches!(result, Some(PromptResult::Ended { .. }))
    })
    .unwrap()
}

#[test]
fn test_prompt_tracked_writes_the_prompt_and_runs_until_the_response() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    let rid = client.prompt_tracked("do the task").unwrap();
    let sent = settle_sent(&proc, |msg| msg["method"] == "session/prompt");
    assert_eq!(sent["id"], json!(rid));
    assert_eq!(
        sent["params"],
        json!({"sessionId": SID, "prompt": [{"type": "text", "text": "do the task"}]})
    );
    assert_eq!(client.prompt_result(rid), Some(PromptResult::Running));

    feed_all(&proc, tool_turn_frames("do the task"));
    // turn_completed is folded (busy drops) yet the prompt still runs: the
    // response, not the notification, is the end
    let runtime = settle(&client, |rt| rt.turn_open == Some(false));
    assert!(!runtime.busy);
    assert_eq!(client.prompt_result(rid), Some(PromptResult::Running));

    proc.feed(&prompt_response(rid, P, "end_turn"));
    assert_eq!(settle_ended(&client, rid), ended("end_turn", "SAGE_FINAL"));
    teardown(&client, &proc);
}

#[test]
fn test_prompt_tracked_text_is_the_whole_message_without_a_tool_call() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    let rid = client.prompt_tracked("say hi").unwrap();
    feed_all(
        &proc,
        vec![
            thought_chunk(P, "Hmm"),
            agent_chunk(Some(P), "Hello, "),
            agent_chunk(Some(P), "world"),
            agent_chunk(Some(P), "!"),
            turn_completed(P, "end_turn"),
            prompt_response(rid, P, "end_turn"),
        ],
    );
    assert_eq!(
        settle_ended(&client, rid),
        ended("end_turn", "Hello, world!")
    );
    teardown(&client, &proc);
}

#[test]
fn test_prompt_tracked_text_falls_back_to_the_preamble_when_nothing_follows_the_tool() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    let rid = client.prompt_tracked("run it").unwrap();
    feed_all(
        &proc,
        vec![
            agent_chunk(Some(P), "Running the "),
            agent_chunk(Some(P), "script now."),
            tool_call(P, "call-1"),
            tracked_update(
                Some(P),
                "tool_call_update",
                json!({"toolCallId": "call-1", "status": "completed"}),
            ),
            tool_call(P, "call-2"),
            turn_completed(P, "end_turn"),
            prompt_response(rid, P, "end_turn"),
        ],
    );
    assert_eq!(
        settle_ended(&client, rid),
        ended("end_turn", "Running the script now.")
    );
    teardown(&client, &proc);
}

#[test]
fn test_prompt_tracked_keeps_only_its_own_prompt_when_a_foreign_one_ran_first() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    let rid = client.prompt_tracked("ours").unwrap();
    feed_all(
        &proc,
        vec![
            // the human's prompt, queued before ours, runs to its own end
            agent_chunk(Some(P_OTHER), "theirs "),
            tool_call(P_OTHER, "call-0"),
            agent_chunk(Some(P_OTHER), "and theirs again"),
            turn_completed(P_OTHER, "end_turn"),
            prompt_complete(P_OTHER, "end_turn"),
            // now ours
            agent_chunk(Some(P), "ours "),
            agent_chunk(Some(P), "only"),
            turn_completed(P, "end_turn"),
            prompt_response(rid, P, "end_turn"),
        ],
    );
    assert_eq!(settle_ended(&client, rid), ended("end_turn", "ours only"));
    teardown(&client, &proc);
}

#[test]
fn test_prompt_tracked_ignores_chunks_without_a_prompt_id() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    let rid = client.prompt_tracked("echo").unwrap();
    feed_all(
        &proc,
        vec![
            agent_chunk(None, "not attributed"),
            thought_chunk(P, "thinking"),
            agent_chunk(Some(P), "attributed"),
            prompt_response(rid, P, "end_turn"),
        ],
    );
    assert_eq!(settle_ended(&client, rid), ended("end_turn", "attributed"));
    teardown(&client, &proc);
}

#[test]
fn test_prompt_tracked_cancelled_response_keeps_the_collected_text() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    let rid = client.prompt_tracked("long task").unwrap();
    feed_all(
        &proc,
        vec![
            agent_chunk(Some(P), "partial"),
            turn_completed(P, "cancelled"),
            prompt_response(rid, P, "cancelled"),
        ],
    );
    assert_eq!(settle_ended(&client, rid), ended("cancelled", "partial"));
    teardown(&client, &proc);
}

#[test]
fn test_prompt_tracked_error_response_ends_with_the_error() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    let rid = client.prompt_tracked("bad").unwrap();
    proc.feed(&json!({
        "jsonrpc": "2.0",
        "id": rid,
        "error": {"code": -32602, "message": "unknown session id"},
    }));
    match settle_ended(&client, rid) {
        PromptResult::Ended {
            stop_reason,
            text,
            error,
        } => {
            assert_eq!(stop_reason, "error");
            assert_eq!(text, "");
            assert!(error.unwrap().contains("unknown session id"));
        }
        other => panic!("{other:?}"),
    }
    teardown(&client, &proc);
}

#[test]
fn test_prompt_tracked_leader_eof_ends_with_closed_and_stays_readable() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    let rid = client.prompt_tracked("task").unwrap();
    proc.feed(&agent_chunk(Some(P), "half"));
    settle(&client, |rt| rt.busy);
    proc.eof();
    assert_eq!(
        settle_ended(&client, rid),
        PromptResult::Ended {
            stop_reason: "error".to_string(),
            text: String::new(),
            error: Some("closed".to_string()),
        }
    );
    assert!(!client.is_alive());
    teardown(&client, &proc);
    assert!(matches!(
        client.prompt_result(rid),
        Some(PromptResult::Ended { .. })
    ));
}

#[test]
fn test_prompt_tracked_err_without_a_loaded_session() {
    let _bed = setup();
    let (client, proc) = make(Some(responder(None, vec![])), Some((SID, CWD)), "%19");
    assert!(client.prompt_tracked("hello").is_err());
    assert!(proc.sent().is_empty());
    teardown(&client, &proc);
}

#[test]
fn test_prompt_tracked_err_when_the_write_fails() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    proc.set_write_fail();
    assert!(client.prompt_tracked("hello").is_err());
    teardown(&client, &proc);
}

#[test]
fn test_prompt_result_none_for_a_rid_this_client_never_sent() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    assert_eq!(client.prompt_result(99), None);
    teardown(&client, &proc);
}

#[test]
fn test_prompt_result_keeps_the_last_sixty_four_ended_prompts() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    let rids: Vec<u64> = (0..65)
        .map(|i| {
            let rid = client.prompt_tracked(&format!("task {i}")).unwrap();
            let prompt_id = format!("p{i}");
            proc.feed(&agent_chunk(Some(&prompt_id), "ok"));
            proc.feed(&prompt_response(rid, &prompt_id, "end_turn"));
            rid
        })
        .collect();
    assert_eq!(settle_ended(&client, rids[64]), ended("end_turn", "ok"));
    assert_eq!(client.prompt_result(rids[0]), None);
    assert_eq!(client.prompt_result(rids[1]), Some(ended("end_turn", "ok")));
    teardown(&client, &proc);
}

#[test]
fn test_prompt_tracked_leaves_the_echo_ack_path_alone() {
    // An ordinary prompt() racing a tracked one still acks on its own echo.
    let _bed = setup();
    let (client, proc) = loaded(
        Some(responder(Some(on_prompt_queue_echo()), vec![])),
        vec![],
    );
    let rid = client.prompt_tracked("tracked").unwrap();
    assert!(GrokStdioClient::prompt(&client, "plain"));
    assert_eq!(client.prompt_result(rid), Some(PromptResult::Running));
    proc.feed(&prompt_response(rid, P, "end_turn"));
    assert_eq!(settle_ended(&client, rid), ended("end_turn", ""));
    teardown(&client, &proc);
}

// ----------------------------------------------------------------------
// permission requests
// ----------------------------------------------------------------------

#[test]
fn test_tracked_prompt_waits_for_the_tui_permission_decision() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
    let rid = client.prompt_tracked("write the requested file").unwrap();
    proc.feed(&agent_chunk(Some(P), "preparing"));
    proc.feed(&json!({
        "jsonrpc": "2.0",
        "id": 77,
        "method": "session/request_permission",
        "params": {
            "sessionId": SID,
            "toolCall": {"toolCallId": "write-file", "title": "write file"},
            "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}],
        },
    }));
    let runtime = settle(&client, |rt| rt.input_state == "waiting_user");
    assert!(runtime.busy);
    assert_eq!(client.prompt_result(rid), Some(PromptResult::Running));
    assert!(!proc
        .sent()
        .iter()
        .any(|msg| msg.get("id") == Some(&json!(77))));
    assert!(!proc
        .sent()
        .iter()
        .any(|msg| msg["method"] == "session/cancel"));

    // The leader forwards the tool result after the TUI answers its modal.
    proc.feed(&update(
        "tool_call_update",
        json!({"toolCallId": "write-file", "status": "completed"}),
    ));
    settle(&client, |rt| rt.input_state == "ready");
    proc.feed(&turn_completed(P, "end_turn"));
    proc.feed(&prompt_response(rid, P, "end_turn"));
    assert_eq!(settle_ended(&client, rid), ended("end_turn", "preparing"));
    teardown(&client, &proc);
}

#[test]
fn test_permission_request_marks_waiting_user_without_answering() {
    let _bed = setup();
    let (client, proc) = loaded(None, vec![]);
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
    let runtime = settle(&client, |rt| rt.input_state == "waiting_user");
    assert!(!proc
        .sent()
        .iter()
        .any(|msg| msg.get("id") == Some(&json!(77))));
    // a prompt alone is not turn evidence
    assert_eq!(runtime.turn_open, None);
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
    let (client, proc) = loaded(None, vec![]);
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
    let (client, proc) = loaded(None, vec![]);
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
            vec![ok(msg, json!({}))]
        } else {
            vec![]
        }
    })
}

#[test]
fn test_compact_returns_compacted_when_idle() {
    let _bed = setup();
    let (client, proc) = loaded(Some(responder(Some(on_compact_ok()), vec![])), vec![]);
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
    let (client, proc) = loaded(Some(responder(Some(on_compact), vec![])), vec![]);
    proc.feed(&activity("working"));
    settle(&client, |rt| rt.busy);
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
    let (client, proc) = loaded(Some(responder(Some(on_compact), vec![])), vec![]);
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
    let (client, proc) = loaded(None, vec![]);
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
    let mut bed = setup();
    bed.env.remove("GROK_HOME");
    assert!(pane_socket_path("%19").to_string_lossy().len() < 104);
}

#[test]
fn test_pane_session_path_shares_the_socket_stem() {
    let _bed = setup();
    assert_eq!(pane_socket_path("%19").file_name().unwrap(), "p19.sock");
    assert_eq!(pane_session_path("%19").file_name().unwrap(), "p19.session");
    assert_eq!(
        pane_session_path("%19").parent(),
        pane_socket_path("%19").parent()
    );
}

#[test]
fn test_pane_session_round_trip() {
    let _bed = setup();
    write_pane_session("%19", SID, CWD).unwrap();
    assert_eq!(
        read_pane_session("%19"),
        Some(SessionRecord {
            session_id: SID.to_string(),
            cwd: CWD.to_string(),
            binding: None,
        })
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
    assert_eq!(key_from_socket_name("p19.sock").as_deref(), Some("p19"));
    assert_eq!(
        key_from_socket_name("m-honey.rex.sock").as_deref(),
        Some("m-honey.rex")
    );
    assert_eq!(
        key_from_socket_name("m-honey.rex.dot.sock").as_deref(),
        Some("m-honey.rex.dot")
    );
    assert_eq!(key_from_socket_name("pdefault.sock"), None);
    assert_eq!(key_from_socket_name("m-noseparator.sock"), None);
    assert_eq!(key_from_socket_name("p19.pid"), None);
    assert_eq!(key_from_socket_name("leader.sock"), None);
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
fn test_write_pane_session_keeps_the_mint_binding_on_the_same_session() {
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
    let binding = RecordBinding {
        team: "honey".to_string(),
        created_at: "1700000000".to_string(),
        member: "rex".to_string(),
    };
    write_session_key("m-honey.rex", "sid-1", "/w", Some(&binding)).unwrap();
    // the pane TUI launched onto the minted session writes its record
    write_pane_session("%9", "sid-1", "/w2").unwrap();
    let record = read_session_key("m-honey.rex").unwrap();
    assert_eq!(record.session_id, "sid-1");
    assert_eq!(record.cwd, "/w2");
    assert_eq!(record.binding, Some(binding));
}

#[test]
fn test_write_pane_session_unbinds_another_session_on_a_member_key() {
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
    let binding = RecordBinding {
        team: "honey".to_string(),
        created_at: "1700000000".to_string(),
        member: "rex".to_string(),
    };
    write_session_key("m-honey.rex", "sid-1", "/w", Some(&binding)).unwrap();
    write_pane_session("%9", "sid-2", "/w").unwrap();
    let record = read_session_key("m-honey.rex").unwrap();
    assert_eq!(record.session_id, "sid-2");
    assert_eq!(record.binding, None);
    // an untagged pane never carries a binding
    write_pane_session("%7", "sid-3", "/w").unwrap();
    assert_eq!(read_session_key("p7").unwrap().binding, None);
}

fn tag_cedar_worker() {
    let mut tags = HashMap::new();
    tags.insert(
        ("%9".to_string(), "hive-team".to_string()),
        "cedar".to_string(),
    );
    tags.insert(
        ("%9".to_string(), "hive-agent".to_string()),
        "worker".to_string(),
    );
    set_pane_options(tags);
}

#[test]
fn test_write_pane_session_on_an_aliased_member_waits_for_the_launch_lock() {
    let bed = setup();
    let _listener = bind_leader_socket(&bed.tmp.path().join("hive/l-ab12.sock"));
    bind_launch("l-ab12", SID, CWD, "cedar", "123", "worker", "%9").unwrap();
    let held = launch_lock("l-ab12").unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let writer = thread::spawn(move || {
        tag_cedar_worker();
        started_tx.send(()).unwrap();
        let result = write_pane_session("%9", SID, "/changed-by-pane");
        done_tx.send(()).unwrap();
        result
    });
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let completed_under_lock = done_rx.recv_timeout(Duration::from_millis(300)).is_ok();
    let record_under_lock = read_session_key("l-ab12").unwrap();
    drop(held);
    writer.join().unwrap().unwrap();
    assert!(
        !completed_under_lock,
        "pane writer finished under a held launch lock"
    );
    assert_eq!(record_under_lock.cwd, CWD);
    let after = read_session_key("l-ab12").unwrap();
    assert_eq!(after.cwd, "/changed-by-pane");
    assert_eq!(after.binding, Some(binding("cedar", "123", "worker")));
}

#[test]
fn test_write_pane_session_refuses_a_member_rebound_before_its_lock() {
    let mut bed = setup();
    bed.env.set("HIVE_HOME", bed.tmp.path().join("home"));
    tag_cedar_worker();
    let _old_listener = bind_leader_socket(&bed.tmp.path().join("hive/l-ab12.sock"));
    let _new_listener = bind_leader_socket(&bed.tmp.path().join("hive/l-cd34.sock"));
    bind_launch("l-ab12", SID, CWD, "cedar", "123", "worker", "%9").unwrap();
    set_pane_write_interleave(|| {
        rollback_launch("l-ab12", SID, "cedar", "123", "worker", "%9").unwrap();
        bind_launch(
            "l-cd34",
            "new-session",
            "/new-cwd",
            "cedar",
            "456",
            "worker",
            "%9",
        )
        .unwrap();
        record_cedar("456", Some("new-session"));
        assert!(binding_holds("m-cedar.worker").is_ok());
    });
    let err = write_pane_session("%9", SID, "/old-pane").unwrap_err();
    assert!(err.to_string().contains("no longer resolves"), "{err}");
    assert_eq!(
        read_session_key("l-cd34").unwrap(),
        SessionRecord {
            session_id: "new-session".to_string(),
            cwd: "/new-cwd".to_string(),
            binding: Some(binding("cedar", "456", "worker")),
        }
    );
    assert!(binding_holds("m-cedar.worker").is_ok());
}

#[test]
fn test_write_pane_session_on_an_aliased_member_keeps_the_launch_binding() {
    let mut bed = setup();
    bed.env.set("HIVE_HOME", bed.tmp.path().join("home"));
    tag_cedar_worker();
    let _listener = bind_leader_socket(&bed.tmp.path().join("hive/l-ab12.sock"));
    bind_launch("l-ab12", SID, CWD, "cedar", "123", "worker", "%9").unwrap();
    record_cedar("123", Some(SID));
    write_pane_session("%9", SID, "/updated-cwd").unwrap();
    assert!(binding_holds("m-cedar.worker").is_ok());
    assert_eq!(read_session_key("l-ab12").unwrap().cwd, "/updated-cwd");
}

#[test]
fn test_write_pane_session_does_not_follow_an_alias_published_after_its_resolve() {
    let mut bed = setup();
    retained_member(&mut bed, "m-cedar.worker");
    tag_cedar_worker();
    let _old_listener = bind_leader_socket(&bed.tmp.path().join("hive/m-cedar.worker.sock"));
    let _new_listener = bind_leader_socket(&bed.tmp.path().join("hive/l-cd34.sock"));
    assert_eq!(canonical_key("m-cedar.worker"), "m-cedar.worker");
    assert!(binding_holds("m-cedar.worker").is_ok());
    set_pane_write_interleave(|| {
        kill_daemon_key("m-cedar.worker");
        bind_launch(
            "l-cd34",
            "new-session",
            "/new-cwd",
            "cedar",
            "456",
            "worker",
            "%9",
        )
        .unwrap();
        record_cedar("456", Some("new-session"));
        assert!(binding_holds("m-cedar.worker").is_ok());
    });
    // the member's own record is gone and its row names the new launch's
    // session: nothing to update, nothing to mint
    let err = write_pane_session("%9", SID, "/old-pane").unwrap_err();
    assert!(err.to_string().contains("already names session"), "{err}");
    assert_eq!(
        read_session_key("l-cd34").unwrap(),
        SessionRecord {
            session_id: "new-session".to_string(),
            cwd: "/new-cwd".to_string(),
            binding: Some(binding("cedar", "456", "worker")),
        }
    );
    assert!(!bed.tmp.path().join("hive/m-cedar.worker.session").exists());
    assert!(binding_holds("m-cedar.worker").is_ok());
}

#[test]
fn test_write_pane_session_creates_a_pane_record_but_not_a_member_one() {
    let bed = setup();
    write_pane_session("%7", "sid-3", "/w").unwrap();
    assert_eq!(read_session_key("p7").unwrap().session_id, "sid-3");
    tag_cedar_worker();
    // a member key with no record and no roster row awaiting a session
    let err = write_pane_session("%9", SID, CWD).unwrap_err();
    assert!(
        err.to_string().contains("not on the roster")
            || err.to_string().contains("not in the registry"),
        "{err}"
    );
    assert!(!bed.tmp.path().join("hive/m-cedar.worker.session").exists());
}

#[test]
fn test_write_pane_session_does_not_recreate_a_member_record_deleted_after_its_read() {
    let mut bed = setup();
    retained_member(&mut bed, "m-cedar.worker");
    tag_cedar_worker();
    let path = session_path_for_key("m-cedar.worker");
    set_record_update_interleave(|| {
        kill_daemon_key("m-cedar.worker");
        assert!(!session_path_for_key("m-cedar.worker").exists());
    });
    let _ = write_pane_session("%9", SID, "/old-pane");
    assert!(
        !path.exists(),
        "pane update recreated a member record deleted after its read"
    );
}

#[test]
fn test_write_pane_session_does_not_overwrite_a_record_replaced_after_its_read() {
    let mut bed = setup();
    retained_member(&mut bed, "m-cedar.worker");
    tag_cedar_worker();
    set_record_update_interleave(|| {
        kill_daemon_key("m-cedar.worker");
        write_session_key(
            "m-cedar.worker",
            "new-session",
            "/new-cwd",
            Some(&binding("cedar", "456", "worker")),
        )
        .unwrap();
        record_cedar("456", Some("new-session"));
        assert!(binding_holds("m-cedar.worker").is_ok());
    });
    let _ = write_pane_session("%9", SID, "/old-pane");
    assert_eq!(
        read_session_key("m-cedar.worker").unwrap(),
        SessionRecord {
            session_id: "new-session".to_string(),
            cwd: "/new-cwd".to_string(),
            binding: Some(binding("cedar", "456", "worker")),
        }
    );
    assert!(binding_holds("m-cedar.worker").is_ok());
}

fn record_cedar_worker_without_session(created_at: &str) {
    let row = json!({"name": "worker", "cli": "grok"})
        .as_object()
        .unwrap()
        .clone();
    crate::registry::record_team("cedar", CWD, created_at, &[row], "").unwrap();
}

#[test]
fn test_write_pane_session_mints_a_bound_record_for_a_member_registered_without_a_session() {
    // `hive fork` registers the member and launches its TUI; the TUI's
    // launch names the session and writes the first record, bound.
    let mut bed = setup();
    bed.env.set("HIVE_HOME", bed.tmp.path().join("home"));
    tag_cedar_worker();
    record_cedar_worker_without_session("123");
    write_pane_session("%9", "sid-fork", "/w").unwrap();
    assert_eq!(
        read_session_key("m-cedar.worker").unwrap(),
        SessionRecord {
            session_id: "sid-fork".to_string(),
            cwd: "/w".to_string(),
            binding: Some(binding("cedar", "123", "worker")),
        }
    );
    // the same launch again is an update, the binding kept
    write_pane_session("%9", "sid-fork", "/w2").unwrap();
    assert_eq!(read_session_key("m-cedar.worker").unwrap().cwd, "/w2");
    assert_eq!(
        read_session_key("m-cedar.worker").unwrap().binding,
        Some(binding("cedar", "123", "worker"))
    );
}

#[test]
fn test_write_pane_session_refuses_a_first_launch_the_roster_does_not_await() {
    let mut bed = setup();
    bed.env.set("HIVE_HOME", bed.tmp.path().join("home"));
    tag_cedar_worker();
    let record = bed.tmp.path().join("hive/m-cedar.worker.session");
    // the row already names another session: not this launch's to mint
    record_cedar("123", Some("sid-other"));
    let err = write_pane_session("%9", "sid-fork", "/w").unwrap_err();
    assert!(err.to_string().contains("already names session"), "{err}");
    assert!(!record.exists());
    // no row at all: a member killed since, or never registered
    record_cedar("123", None);
    let err = write_pane_session("%9", "sid-fork", "/w").unwrap_err();
    assert!(err.to_string().contains("not on the roster"), "{err}");
    assert!(!record.exists());
}

#[test]
fn test_spawn_member_daemon_raises_one_leader_for_two_raisers_at_once() {
    let bed = setup();
    let spawns: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let both_done = Arc::new(std::sync::Barrier::new(2));
    let raisers: Vec<_> = (0..2)
        .map(|_| {
            let spawns = spawns.clone();
            let both_done = both_done.clone();
            thread::spawn(move || {
                // the seams are per thread: each raiser fakes its own spawn
                set_daemon_spawn(move |argv, _env| {
                    *spawns.lock().unwrap() += 1;
                    thread::sleep(Duration::from_millis(150));
                    touch_leader_socket(argv);
                    Ok(Box::new(FakeDaemonChild {
                        pid: 7778,
                        returncode: None,
                        panic_on_terminate: true,
                    }) as Box<dyn DaemonChild>)
                });
                let raised = spawn_member_daemon("honey", "rex");
                // the fake leader's lock lives in this thread: stay until
                // the other raiser has seen it
                both_done.wait();
                raised
            })
        })
        .collect();
    for raiser in raisers {
        assert!(raiser.join().unwrap(), "a raiser reported no leader");
    }
    assert_eq!(*spawns.lock().unwrap(), 1, "both raisers spawned a leader");
    assert!(bed.tmp.path().join("hive/m-honey.rex.raise-lock").exists());
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
fn test_probe_socket_needs_a_listener() {
    let _bed = setup();
    let sock = pane_socket_path("%19");
    fs::create_dir_all(sock.parent().unwrap()).unwrap();
    assert!(!probe_socket(&sock)); // no socket
    fs::write(&sock, "").unwrap();
    assert!(!probe_socket(&sock)); // a plain file, not a socket
    fs::write(pane_pidfile_path("%19"), std::process::id().to_string()).unwrap();
    assert!(!probe_socket(&sock)); // a live pid does not make a listener
    fs::remove_file(&sock).unwrap();
    let listener = bind_leader_socket(&sock);
    assert!(probe_socket(&sock));
    drop(listener);
    // the socket file outlives its leader; the lock it held does not
    assert!(sock.exists());
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

/// What a spawned leader leaves on disk once it is up: the socket file
/// and its held lock.
fn touch_leader_socket(argv: &[String]) {
    let sock = &argv[argv
        .iter()
        .position(|arg| arg == "--leader-socket")
        .unwrap()
        + 1];
    fs::write(sock, "").unwrap();
    let lock = hold_leader_lock(std::path::Path::new(sock));
    TOUCHED_LOCKS.with(|held| held.borrow_mut().push(lock));
}

#[test]
fn test_probe_socket_is_the_leader_lock_not_a_connection() {
    let bed = setup();
    let sock = bed.tmp.path().join("hive/m-honey.rex.sock");
    fs::create_dir_all(sock.parent().unwrap()).unwrap();
    // a bare listener accepting connections is no leader: nothing holds the lock
    let listener = UnixListener::bind(&sock).unwrap();
    assert!(!probe_socket(&sock));
    assert_eq!(leader_holds_lock(&sock).unwrap(), false);
    // a lock file nobody holds is a leader that exited
    fs::write(sock.with_extension("lock"), "4242").unwrap();
    assert!(!probe_socket(&sock));
    // the held lock is the leader, socket on disk
    let lock = hold_leader_lock(&sock);
    assert!(probe_socket(&sock));
    assert_eq!(leader_holds_lock(&sock).unwrap(), true);
    // the lock alone, before the socket is bound, is not yet a leader to reach
    drop(listener);
    fs::remove_file(&sock).unwrap();
    assert!(!probe_socket(&sock));
    assert_eq!(leader_holds_lock(&sock).unwrap(), true);
    drop(lock);
    assert_eq!(leader_holds_lock(&sock).unwrap(), false);
}

#[test]
fn test_spawn_daemon_builds_leader_argv_and_pane_env() {
    let mut bed = setup();
    bed.env.set("TMUX_PANE", "%old");
    let seen: SeenDaemonSpawn = Arc::new(Mutex::new(None));
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
        ],
        "no --no-exit-on-disconnect: the leader exits with its last client"
    );
    assert_eq!(env.get("TMUX_PANE").map(String::as_str), Some("%19"));
    // the pidfile is the socket's sibling, written by the spawn itself
    assert_eq!(
        fs::read_to_string(bed.tmp.path().join("hive/p19.pid")).unwrap(),
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
    let _listener = bind_leader_socket(&sock);
    set_daemon_spawn(|_argv, _env| panic!("must not respawn a live leader"));
    set_terminate_pg(|pid| panic!("terminated {pid} under a live leader"));
    assert!(spawn_daemon("%19"));
}

#[test]
fn test_spawn_daemon_relaunches_when_the_socket_has_no_listener() {
    // pidfile names a live pid, the socket file is there, nothing listens:
    // the pid is not this key's leader, so the files are stale and go
    let _bed = setup();
    let sock = pane_socket_path("%19");
    fs::create_dir_all(sock.parent().unwrap()).unwrap();
    fs::write(&sock, "").unwrap();
    fs::write(pane_pidfile_path("%19"), std::process::id().to_string()).unwrap();
    set_terminate_pg(|pid| panic!("signalled {pid}, which is not a leader"));
    let existed: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
    let existed_spawn = existed.clone();
    let sock_spawn = sock.clone();
    set_daemon_spawn(move |argv, _env| {
        *existed_spawn.lock().unwrap() = Some(sock_spawn.exists());
        touch_leader_socket(argv);
        Ok(Box::new(FakeDaemonChild {
            pid: 7780,
            returncode: None,
            panic_on_terminate: true,
        }))
    });
    assert!(spawn_daemon("%19"));
    assert_eq!(*existed.lock().unwrap(), Some(false));
    assert_eq!(
        fs::read_to_string(pane_pidfile_path("%19")).unwrap(),
        "7780"
    );
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
    set_leader_identity(std::process::id() as libc::pid_t, &sock);
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
fn test_spawn_daemon_refuses_to_reclaim_a_holder_that_is_not_a_leader() {
    // grok's lock file names a pid that is alive but is not a leader (here:
    // this test process, checked through the real `ps`). Signalling it
    // would kill an unrelated process; the stale lock is dropped instead.
    let _bed = setup();
    let sock = pane_socket_path("%19");
    fs::create_dir_all(sock.parent().unwrap()).unwrap();
    fs::write(sock.with_extension("lock"), std::process::id().to_string()).unwrap();
    set_terminate_pg(|pid| panic!("signalled {pid}, which is not a leader"));
    let lock_at_spawn: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
    let lock_probe = lock_at_spawn.clone();
    let lock_path = sock.with_extension("lock");
    set_daemon_spawn(move |argv, _env| {
        *lock_probe.lock().unwrap() = Some(lock_path.exists());
        touch_leader_socket(argv);
        Ok(Box::new(FakeDaemonChild {
            pid: 4244,
            returncode: None,
            panic_on_terminate: true,
        }))
    });
    assert!(spawn_daemon("%19"));
    assert_eq!(*lock_at_spawn.lock().unwrap(), Some(false));
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
fn test_kill_daemon_key_refuses_a_pid_that_is_not_a_leader() {
    // the pidfile names this test process — alive, but no grok leader as
    // the real `ps` reports it: never signalled, and the stale files go
    let _bed = setup();
    let sock = socket_path_for_key("m-honey.rex");
    fs::create_dir_all(sock.parent().unwrap()).unwrap();
    fs::write(&sock, "").unwrap();
    fs::write(sock.with_extension("pid"), std::process::id().to_string()).unwrap();
    fs::write(sock.with_extension("lock"), std::process::id().to_string()).unwrap();
    set_terminate_pg(|pid| panic!("signalled {pid}, which is not a leader"));
    kill_daemon_key("m-honey.rex");
    assert!(!sock.exists());
    assert!(!sock.with_extension("pid").exists());
    assert!(!sock.with_extension("lock").exists());
}

#[test]
fn test_kill_daemon_key_refuses_a_leader_of_another_key() {
    // a recycled pid now runs the leader of a different socket: still not
    // ours to signal
    let _bed = setup();
    let sock = socket_path_for_key("m-honey.rex");
    fs::create_dir_all(sock.parent().unwrap()).unwrap();
    fs::write(&sock, "").unwrap();
    fs::write(sock.with_extension("pid"), "4321").unwrap();
    set_leader_identity(4321, &socket_path_for_key("m-honey.ada"));
    set_terminate_pg(|pid| panic!("signalled {pid}, another key's leader"));
    kill_daemon_key("m-honey.rex");
    assert!(!sock.with_extension("pid").exists());
}

#[test]
fn test_kill_daemon_key_kills_every_client_of_the_socket_before_the_leader() {
    // A client outliving its leader raises a replacement on the same socket,
    // so the clients go first — all of them, whoever started them, and only
    // the ones naming THIS socket.
    let _bed = setup();
    let sock = socket_path_for_key("m-honey.rex");
    let foreign = socket_path_for_key("m-honey.ada");
    fs::create_dir_all(sock.parent().unwrap()).unwrap();
    fs::write(&sock, "").unwrap();
    fs::write(sock.with_extension("pid"), "4321").unwrap();
    fs::write(sock.with_extension("session"), "{}").unwrap();
    let (_procs, killed) = set_process_table(vec![
        (11, tui_args(&sock)),
        (12, stdio_args(&sock)),
        (13, stdio_args(&foreign)),
        (4321, leader_args(&sock)),
    ]);

    kill_daemon_key("m-honey.rex");

    assert_eq!(*killed.lock().unwrap(), vec![11, 12, 4321]);
    assert!(!sock.exists());
    assert!(!sock.with_extension("pid").exists());
    assert!(!sock.with_extension("session").exists());
}

#[test]
fn test_kill_daemon_key_reaps_a_leader_a_dying_client_raised() {
    // grok raises the replacement itself, so its argv names no socket at
    // all — only the lock file it writes ties it to the key. The second
    // pass is what catches it; the key files go after both.
    let _bed = setup();
    let sock = socket_path_for_key("m-honey.rex");
    fs::create_dir_all(sock.parent().unwrap()).unwrap();
    fs::write(&sock, "").unwrap();
    let (procs, killed) = set_process_table(vec![(11, stdio_args(&sock))]);
    let respawn_lock = sock.with_extension("lock");
    let reaped = procs.clone();
    let log = killed.clone();
    set_terminate_pg(move |pid| {
        log.lock().unwrap().push(pid);
        reaped.lock().unwrap().retain(|(seen, _args)| *seen != pid);
        if pid == 11 {
            fs::write(&respawn_lock, "22").unwrap();
            reaped
                .lock()
                .unwrap()
                .push((22, "grok agent leader".to_string()));
        }
    });

    kill_daemon_key("m-honey.rex");

    assert_eq!(*killed.lock().unwrap(), vec![11, 22]);
    assert!(!sock.exists());
    assert!(!sock.with_extension("lock").exists());
}

#[test]
fn test_kill_daemon_key_trusts_a_bare_leader_only_through_the_lock_file() {
    // hive's own `.pid` is never cleared by a crash: a recycled pid running
    // some other key's grok-raised leader (bare argv) must not pass through
    // it, while grok's `.lock` — written by this socket's flock holder —
    // vouches for the same bare shape.
    let _bed = setup();
    let sock = socket_path_for_key("m-honey.rex");
    fs::create_dir_all(sock.parent().unwrap()).unwrap();
    fs::write(&sock, "").unwrap();
    fs::write(sock.with_extension("pid"), "4321").unwrap();
    let (_procs, killed) = set_process_table(vec![(4321, "grok agent leader".to_string())]);

    kill_daemon_key("m-honey.rex");
    assert!(
        killed.lock().unwrap().is_empty(),
        "pidfile vouched for a bare leader"
    );
    assert!(!sock.with_extension("pid").exists());

    fs::write(&sock, "").unwrap();
    fs::write(sock.with_extension("lock"), "4321").unwrap();
    kill_daemon_key("m-honey.rex");
    assert_eq!(*killed.lock().unwrap(), vec![4321]);
    assert!(!sock.with_extension("lock").exists());
}

#[test]
fn test_kill_daemon_key_ignores_a_dead_pid() {
    let _bed = setup();
    let sock = socket_path_for_key("m-honey.rex");
    fs::create_dir_all(sock.parent().unwrap()).unwrap();
    fs::write(sock.with_extension("pid"), "999999").unwrap();
    set_terminate_pg(|pid| panic!("signalled dead pid {pid}"));
    kill_daemon_key("m-honey.rex");
    assert!(!sock.with_extension("pid").exists());
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
fn test_pool_send_to_key_returns_prompt_queued() {
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
fn test_pool_send_to_key_none_without_client() {
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
fn test_pool_send_to_key_none_when_client_raises() {
    let grok_pool = GrokClientPool::new();
    *grok_pool.client_override.lock().unwrap() =
        Some(Box::new(|_key| Some(Arc::new(FakeRaisingPromptClient))));
    assert_eq!(grok_pool.send_to_key("p19", "hi"), None);
}

struct FakeTrackedClient {
    sent: Arc<Mutex<Vec<String>>>,
    asked: Arc<Mutex<Vec<u64>>>,
}

impl LeaderClient for FakeTrackedClient {
    fn generation(&self) -> u64 {
        1
    }
    fn prompt_tracked(&self, text: &str) -> Result<u64> {
        self.sent.lock().unwrap().push(text.to_string());
        Ok(7)
    }

    fn prompt_result(&self, rid: u64) -> Option<PromptResult> {
        self.asked.lock().unwrap().push(rid);
        (rid == 7).then(|| ended("end_turn", "relayed"))
    }
}

type TrackedPool = (
    GrokClientPool,
    Arc<Mutex<Vec<String>>>,
    Arc<Mutex<Vec<u64>>>,
);

fn tracked_pool() -> TrackedPool {
    let grok_pool = GrokClientPool::new();
    let sent: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let asked: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let (sent_client, asked_client) = (sent.clone(), asked.clone());
    *grok_pool.client_override.lock().unwrap() = Some(Box::new(move |_key| {
        Some(Arc::new(FakeTrackedClient {
            sent: sent_client.clone(),
            asked: asked_client.clone(),
        }))
    }));
    (grok_pool, sent, asked)
}

#[test]
fn test_pool_dispatch_confirmed_returns_the_rid() {
    let (grok_pool, sent, _asked) = tracked_pool();
    assert_eq!(
        grok_pool.dispatch_confirmed(&override_confirmation("p19"), "task"),
        Ok(PromptId {
            generation: 1,
            rid: 7
        })
    );
    assert_eq!(*sent.lock().unwrap(), vec!["task"]);
}

#[test]
fn test_pool_prompt_result_for_key_relays_the_client() {
    let (grok_pool, _sent, asked) = tracked_pool();
    assert_eq!(
        grok_pool.prompt_result_for_key(
            "p19",
            PromptId {
                generation: 1,
                rid: 7
            }
        ),
        Some(ended("end_turn", "relayed"))
    );
    assert_eq!(
        grok_pool.prompt_result_for_key(
            "p19",
            PromptId {
                generation: 1,
                rid: 8
            }
        ),
        None
    );
    assert_eq!(*asked.lock().unwrap(), vec![7, 8]);
}

#[test]
fn test_pool_dispatch_confirmed_err_without_client() {
    let grok_pool = GrokClientPool::new();
    *grok_pool.client_override.lock().unwrap() = Some(Box::new(|_key| None));
    let err = grok_pool
        .dispatch_confirmed(&override_confirmation("p19"), "task")
        .unwrap_err();
    assert!(err.contains("p19"), "{err}");
    assert_eq!(
        grok_pool.prompt_result_for_key(
            "p19",
            PromptId {
                generation: 1,
                rid: 7
            }
        ),
        None
    );
}

struct FakeRaisingTrackedClient;

impl LeaderClient for FakeRaisingTrackedClient {
    fn prompt_tracked(&self, _text: &str) -> Result<u64> {
        Err(anyhow::anyhow!("broken pipe"))
    }
}

#[test]
fn test_pool_dispatch_confirmed_err_when_client_raises() {
    let grok_pool = GrokClientPool::new();
    *grok_pool.client_override.lock().unwrap() =
        Some(Box::new(|_key| Some(Arc::new(FakeRaisingTrackedClient))));
    assert_eq!(
        grok_pool.dispatch_confirmed(&override_confirmation("p19"), "task"),
        Err("broken pipe".to_string())
    );
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
fn test_pool_interrupt_key_returns_cancel_sent() {
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
fn test_pool_interrupt_key_none_without_client() {
    let grok_pool = GrokClientPool::new();
    *grok_pool.client_override.lock().unwrap() = Some(Box::new(|_key| None));
    assert_eq!(grok_pool.interrupt_key("p19"), None);
}

#[test]
fn test_pool_interrupt_key_none_when_the_write_fails() {
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
fn test_pool_interrupt_key_none_when_client_raises() {
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
fn test_pool_compact_key_unavailable_without_client() {
    let grok_pool = GrokClientPool::new();
    *grok_pool.client_override.lock().unwrap() = Some(Box::new(|_key| None));
    assert_eq!(grok_pool.compact_key("p19"), "unavailable");
}

#[test]
fn test_pool_runtime_for_key_none_without_client() {
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
    assert!(grok_pool.client_for_key("p19").is_none()); // no socket at all
    let sock = pane_socket_path("%19");
    fs::create_dir_all(sock.parent().unwrap()).unwrap();
    fs::write(&sock, "").unwrap();
    grok_pool.state.lock().unwrap().cooldown.clear();
    // socket but no session record
    assert!(grok_pool.client_for_key("p19").is_none());
}

#[test]
fn test_pool_skips_a_pane_whose_socket_has_no_listener() {
    // a socket file outlives the leader that bound it, and the pidfile still
    // names a live pid: nothing listens, so no client
    let _bed = setup();
    write_pane_session("%19", SID, CWD).unwrap();
    fs::write(pane_socket_path("%19"), "").unwrap();
    fs::write(pane_pidfile_path("%19"), std::process::id().to_string()).unwrap();
    set_stdio_spawn(|_argv| panic!("no client without a live leader"));
    assert!(GrokClientPool::new().client_for_key("p19").is_none());
}

#[test]
fn test_pool_refuses_a_member_key_the_roster_no_longer_lists() {
    // The hived's pool outlives every kill: a client bound after the member
    // is gone raises a fresh leader on the dead member's socket.
    let mut bed = setup();
    bed.env.set("HIVE_HOME", bed.tmp.path().join(".hive"));
    let key = member_key("honey", "rex");
    write_session_key(&key, SID, CWD, None).unwrap();
    let _listener = bind_leader_socket(&socket_path_for_key(&key));
    let spawned: Arc<Mutex<Vec<Arc<FakeProc>>>> = Arc::new(Mutex::new(Vec::new()));
    let spawn_log = spawned.clone();
    set_stdio_spawn(move |_argv| {
        let proc = FakeProc::new(Some(responder(None, vec![])));
        spawn_log.lock().unwrap().push(proc.clone());
        Ok(proc as Arc<dyn LeaderProc>)
    });

    // team file missing entirely: the member is gone with its team
    let grok_pool = GrokClientPool::new();
    assert!(grok_pool.client_for_key(&key).is_none());
    assert!(spawned.lock().unwrap().is_empty());

    // team back, but the roster does not list rex
    let mut other = serde_json::Map::new();
    other.insert("name".to_string(), Value::String("ada".to_string()));
    crate::registry::record_team("honey", CWD, "1.0", &[other], "").unwrap();
    grok_pool.state.lock().unwrap().cooldown.clear();
    assert!(grok_pool.client_for_key(&key).is_none());
    assert!(spawned.lock().unwrap().is_empty());

    // listed again: the client binds
    let mut rex = serde_json::Map::new();
    rex.insert("name".to_string(), Value::String("rex".to_string()));
    crate::registry::record_team("honey", CWD, "1.0", &[rex], "").unwrap();
    grok_pool.state.lock().unwrap().cooldown.clear();
    let client = grok_pool.client_for_key(&key).unwrap();
    assert_eq!(spawned.lock().unwrap().len(), 1);

    grok_pool.drop_key(&key);
    for proc in spawned.lock().unwrap().iter() {
        proc.eof();
    }
    if let Some(handle) = client.reader.lock().unwrap().take() {
        let _ = handle.join();
    }
    drop(client);
}

#[test]
fn test_pool_rebinds_when_the_pane_session_record_rotates() {
    // grok relaunched in the same pane mints a new session id; the client bound
    // to the old one would report a stale session forever
    let _bed = setup();
    write_pane_session("%19", SID, CWD).unwrap();
    let _listener = bind_leader_socket(&pane_socket_path("%19"));
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
        let client = grok_pool.client_for_key("p19");
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

    grok_pool.drop_pane("%19");
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
    let mut bed = setup();
    bed.env
        .set("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/cc-socks/999.sock");
    bed.env.set("CLAUDE_CONFIG_DIR", "/tmp/elsewhere");
    bed.env.set("CODEX_THREAD_ID", "tid-1");
    bed.env.set("GROK_SESSION_ID", "spawner-session");
    bed.env.set("TMUX_PANE", "%stale");

    let env_map = daemon_env_for_pane("%42");

    assert_eq!(env_map.get("TMUX_PANE").map(String::as_str), Some("%42"));
    assert!(!env_map.contains_key("CLAUDE_CODE_MESSAGING_SOCKET"));
    assert!(!env_map.contains_key("CLAUDE_CONFIG_DIR"));
    assert!(!env_map.contains_key("CODEX_THREAD_ID"));
    assert!(!env_map.contains_key("GROK_SESSION_ID"));
    // identity never rides the env: the pane is the only key pinned, and
    // the leader mints its own session id for the tools it runs
    let inherited: HashSet<String> = env::vars()
        .map(|(key, _)| key)
        .filter(|key| key != "TMUX_PANE")
        .collect();
    let pinned: Vec<&String> = env_map
        .keys()
        .filter(|key| !inherited.contains(*key))
        .collect();
    assert_eq!(pinned, vec!["TMUX_PANE"]);
}

#[test]
fn test_spawn_daemon_member_pane_gets_the_member_socket() {
    // A tagged member pane spawns a member-keyed daemon: the member identity
    // lives in the socket key, never in the env handed to the leader.
    let mut bed = setup();
    bed.env.set("GROK_SESSION_ID", "spawner-session");
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
    let seen: SeenDaemonSpawn = Arc::new(Mutex::new(None));
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
    assert!(!env_map.contains_key("GROK_SESSION_ID"));
    assert!(!env_map
        .values()
        .any(|value| value == "honey" || value == "rex"));
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
}

#[test]
fn test_spawn_member_daemon_env_carries_no_inherited_identity() {
    // The identity lane: no pane, and nothing of the spawner's identity —
    // its GROK_SESSION_ID would make every hive call in this member sign as
    // the spawner.
    let mut bed = setup();
    bed.env.set("GROK_SESSION_ID", "spawner-session");
    bed.env.set("CODEX_THREAD_ID", "tid-1");
    bed.env.set("TMUX_PANE", "%stale");
    bed.env.set("TMUX", "/tmp/tmux-0/default,4242,0");
    let seen: SeenDaemonSpawn = Arc::new(Mutex::new(None));
    let seen_spawn = seen.clone();
    set_daemon_spawn(move |argv, env| {
        *seen_spawn.lock().unwrap() = Some((argv.to_vec(), env.clone()));
        touch_leader_socket(argv);
        Ok(Box::new(FakeDaemonChild {
            pid: 7778,
            returncode: None,
            panic_on_terminate: false,
        }))
    });

    assert!(spawn_member_daemon("honey", "rex"));

    let seen = seen.lock().unwrap();
    let (argv, env_map) = seen.as_ref().unwrap();
    let sock = &argv[argv
        .iter()
        .position(|arg| arg == "--leader-socket")
        .unwrap()
        + 1];
    assert_eq!(
        *sock,
        socket_path_for_key("m-honey.rex")
            .to_string_lossy()
            .into_owned()
    );
    assert!(!env_map.contains_key("GROK_SESSION_ID"));
    assert!(!env_map.contains_key("CODEX_THREAD_ID"));
    assert!(!env_map.contains_key("TMUX_PANE"));
    // no TMUX either: identity::is_inside_tmux takes any non-empty TMUX as a client
    assert!(!env_map.contains_key("TMUX"));
    // the member's name is in the socket key, never in the leader's env
    assert!(!env_map
        .values()
        .any(|value| value == "honey" || value == "rex"));
    let inherited: HashSet<String> = env::vars().map(|(key, _)| key).collect();
    assert!(
        env_map.keys().all(|key| inherited.contains(key)),
        "{env_map:?}"
    );
    assert_eq!(
        fs::read_to_string(bed.tmp.path().join("hive").join("m-honey.rex.pid")).unwrap(),
        "7778"
    );
}

/// A fake leader answering the mint: initialize, then session/new echoing
/// the id hive put in `_meta`.
fn minting_responder() -> Responder {
    Box::new(
        |msg: &Value| match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => vec![ok(msg, json!({"protocolVersion": 1}))],
            Some("session/new") => vec![ok(
                msg,
                json!({"sessionId": msg["params"]["_meta"]["sessionId"]}),
            )],
            _ => Vec::new(),
        },
    )
}

/// One fake member daemon spawn (pid 7778) that binds a live listener on
/// the socket it is asked for, so later probes find it; returns the
/// spawn count.
fn set_listening_daemon_spawn() -> Arc<Mutex<usize>> {
    let spawns: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let count = spawns.clone();
    let listeners: Arc<Mutex<Vec<FakeLeader>>> = Arc::new(Mutex::new(Vec::new()));
    set_daemon_spawn(move |argv, _env| {
        *count.lock().unwrap() += 1;
        let sock = &argv[argv
            .iter()
            .position(|arg| arg == "--leader-socket")
            .unwrap()
            + 1];
        listeners
            .lock()
            .unwrap()
            .push(bind_leader_socket(std::path::Path::new(sock)));
        Ok(Box::new(FakeDaemonChild {
            pid: 7778,
            returncode: None,
            panic_on_terminate: false,
        }))
    });
    spawns
}

#[test]
fn test_a_members_session_is_minted_and_reloaded_always_approve_a_humans_is_not() {
    let _bed = setup();
    // a member key: the mint and the reload both carry `_meta.yoloMode`
    let proc = FakeProc::new(Some(minting_responder()));
    let handout = proc.clone();
    set_stdio_spawn(move |_argv| Ok(handout.clone() as Arc<dyn LeaderProc>));
    let client = Arc::new(GrokStdioClient::new("m-honey.sage").unwrap());
    assert!(client.new_session(SID, CWD));
    let minted = settle_sent(&proc, |msg| msg["method"] == "session/new");
    assert_eq!(minted["params"]["_meta"]["yoloMode"], json!(true));
    assert_eq!(minted["params"]["_meta"]["sessionId"], json!(SID));
    teardown(&client, &proc);
    write_session_key("m-honey.sage", SID, CWD, None).unwrap();
    let (client, proc) = {
        let proc = FakeProc::new(Some(responder(None, Vec::new())));
        let handout = proc.clone();
        set_stdio_spawn(move |_argv| Ok(handout.clone() as Arc<dyn LeaderProc>));
        (
            Arc::new(GrokStdioClient::new("m-honey.sage").unwrap()),
            proc,
        )
    };
    assert!(client.handshake());
    let reloaded = settle_sent(&proc, |msg| msg["method"] == "session/load");
    assert_eq!(reloaded["params"]["_meta"]["yoloMode"], json!(true));
    teardown(&client, &proc);

    // a human's own pane: grok's prompts stay
    let (client, proc) = loaded(None, vec![]);
    let request = settle_sent(&proc, |msg| msg["method"] == "session/load");
    assert!(request["params"].get("_meta").is_none(), "{request}");
    teardown(&client, &proc);
}

#[test]
fn test_new_session_accepts_a_reply_after_the_load_budget_without_retrying() {
    let _bed = setup();
    let (client, proc) = make(
        Some(Box::new(|msg| match msg["method"].as_str() {
            Some("initialize") => vec![ok(msg, json!({"protocolVersion": 1}))],
            _ => Vec::new(),
        })),
        None,
        "%19",
    );
    let delayed = proc.clone();
    let reply = thread::spawn(move || {
        let request = settle_sent(&delayed, |msg| msg["method"] == "session/new");
        thread::sleep(Duration::from_secs_f64(LOAD_TIMEOUT + 1.0));
        delayed.feed(&ok(&request, json!({"sessionId": SID})));
    });

    let created = client.new_session(SID, CWD);
    reply.join().unwrap();
    teardown(&client, &proc);

    assert!(created);
    assert_eq!(client.session_id().as_deref(), Some(SID));
    assert_eq!(
        proc.sent()
            .iter()
            .filter(|msg| msg["method"] == "session/new")
            .count(),
        1
    );
}

#[test]
fn test_create_member_session_mints_on_the_identity_key_before_any_pane() {
    let _bed = setup();
    let _spawns = set_listening_daemon_spawn();
    let proc = FakeProc::new(Some(minting_responder()));
    let handout = proc.clone();
    let stdio_argvs: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let argv_log = stdio_argvs.clone();
    set_stdio_spawn(move |argv| {
        argv_log.lock().unwrap().push(argv.to_vec());
        Ok(handout.clone() as Arc<dyn LeaderProc>)
    });

    assert!(create_member_session("honey", "123", "rex", SID, CWD));

    // the stdio client went to the identity socket — no pane was consulted
    let key = member_key("honey", "rex");
    assert_eq!(
        stdio_argvs.lock().unwrap().as_slice(),
        &[vec![
            "grok".to_string(),
            "agent".to_string(),
            "--leader".to_string(),
            "stdio".to_string(),
            "--leader-socket".to_string(),
            socket_path_for_key(&key).to_string_lossy().into_owned(),
        ]]
    );
    // initialize, then session/new carrying hive's id in _meta and the cwd
    let sent = proc.sent();
    let methods: Vec<&str> = sent
        .iter()
        .filter_map(|msg| msg.get("method").and_then(Value::as_str))
        .collect();
    assert_eq!(methods, vec!["initialize", "session/new"]);
    assert_eq!(sent[1]["params"]["cwd"], json!(CWD));
    assert_eq!(sent[1]["params"]["_meta"]["sessionId"], json!(SID));
    // the record lives on the identity key, bound to the team instance
    // and member it was minted for
    assert_eq!(
        read_session_key(&key),
        Some(SessionRecord {
            session_id: SID.to_string(),
            cwd: CWD.to_string(),
            binding: Some(binding("honey", "123", "rex")),
        })
    );
    // and the creating client is the pool's, already bound to the session
    let pooled = pool().client_for_key(&key).unwrap();
    assert_eq!(pooled.session_id().as_deref(), Some(SID));
    assert_eq!(stdio_argvs.lock().unwrap().len(), 1); // adopted, not re-spawned

    pool().drop_key(&key);
    proc.eof();
    let handle = pooled.reader.lock().unwrap().take();
    if let Some(handle) = handle {
        let _ = handle.join();
    }
    drop(pooled);
}

#[test]
fn test_create_member_session_leaves_no_record_when_session_new_fails() {
    let _bed = setup();
    let _spawns = set_listening_daemon_spawn();
    let proc = FakeProc::new(Some(Box::new(|msg: &Value| {
        match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => vec![ok(msg, json!({"protocolVersion": 1}))],
            Some("session/new") => vec![json!({
                "jsonrpc": "2.0",
                "id": msg["id"],
                "error": {"code": -32000, "message": "cwd not allowed"},
            })],
            _ => Vec::new(),
        }
    })));
    let handout = proc.clone();
    set_stdio_spawn(move |_argv| Ok(handout.clone() as Arc<dyn LeaderProc>));

    let key = member_key("honey", "rex");
    let sock = socket_path_for_key(&key);
    set_leader_identity(7778, &sock);
    let killed: KillLog = Arc::new(Mutex::new(Vec::new()));
    let record = killed.clone();
    set_terminate_pg(move |pid| record.lock().unwrap().push(pid));

    assert!(!create_member_session("honey", "123", "rex", SID, CWD));

    assert_eq!(read_session_key(&key), None);
    assert!(proc.terminated.load(Ordering::SeqCst)); // the failed client is closed
    assert!(pool().client_for_key(&key).is_none());
    // the leader this mint raised goes with it: nothing is left for the
    // hived's orphan reap to find
    assert_eq!(*killed.lock().unwrap(), vec![7778]);
    assert!(!sock.exists());
    assert!(!sock.with_extension("pid").exists());
}

#[test]
fn test_create_member_session_leaves_a_reused_leader_alone_when_session_new_fails() {
    // The leader was already listening before this mint: not ours to
    // kill on the way out — only the record and the failed client go.
    let _bed = setup();
    let key = member_key("honey", "rex");
    let sock = socket_path_for_key(&key);
    let _listener = bind_leader_socket(&sock);
    fs::write(sock.with_extension("pid"), "4321").unwrap();
    set_leader_identity(4321, &sock);
    set_daemon_spawn(|_argv, _env| panic!("a listening leader is reused, never respawned"));
    set_terminate_pg(|pid| panic!("signalled {pid}, a leader this mint did not raise"));
    let proc = FakeProc::new(Some(Box::new(|msg: &Value| {
        match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => vec![ok(msg, json!({"protocolVersion": 1}))],
            Some("session/new") => vec![json!({
                "jsonrpc": "2.0",
                "id": msg["id"],
                "error": {"code": -32000, "message": "cwd not allowed"},
            })],
            _ => Vec::new(),
        }
    })));
    let handout = proc.clone();
    set_stdio_spawn(move |_argv| Ok(handout.clone() as Arc<dyn LeaderProc>));

    assert!(!create_member_session("honey", "123", "rex", SID, CWD));

    assert_eq!(read_session_key(&key), None);
    assert!(proc.terminated.load(Ordering::SeqCst));
    assert!(pool().client_for_key(&key).is_none());
    assert!(sock.exists());
    assert_eq!(
        fs::read_to_string(sock.with_extension("pid")).unwrap(),
        "4321"
    );
}

#[test]
fn test_create_member_session_fails_without_a_leader() {
    let _bed = setup();
    set_daemon_spawn(|_argv, _env| Err(io::Error::new(io::ErrorKind::NotFound, "no grok")));
    let spawned = Arc::new(AtomicBool::new(false));
    let flag = spawned.clone();
    set_stdio_spawn(move |_argv| {
        flag.store(true, Ordering::SeqCst);
        Err(io::Error::other("unreachable"))
    });
    assert!(!create_member_session("honey", "123", "rex", SID, CWD));
    assert!(!spawned.load(Ordering::SeqCst)); // no client without a daemon
    assert_eq!(read_session_key(&member_key("honey", "rex")), None);
}

#[test]
fn test_member_pane_is_a_client_of_the_identity_minted_engine() {
    // The engine is minted by identity; a pane tagged as the member later
    // reaches the very same daemon key, socket and session record — the
    // pane's `spawn_daemon` finds the leader listening and raises nothing.
    let _bed = setup();
    let spawns = set_listening_daemon_spawn();
    let proc = FakeProc::new(Some(minting_responder()));
    let handout = proc.clone();
    set_stdio_spawn(move |_argv| Ok(handout.clone() as Arc<dyn LeaderProc>));
    assert!(create_member_session("honey", "123", "rex", SID, CWD));
    assert_eq!(*spawns.lock().unwrap(), 1);

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

    let key = member_key("honey", "rex");
    assert_eq!(resolve_pane_key("%19"), key);
    assert_eq!(pane_socket_path("%19"), socket_path_for_key(&key));
    assert_eq!(
        read_pane_session("%19"),
        Some(SessionRecord {
            session_id: SID.to_string(),
            cwd: CWD.to_string(),
            binding: Some(binding("honey", "123", "rex")),
        })
    );
    assert!(spawn_daemon("%19"));
    assert_eq!(*spawns.lock().unwrap(), 1); // reused, not a second leader

    let pooled = pool().client_for_key(&key).unwrap();
    pool().drop_key(&key);
    proc.eof();
    let handle = pooled.reader.lock().unwrap().take();
    if let Some(handle) = handle {
        let _ = handle.join();
    }
    drop(pooled);
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
    set_leader_identity(4321, &sock);
    let killed: Arc<Mutex<Vec<libc::pid_t>>> = Arc::new(Mutex::new(Vec::new()));
    let killed_record = killed.clone();
    set_terminate_pg(move |pid| killed_record.lock().unwrap().push(pid));

    kill_daemon_key("m-honey.rex");

    assert_eq!(*killed.lock().unwrap(), vec![4321]);
    assert!(!sock.exists());
    assert!(!sock.with_extension("pid").exists());
    assert!(!sock.with_extension("session").exists());
}

#[test]
fn test_pool_old_prompt_id_cannot_read_replacement_clients_result() {
    let _bed = setup();
    let pool = GrokClientPool::new();
    let (first, first_proc) = loaded(None, vec![]);
    let handout = first.clone();
    *pool.client_override.lock().unwrap() = Some(Box::new(move |_key| Some(handout.clone())));
    let old_id = pool
        .dispatch_confirmed(&override_confirmation("p19"), "old task")
        .unwrap();
    first_proc.feed(&agent_chunk(Some("old-prompt"), "OLD RESULT"));
    first_proc.feed(&prompt_response(old_id.rid, "old-prompt", "end_turn"));
    assert_eq!(
        settle_ended(&first, old_id.rid),
        ended("end_turn", "OLD RESULT")
    );
    assert_eq!(
        pool.prompt_result_for_key("p19", old_id),
        Some(ended("end_turn", "OLD RESULT"))
    );
    teardown(&first, &first_proc);

    let (replacement, replacement_proc) = loaded(None, vec![]);
    let handout = replacement.clone();
    *pool.client_override.lock().unwrap() = Some(Box::new(move |_key| Some(handout.clone())));
    let new_id = pool
        .dispatch_confirmed(&override_confirmation("p19"), "new task")
        .unwrap();
    assert_eq!(
        old_id.rid, new_id.rid,
        "client counters restart at handshake"
    );
    assert_ne!(old_id.generation, new_id.generation);
    replacement_proc.feed(&agent_chunk(Some("new-prompt"), "NEW RESULT"));
    replacement_proc.feed(&prompt_response(new_id.rid, "new-prompt", "end_turn"));
    assert_eq!(
        settle_ended(&replacement, new_id.rid),
        ended("end_turn", "NEW RESULT")
    );
    assert_eq!(pool.prompt_result_for_key("p19", old_id), None);
    assert_eq!(
        pool.prompt_result_for_key("p19", new_id),
        Some(ended("end_turn", "NEW RESULT"))
    );
    teardown(&replacement, &replacement_proc);
}

// ----------------------------------------------------------------------
// launch leaders and their member alias (handoff.rs)
// ----------------------------------------------------------------------

#[test]
fn test_launch_keys_parse_and_list_beside_pane_and_member_keys() {
    assert!(is_launch_key("l-ab12"));
    assert!(!is_launch_key("l-"));
    assert!(!is_launch_key("l-a.b"));
    assert!(!is_launch_key("p19"));
    assert_eq!(
        key_from_socket_name("l-ab12.sock").as_deref(),
        Some("l-ab12")
    );
    assert_eq!(key_from_socket_name("l-.sock"), None);
    assert_eq!(
        key_from_alias_name("m-honey.orch.alias").as_deref(),
        Some("m-honey.orch")
    );
    assert_eq!(key_from_alias_name("l-ab12.alias"), None);
    assert!(is_launch_key(&mint_launch_key()));
    assert_ne!(mint_launch_key(), mint_launch_key());

    let bed = setup();
    let hive_dir = bed.tmp.path().join("hive");
    fs::create_dir_all(&hive_dir).unwrap();
    fs::write(hive_dir.join("l-ab12.sock"), "").unwrap();
    fs::write(hive_dir.join("m-honey.orch.alias"), "l-ab12").unwrap();
    fs::write(hive_dir.join("p7.sock"), "").unwrap();
    let mut keys = list_daemon_keys();
    keys.sort();
    // the bound member is listed under its own key: kill, delete and the
    // hived's reap reach the launch leader through the alias
    assert_eq!(keys, vec!["l-ab12", "m-honey.orch", "p7"]);
}

#[test]
fn test_a_member_alias_redirects_every_path_lookup_to_the_launch_key() {
    let bed = setup();
    let hive_dir = bed.tmp.path().join("hive");
    fs::create_dir_all(&hive_dir).unwrap();
    assert_eq!(canonical_key("m-honey.orch"), "m-honey.orch");
    assert_eq!(
        socket_path_for_key("m-honey.orch"),
        hive_dir.join("m-honey.orch.sock")
    );
    fs::write(hive_dir.join("m-honey.orch.alias"), "l-ab12\n").unwrap();
    assert_eq!(canonical_key("m-honey.orch"), "l-ab12");
    assert_eq!(
        socket_path_for_key("m-honey.orch"),
        hive_dir.join("l-ab12.sock")
    );
    assert_eq!(
        session_path_for_key("m-honey.orch"),
        hive_dir.join("l-ab12.session")
    );
    write_session_key("l-ab12", "sid-1", "/w", None).unwrap();
    assert_eq!(
        read_session_key("m-honey.orch").map(|r| r.session_id),
        Some("sid-1".to_string())
    );
    // a launch key and a pane key never follow an alias
    assert_eq!(canonical_key("l-ab12"), "l-ab12");
    assert_eq!(canonical_key("p7"), "p7");
    // an alias naming something that is not a launch key is ignored
    fs::write(hive_dir.join("m-honey.rex.alias"), "p7").unwrap();
    assert_eq!(canonical_key("m-honey.rex"), "m-honey.rex");
}

#[test]
fn test_bind_launch_needs_a_listening_leader_serving_that_session() {
    let bed = setup();
    let sock = bed.tmp.path().join("hive").join("l-ab12.sock");
    let err = bind_launch("l-ab12", "sid-1", "/w", "honey", "1", "orch", "%3").unwrap_err();
    assert!(err.to_string().contains("not listening"), "{err}");
    let _listener = bind_leader_socket(&sock);
    write_session_key("l-ab12", "sid-other", "/w", None).unwrap();
    let err = bind_launch("l-ab12", "sid-1", "/w", "honey", "1", "orch", "%3").unwrap_err();
    assert!(err.to_string().contains("sid-other"), "{err}");
    assert!(alias_target("m-honey.orch").is_none());
    assert!(bind_launch("p7", "sid-1", "/w", "honey", "1", "orch", "%3").is_err());
    assert!(bind_launch("l-ab12", "", "/w", "honey", "1", "orch", "%3").is_err());
}

#[test]
fn test_bind_launch_writes_the_alias_once_and_refuses_a_second_engine() {
    let bed = setup();
    let hive_dir = bed.tmp.path().join("hive");
    let _listener = bind_leader_socket(&hive_dir.join("l-ab12.sock"));
    // no record yet (a launcher restart): written from the arguments
    bind_launch("l-ab12", "sid-1", "/w", "honey", "1", "orch", "%3").unwrap();
    assert_eq!(alias_target("m-honey.orch").as_deref(), Some("l-ab12"));
    assert_eq!(
        read_session_key("l-ab12"),
        Some(SessionRecord {
            session_id: "sid-1".to_string(),
            cwd: "/w".to_string(),
            binding: Some(binding("honey", "1", "orch")),
        })
    );
    assert!(launch_is_bound(
        "l-ab12", "sid-1", "honey", "1", "orch", "%3"
    ));
    assert!(!launch_is_bound(
        "l-ab12", "sid-2", "honey", "1", "orch", "%3"
    ));
    assert_eq!(bound_member("l-ab12").as_deref(), Some("m-honey.orch"));
    assert_eq!(bound_member("l-zz99"), None);
    // idempotent from either side
    bind_launch("l-ab12", "sid-1", "/w", "honey", "1", "orch", "%3").unwrap();
    // the member is taken: another launch is refused
    let _other = bind_leader_socket(&hive_dir.join("l-cd34.sock"));
    let err = bind_launch("l-cd34", "sid-9", "/w", "honey", "1", "orch", "%3").unwrap_err();
    assert!(err.to_string().contains("already bound"), "{err}");
    assert_eq!(alias_target("m-honey.orch").as_deref(), Some("l-ab12"));
    // a member with a leader of its own is never aliased over
    let _own = bind_leader_socket(&hive_dir.join("m-honey.rex.sock"));
    let err = bind_launch("l-cd34", "sid-9", "/w", "honey", "1", "rex", "%4").unwrap_err();
    assert!(err.to_string().contains("leader of its own"), "{err}");
}

#[test]
fn test_bind_launch_binds_the_record_before_the_alias_and_a_refused_alias_puts_it_back() {
    let bed = setup();
    let hive_dir = bed.tmp.path().join("hive");
    let _listener = bind_leader_socket(&hive_dir.join("l-ab12.sock"));
    // the launcher's own record, unbound (hgrok at a terminal), with a
    // field hive does not know
    write_session_key("l-ab12", "sid-1", "/w", None).unwrap();
    let path = session_path_for_key("l-ab12");
    let mut raw: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    raw["extra"] = json!("kept");
    fs::write(&path, raw.to_string()).unwrap();
    assert_eq!(read_session_key("l-ab12").unwrap().binding, None);

    bind_launch("l-ab12", "sid-1", "/w", "honey", "1", "orch", "%3").unwrap();
    let raw: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(raw["extra"], "kept");
    assert_eq!(raw["sessionId"], "sid-1");
    assert_eq!(raw["cwd"], "/w");
    assert_eq!(
        read_session_key("l-ab12").unwrap().binding,
        Some(binding("honey", "1", "orch"))
    );
    // the member's record is the launch's, read through the alias
    assert_eq!(
        read_session_key("m-honey.orch").unwrap().binding,
        Some(binding("honey", "1", "orch"))
    );

    // another launch for the same member: the alias is refused, and the
    // record it had bound first goes back to unbound — no half-bound
    // launch record is left behind
    let _other = bind_leader_socket(&hive_dir.join("l-cd34.sock"));
    write_session_key("l-cd34", "sid-9", "/w", None).unwrap();
    let err = bind_launch("l-cd34", "sid-9", "/w", "honey", "1", "orch", "%3").unwrap_err();
    assert!(err.to_string().contains("already bound"), "{err}");
    assert_eq!(read_session_key("l-cd34").unwrap().binding, None);
    assert_eq!(
        read_session_key("l-cd34").map(|r| r.session_id),
        Some("sid-9".to_string())
    );
    // a launch previously bound elsewhere keeps that binding when a new
    // bind is refused
    bind_launch("l-cd34", "sid-9", "/w", "honey", "1", "rex", "%4").unwrap();
    let err = bind_launch("l-cd34", "sid-9", "/w", "honey", "1", "orch", "%3").unwrap_err();
    assert!(err.to_string().contains("already bound"), "{err}");
    assert_eq!(
        read_session_key("l-cd34").unwrap().binding,
        Some(binding("honey", "1", "rex"))
    );
}

#[test]
fn test_rollback_launch_removes_only_its_own_alias_and_binding_and_keeps_the_leader() {
    let bed = setup();
    let hive_dir = bed.tmp.path().join("hive");
    let _listener = bind_leader_socket(&hive_dir.join("l-ab12.sock"));
    bind_launch("l-ab12", "sid-1", "/w", "honey", "1", "orch", "%3").unwrap();
    rollback_launch("l-cd34", "sid-9", "honey", "1", "orch", "%3").unwrap();
    assert!(launch_is_bound(
        "l-ab12", "sid-1", "honey", "1", "orch", "%3"
    ));
    assert_eq!(
        read_session_key("l-ab12").unwrap().binding,
        Some(binding("honey", "1", "orch"))
    );
    rollback_launch("l-ab12", "sid-1", "honey", "1", "orch", "%3").unwrap();
    assert!(!launch_is_bound(
        "l-ab12", "sid-1", "honey", "1", "orch", "%3"
    ));
    assert!(!hive_dir.join("m-honey.orch.alias").exists());
    // the leader's socket and record are the launcher's, untouched — the
    // record unbound again, its session kept
    assert!(hive_dir.join("l-ab12.sock").exists());
    assert_eq!(
        read_session_key("l-ab12"),
        Some(SessionRecord {
            session_id: "sid-1".to_string(),
            cwd: "/w".to_string(),
            binding: None,
        })
    );
    rollback_launch("l-ab12", "sid-1", "honey", "1", "orch", "%3").unwrap();

    // a launch since bound to another member keeps that binding when the
    // first member's bind is rolled back
    bind_launch("l-ab12", "sid-1", "/w", "honey", "1", "orch", "%3").unwrap();
    bind_launch("l-ab12", "sid-1", "/w", "honey", "1", "rex", "%4").unwrap();
    rollback_launch("l-ab12", "sid-1", "honey", "1", "orch", "%3").unwrap();
    assert!(!hive_dir.join("m-honey.orch.alias").exists());
    assert!(launch_is_bound(
        "l-ab12", "sid-1", "honey", "1", "rex", "%4"
    ));
    assert_eq!(
        read_session_key("l-ab12").unwrap().binding,
        Some(binding("honey", "1", "rex"))
    );
}

/// A launch bound to a member name, then bound to the same name again
/// under another instance of the team (recreated, the launch still on
/// its session) or after the launch was re-minted onto another session:
/// the alias and the record's binding are the later bind's, and the
/// earlier bind's rollback — the same name, not the same identity —
/// undoes nothing. The later bind's own rollback undoes it.
#[test]
fn test_rollback_launch_keeps_a_same_name_bind_of_another_instance_or_session() {
    let bed = setup();
    let hive_dir = bed.tmp.path().join("hive");
    let _listener = bind_leader_socket(&hive_dir.join("l-ab12.sock"));
    bind_launch("l-ab12", "sid-1", "/w", "honey", "1", "orch", "%3").unwrap();
    bind_launch("l-ab12", "sid-1", "/w", "honey", "2", "orch", "%4").unwrap();
    assert_eq!(
        read_session_key("l-ab12").unwrap().binding,
        Some(binding("honey", "2", "orch"))
    );
    assert!(launch_is_bound(
        "l-ab12", "sid-1", "honey", "2", "orch", "%4"
    ));
    assert!(!launch_is_bound(
        "l-ab12", "sid-1", "honey", "1", "orch", "%3"
    ));
    rollback_launch("l-ab12", "sid-1", "honey", "1", "orch", "%3").unwrap();
    assert_eq!(alias_target("m-honey.orch").as_deref(), Some("l-ab12"));
    assert_eq!(
        read_session_key("l-ab12").unwrap().binding,
        Some(binding("honey", "2", "orch")),
        "the earlier instance's rollback cleared the later bind"
    );
    // the launch re-minted onto another session, bound to the name again
    write_session_key(
        "l-ab12",
        "sid-2",
        "/w",
        Some(&binding("honey", "2", "orch")),
    )
    .unwrap();
    rollback_launch("l-ab12", "sid-1", "honey", "2", "orch", "%4").unwrap();
    assert_eq!(alias_target("m-honey.orch").as_deref(), Some("l-ab12"));
    assert_eq!(
        read_session_key("l-ab12").unwrap().binding,
        Some(binding("honey", "2", "orch"))
    );
    // its own rollback undoes it
    rollback_launch("l-ab12", "sid-2", "honey", "2", "orch", "%4").unwrap();
    assert!(alias_target("m-honey.orch").is_none());
    assert_eq!(read_session_key("l-ab12").unwrap().binding, None);
}

/// Two binds of one launch to two members, started together on their
/// own threads: one is refused (its member is another launch's), the
/// other succeeds. Whichever the launch lock admits first, the refused
/// bind restores the binding it read under the lock — so the succeeded
/// bind's record and alias survive, never the unbound record the refused
/// bind would have read before the lock.
#[test]
fn test_bind_launch_refused_beside_another_members_bind_keeps_that_bind() {
    let bed = setup();
    let hive_dir = bed.tmp.path().join("hive");
    let _leader = bind_leader_socket(&hive_dir.join("l-ab12.sock"));
    let _other = bind_leader_socket(&hive_dir.join("l-cd34.sock"));
    for round in 0..8 {
        write_session_key("l-ab12", "sid-1", "/w", None).unwrap();
        bind_launch("l-cd34", "sid-9", "/w", "honey", "1", "orch", "%5").unwrap();
        let start = Arc::new(Barrier::new(2));
        let succeeding = thread::spawn({
            let start = Arc::clone(&start);
            move || {
                start.wait();
                bind_launch("l-ab12", "sid-1", "/w", "honey", "1", "rex", "%4")
            }
        });
        start.wait();
        let refused = bind_launch("l-ab12", "sid-1", "/w", "honey", "1", "orch", "%3");
        succeeding.join().unwrap().unwrap();
        let err = refused.unwrap_err();
        assert!(err.to_string().contains("already bound"), "{err}");
        assert_eq!(
            read_session_key("l-ab12").unwrap().binding,
            Some(binding("honey", "1", "rex")),
            "round {round}: the refused bind's restore overwrote the succeeded one"
        );
        assert_eq!(alias_target("m-honey.rex").as_deref(), Some("l-ab12"));
        assert_eq!(alias_target("m-honey.orch").as_deref(), Some("l-cd34"));
        rollback_launch("l-ab12", "sid-1", "honey", "1", "rex", "%4").unwrap();
        rollback_launch("l-cd34", "sid-9", "honey", "1", "orch", "%5").unwrap();
        assert_eq!(read_session_key("l-ab12").unwrap().binding, None);
        assert!(alias_target("m-honey.rex").is_none());
        assert!(alias_target("m-honey.orch").is_none());
    }
}

/// The launch lock is the boundary: a bind writes nothing to the record
/// and publishes nothing before it holds the lock, and a rollback removes
/// nothing before it does.
#[test]
fn test_bind_and_rollback_write_nothing_before_they_hold_the_launch_lock() {
    let bed = setup();
    let hive_dir = bed.tmp.path().join("hive");
    let _leader = bind_leader_socket(&hive_dir.join("l-ab12.sock"));
    write_session_key("l-ab12", "sid-1", "/w", None).unwrap();
    let held = launch_lock("l-ab12").unwrap();
    let bind = thread::spawn(|| bind_launch("l-ab12", "sid-1", "/w", "honey", "1", "orch", "%3"));
    thread::sleep(Duration::from_millis(200));
    assert!(!bind.is_finished(), "the bind did not wait for the lock");
    assert_eq!(read_session_key("l-ab12").unwrap().binding, None);
    assert!(alias_target("m-honey.orch").is_none());
    drop(held);
    bind.join().unwrap().unwrap();
    assert!(launch_is_bound(
        "l-ab12", "sid-1", "honey", "1", "orch", "%3"
    ));

    let held = launch_lock("l-ab12").unwrap();
    let rollback = thread::spawn(|| rollback_launch("l-ab12", "sid-1", "honey", "1", "orch", "%3"));
    thread::sleep(Duration::from_millis(200));
    assert!(
        !rollback.is_finished(),
        "the rollback did not wait for the lock"
    );
    assert!(launch_is_bound(
        "l-ab12", "sid-1", "honey", "1", "orch", "%3"
    ));
    drop(held);
    rollback.join().unwrap().unwrap();
    assert!(alias_target("m-honey.orch").is_none());
    assert_eq!(read_session_key("l-ab12").unwrap().binding, None);
}

/// A kill through the member's alias removes nothing of the launch —
/// its session record above all, the file a bind reads and writes —
/// before it holds the launch lock: with the lock held elsewhere, the
/// reap runs (the process listing is where it spends its time) and the
/// record and alias stay until the lock is released.
#[test]
fn test_a_member_kill_removes_the_launchs_files_only_under_the_launch_lock() {
    let bed = setup();
    let hive_dir = bed.tmp.path().join("hive");
    let _listener = bind_leader_socket(&hive_dir.join("l-ab12.sock"));
    bind_launch("l-ab12", "sid-1", "/w", "honey", "1", "orch", "%3").unwrap();
    let record_path = session_path_for_key("l-ab12");
    let held = launch_lock("l-ab12").unwrap();
    let (reaped_tx, reaped_rx) = std::sync::mpsc::channel();
    let kill = thread::spawn(move || {
        set_process_listing(move || {
            let _ = reaped_tx.send(());
            Vec::new()
        });
        kill_daemon_key("m-honey.orch");
    });
    reaped_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while record_path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    let record_survived_lock = record_path.exists();
    let alias_during_lock = alias_target("m-honey.orch");
    let kill_finished = kill.is_finished();
    drop(held);
    kill.join().unwrap();
    assert!(
        record_survived_lock,
        "kill removed the session while another holder owned the launch lock; \
         alias={alias_during_lock:?}, kill_finished={kill_finished}"
    );
    assert_eq!(alias_during_lock.as_deref(), Some("l-ab12"));
    assert!(!kill_finished);
    assert!(!record_path.exists());
    assert!(!hive_dir.join("l-ab12.sock").exists());
    assert!(alias_target("m-honey.orch").is_none());
    assert!(hive_dir.join("l-ab12.bind-lock").exists());
}

/// The same boundary on the direct entry: a kill by launch key holds the
/// launch lock before the socket and record go.
#[test]
fn test_a_launch_key_kill_removes_its_files_only_under_the_launch_lock() {
    let bed = setup();
    let hive_dir = bed.tmp.path().join("hive");
    let _listener = bind_leader_socket(&hive_dir.join("l-ab12.sock"));
    write_session_key("l-ab12", "sid-1", "/w", None).unwrap();
    let record_path = session_path_for_key("l-ab12");
    let held = launch_lock("l-ab12").unwrap();
    let (reaped_tx, reaped_rx) = std::sync::mpsc::channel();
    let kill = thread::spawn(move || {
        set_process_listing(move || {
            let _ = reaped_tx.send(());
            Vec::new()
        });
        kill_daemon_key("l-ab12");
    });
    reaped_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(record_path.exists(), "record removed under a held lock");
    assert!(hive_dir.join("l-ab12.sock").exists());
    assert!(!kill.is_finished(), "the kill did not wait for the lock");
    drop(held);
    kill.join().unwrap();
    assert!(!record_path.exists());
    assert!(!hive_dir.join("l-ab12.sock").exists());
    assert!(hive_dir.join("l-ab12.bind-lock").exists());
}

/// The probe before the lock authorizes nothing: a bind that saw the
/// leader, then waited for the lock while the leader went, fails once it
/// holds the lock and publishes no alias.
#[test]
fn test_bind_fails_when_the_leader_vanishes_while_it_waits_for_the_launch_lock() {
    let bed = setup();
    let sock = bed.tmp.path().join("hive/l-ab12.sock");
    let leader = bind_leader_socket(&sock);
    write_session_key("l-ab12", "sid-1", "/w", None).unwrap();
    assert!(
        probe_socket(&sock),
        "the bind's probe before the lock sees the leader"
    );
    let held = launch_lock("l-ab12").unwrap();
    let bind = thread::spawn(|| bind_launch("l-ab12", "sid-1", "/w", "honey", "1", "orch", "%3"));
    thread::sleep(Duration::from_millis(200));
    assert!(!bind.is_finished(), "the bind did not wait for the lock");
    // the leader goes while the bind waits: its lock released, its socket gone
    drop(leader);
    fs::remove_file(&sock).unwrap();
    drop(held);
    let result = bind.join().unwrap();
    let alias = alias_target("m-honey.orch");
    let err = result.expect_err(&format!(
        "bind succeeded after the leader disappeared while it waited; alias={alias:?}"
    ));
    assert!(err.to_string().contains("not listening"), "{err}");
    assert!(alias.is_none());
    assert_eq!(read_session_key("l-ab12").unwrap().binding, None);
}

#[test]
fn test_stop_launch_steps_back_once_a_member_owns_the_leader() {
    let bed = setup();
    let hive_dir = bed.tmp.path().join("hive");
    set_process_listing(Vec::new);
    let listener = bind_leader_socket(&hive_dir.join("l-ab12.sock"));
    write_session_key("l-ab12", "sid-1", "/w", None).unwrap();
    bind_launch("l-ab12", "sid-1", "/w", "honey", "1", "orch", "%3").unwrap();
    // bound: the launcher's stop is a no-op
    stop_launch("l-ab12", "sid-1");
    assert!(hive_dir.join("l-ab12.sock").exists());
    assert!(hive_dir.join("l-ab12.session").exists());
    // the member's kill reaches the leader through the alias and takes the
    // alias with it
    kill_daemon_key("m-honey.orch");
    assert!(!hive_dir.join("l-ab12.sock").exists());
    assert!(!hive_dir.join("l-ab12.session").exists());
    assert!(!hive_dir.join("m-honey.orch.alias").exists());
    // the launch's lock file stays: flock is by inode, a bind or rollback
    // racing this kill must lock the same one
    assert!(hive_dir.join("l-ab12.bind-lock").exists());
    drop(listener);
    // unbound: the launcher's stop removes the key's files
    let _listener = bind_leader_socket(&hive_dir.join("l-cd34.sock"));
    write_session_key("l-cd34", "sid-2", "/w", None).unwrap();
    stop_launch("l-cd34", "sid-2");
    assert!(!hive_dir.join("l-cd34.sock").exists());
    assert!(!hive_dir.join("l-cd34.session").exists());
    // a pane key is not a launch: untouched
    let _pane = bind_leader_socket(&hive_dir.join("p7.sock"));
    stop_launch("p7", "sid-3");
    assert!(hive_dir.join("p7.sock").exists());
}

#[test]
fn test_two_launches_racing_for_one_member_bind_at_most_one() {
    let bed = setup();
    let hive_dir = bed.tmp.path().join("hive");
    let _a = bind_leader_socket(&hive_dir.join("l-aaaa.sock"));
    let _b = bind_leader_socket(&hive_dir.join("l-bbbb.sock"));
    let home = bed.tmp.path().to_path_buf();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut hands = Vec::new();
    for key in ["l-aaaa", "l-bbbb"] {
        let barrier = std::sync::Arc::clone(&barrier);
        let home = home.clone();
        hands.push(std::thread::spawn(move || {
            // the env var is process-global; the racing threads read the
            // same GROK_HOME the bed pinned
            assert_eq!(
                std::env::var("GROK_HOME").ok().map(PathBuf::from),
                Some(home)
            );
            barrier.wait();
            bind_launch(key, "sid-1", "/w", "honey", "1", "orch", "%3").map_err(|e| e.to_string())
        }));
    }
    let results: Vec<Result<(), String>> = hands.into_iter().map(|h| h.join().unwrap()).collect();
    let wins = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(wins, 1, "{results:?}");
    let winner = alias_target("m-honey.orch").unwrap();
    assert!(winner == "l-aaaa" || winner == "l-bbbb");
    let loser = results
        .iter()
        .find(|r| r.is_err())
        .unwrap()
        .as_ref()
        .unwrap_err();
    assert!(
        loser.contains(&format!("already bound to launch {winner}")),
        "{loser}"
    );
    // no staging file left behind
    assert!(fs::read_dir(&hive_dir)
        .unwrap()
        .flatten()
        .all(|e| !e.file_name().to_string_lossy().ends_with(".alias-tmp")));
}

#[test]
fn test_a_corrupt_alias_is_refused_never_overwritten() {
    let bed = setup();
    let hive_dir = bed.tmp.path().join("hive");
    let _a = bind_leader_socket(&hive_dir.join("l-aaaa.sock"));
    fs::write(hive_dir.join("m-honey.orch.alias"), "not a key").unwrap();
    let err = bind_launch("l-aaaa", "sid-1", "/w", "honey", "1", "orch", "%3").unwrap_err();
    assert!(err.to_string().contains("names no launch"), "{err}");
    assert_eq!(
        fs::read_to_string(hive_dir.join("m-honey.orch.alias")).unwrap(),
        "not a key"
    );
}

#[test]
fn test_a_member_kill_leaves_an_alias_rebound_to_another_launch_meanwhile() {
    let bed = setup();
    let hive_dir = bed.tmp.path().join("hive");
    // the kill resolves through l-ab12; a concurrent join rebinds the member
    // to l-cd34 before the alias removal runs (the process listing seam is
    // where the reap spends its time)
    let _a = bind_leader_socket(&hive_dir.join("l-ab12.sock"));
    let _b = bind_leader_socket(&hive_dir.join("l-cd34.sock"));
    write_session_key("l-ab12", "sid-1", "/w", None).unwrap();
    bind_launch("l-ab12", "sid-1", "/w", "honey", "1", "orch", "%3").unwrap();
    let alias = hive_dir.join("m-honey.orch.alias");
    set_process_listing(move || {
        fs::write(&alias, "l-cd34").unwrap();
        Vec::new()
    });
    kill_daemon_key("m-honey.orch");
    assert_eq!(alias_target("m-honey.orch").as_deref(), Some("l-cd34"));
    // l-ab12's own files went, l-cd34 is untouched
    assert!(!hive_dir.join("l-ab12.session").exists());
    assert!(hive_dir.join("l-cd34.sock").exists());
}

#[test]
fn test_sleep_pool_observation_is_scoped_and_requires_idle_evidence() {
    let _bed = setup();
    let pool = GrokClientPool::new();
    let key = "m-cedar.worker";
    write_session_key(key, SID, CWD, None).unwrap();
    let proc = FakeProc::new(Some(responder(None, vec![])));
    let handed = Arc::clone(&proc);
    set_stdio_spawn(move |_| Ok(handed.clone() as Arc<dyn LeaderProc>));
    let client = Arc::new(GrokStdioClient::new(key).unwrap());
    assert!(client.handshake());
    pool.hold_for_test(key, client.clone());
    assert_eq!(pool.idle_owned_keys("other"), Some(Vec::new()));
    assert_eq!(pool.idle_owned_keys("cedar"), Some(vec![key.into()]));
    proc.feed(&activity("working"));
    settle(&client, |rt| rt.turn_open == Some(true));
    assert_eq!(pool.idle_owned_keys("cedar"), None);
    proc.feed(&activity("idle"));
    settle(&client, |rt| rt.turn_open == Some(false));
    assert_eq!(
        pool.idle_owned_keys("cedar"),
        Some(vec!["m-cedar.worker".into()])
    );
    fs::write(alias_path_for_key(key), "l-rebound").unwrap();
    assert_eq!(
        pool.idle_owned_keys("cedar"),
        None,
        "rebound key is not owned by the held client"
    );
    fs::remove_file(alias_path_for_key(key)).unwrap();
    let rid = client.prompt_tracked("queued").unwrap();
    assert_eq!(
        pool.idle_owned_keys("cedar"),
        None,
        "outstanding prompt cannot be retired on idle notification alone"
    );
    proc.feed(&prompt_response(rid, "queued-prompt", "end_turn"));
    settle_ended(&client, rid);
    teardown(&client, &proc);
    assert_eq!(
        pool.idle_owned_keys("cedar"),
        None,
        "dead client cannot prove leader idle"
    );
}

/// The desk's sleep hands the pool a key; the only process that may go is
/// the stdio client this process spawned. The leader on the socket is the
/// member's own, the TUI in its pane is one of the leader's clients, and
/// the session and alias that resume the member read back unchanged.
#[test]
fn test_pool_drop_closes_only_owned_stdio_client() {
    let mut bed = setup();
    bed.env.set("HIVE_HOME", bed.tmp.path().join("home"));
    let key = "m-cedar.worker";
    let spare_key = "m-cedar.scribe";
    fs::create_dir_all(bed.tmp.path().join("hive")).unwrap();
    fs::write(alias_path_for_key(key), "l-cafe").unwrap();
    let sock = socket_path_for_key(key);
    fs::write(&sock, "").unwrap();
    fs::write(sock.with_extension("pid"), "4321").unwrap();
    let (_procs, killed) = set_process_table(vec![
        (11, tui_args(&sock)),
        (12, stdio_args(&sock)),
        (4321, leader_args(&sock)),
    ]);

    let pool = GrokClientPool::new();
    let owned = FakeProc::new(Some(responder(None, Vec::new())));
    let handout = Arc::clone(&owned);
    set_stdio_spawn(move |_argv| Ok(handout.clone() as Arc<dyn LeaderProc>));
    write_session_key(key, SID, CWD, None).unwrap();
    let client = Arc::new(GrokStdioClient::new(key).unwrap());
    assert!(client.handshake());
    pool.hold_for_test(key, client.clone());
    let spare = FakeProc::new(Some(responder(None, Vec::new())));
    let handout = Arc::clone(&spare);
    set_stdio_spawn(move |_argv| Ok(handout.clone() as Arc<dyn LeaderProc>));
    write_session_key(spare_key, SID, CWD, None).unwrap();
    let spare_client = Arc::new(GrokStdioClient::new(spare_key).unwrap());
    assert!(spare_client.handshake());
    pool.hold_for_test(spare_key, spare_client.clone());

    pool.drop_key(key);

    assert!(owned.terminated(), "the pool's own client is closed");
    assert!(!client.is_alive());
    assert!(
        killed.lock().unwrap().is_empty(),
        "no process-group signal: {:?}",
        killed.lock().unwrap()
    );
    assert!(sock.exists(), "the leader keeps its socket");
    assert!(sock.with_extension("pid").exists());
    assert_eq!(alias_target(key).as_deref(), Some("l-cafe"));
    assert_eq!(
        read_session_key(key).map(|record| record.session_id),
        Some(SID.to_string())
    );
    assert!(!spare.terminated(), "another key's client is untouched");
    assert_eq!(spare_client.turn_open(), None);
    assert!(spare_client.is_alive());
    teardown(&spare_client, &spare);
}

// ----------------------------------------------------------------------
// retained binding and revival
// ----------------------------------------------------------------------

/// A retained member: team `cedar` (instance 123) lists grok member
/// `worker` on `SID`; the record on `key` is bound to the same; nothing
/// listens on the socket.
fn retained_member(bed: &mut TestBed, key: &str) {
    bed.env.set("HIVE_HOME", bed.tmp.path().join("home"));
    write_session_key(key, SID, CWD, Some(&binding("cedar", "123", "worker"))).unwrap();
    record_cedar("123", Some(SID));
}

/// The registry's `cedar`: one grok row `worker` on *session_id*, or an
/// empty roster.
fn record_cedar(created_at: &str, session_id: Option<&str>) {
    let rows: Vec<Map<String, Value>> = session_id
        .map(|sid| {
            json!({"name": "worker", "cli": "grok", "sessionId": sid})
                .as_object()
                .unwrap()
                .clone()
        })
        .into_iter()
        .collect();
    crate::registry::record_team("cedar", CWD, created_at, &rows, "").unwrap();
}

/// A fake member leader that binds a listener where it is asked to, the
/// stdio client on it answering the handshake and echoing prompts.
fn revivable_engine() -> (Arc<Mutex<usize>>, Arc<FakeProc>) {
    let spawns = set_listening_daemon_spawn();
    let proc = FakeProc::new(Some(responder(Some(on_prompt_queue_echo()), vec![])));
    let handed = Arc::clone(&proc);
    set_stdio_spawn(move |_| Ok(handed.clone() as Arc<dyn LeaderProc>));
    (spawns, proc)
}

/// A confirmation on *key* for a submission a test override answers: the
/// override stands in for the client and every identity check with it,
/// so the fields are nominal.
fn override_confirmation(key: &str) -> Confirmation {
    Confirmation {
        key: key.to_string(),
        binding: binding("cedar", "123", "worker"),
        session_id: SID.to_string(),
        socket_path: socket_path_for_key(key).to_string_lossy().into_owned(),
        generation: 0,
    }
}

fn methods(proc: &FakeProc) -> Vec<String> {
    proc.sent()
        .iter()
        .filter_map(|msg| {
            msg.get("method")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

#[test]
fn test_session_record_round_trips_its_binding_and_keeps_unknown_fields() {
    let _bed = setup();
    let key = "m-cedar.worker";
    write_session_key(key, SID, CWD, Some(&binding("cedar", "123", "worker"))).unwrap();
    let path = session_path_for_key(key);
    let mut raw: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(raw["team"], "cedar");
    assert_eq!(raw["createdAt"], "123");
    assert_eq!(raw["member"], "worker");
    raw["extra"] = json!("preserved");
    fs::write(&path, raw.to_string()).unwrap();
    assert_eq!(
        read_session_key(key),
        Some(SessionRecord {
            session_id: SID.to_string(),
            cwd: CWD.to_string(),
            binding: Some(binding("cedar", "123", "worker")),
        })
    );
    // rebinding rewrites only the binding fields
    bind_session_key(key, Some(&binding("cedar", "456", "worker"))).unwrap();
    let raw: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(raw["extra"], "preserved");
    assert_eq!(raw["sessionId"], SID);
    assert_eq!(raw["createdAt"], "456");
    // and clearing it leaves an unbound record with everything else intact
    bind_session_key(key, None).unwrap();
    let raw: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(raw["extra"], "preserved");
    assert!(raw.get("team").is_none() && raw.get("member").is_none());
    assert_eq!(read_session_key(key).unwrap().binding, None);
    // a record from before the binding existed reads as unbound
    write_session_key(key, SID, CWD, None).unwrap();
    assert_eq!(read_session_key(key).unwrap().binding, None);
    // a partial binding is no binding
    fs::write(
        &path,
        json!({"sessionId": SID, "cwd": CWD, "team": "cedar", "member": "worker"}).to_string(),
    )
    .unwrap();
    assert_eq!(read_session_key(key).unwrap().binding, None);
}

#[test]
fn test_retained_holds_for_the_bound_instance_and_row_while_no_leader_listens() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    assert_eq!(binding_holds(key), Ok(binding("cedar", "123", "worker")));
    assert!(retained(key));
    // the registry's instance may be stored as a number
    let entry_path = crate::registry::entry_path("cedar").unwrap();
    let mut entry: Value = serde_json::from_slice(&fs::read(&entry_path).unwrap()).unwrap();
    entry["createdAt"] = json!(123.0);
    fs::write(&entry_path, entry.to_string()).unwrap();
    assert!(retained(key));
    // a leader listening is an online member, not a retained one — the
    // binding itself still holds
    let _listener = bind_leader_socket(&socket_path_for_key(key));
    assert!(binding_holds(key).is_ok());
    assert!(!retained(key));
    // the same through a launch alias: the member's record is the launch's
    let aliased = "m-cedar.aliased";
    fs::write(alias_path_for_key(aliased), "l-cafe").unwrap();
    write_session_key(
        "l-cafe",
        SID,
        CWD,
        Some(&binding("cedar", "123", "aliased")),
    )
    .unwrap();
    record_cedar_rows(vec![("worker", SID), ("aliased", SID)]);
    assert!(retained(aliased));
}

/// `cedar` (instance 123) with these grok rows.
fn record_cedar_rows(rows: Vec<(&str, &str)>) {
    let rows: Vec<Map<String, Value>> = rows
        .into_iter()
        .map(|(name, sid)| {
            json!({"name": name, "cli": "grok", "sessionId": sid})
                .as_object()
                .unwrap()
                .clone()
        })
        .collect();
    crate::registry::record_team("cedar", CWD, "123", &rows, "").unwrap();
}

#[test]
fn test_retained_refuses_a_same_name_team_of_another_instance() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    record_cedar("456", Some(SID));
    let err = binding_holds(key).unwrap_err();
    assert!(err.contains("another instance"), "{err}");
    assert!(!retained(key));
}

#[test]
fn test_retained_refuses_a_member_respawned_onto_another_session() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    record_cedar("123", Some("22222222-2222-3333-4444-555555555555"));
    let err = binding_holds(key).unwrap_err();
    assert!(err.contains("another session"), "{err}");
    assert!(!retained(key));
}

#[test]
fn test_retained_refuses_a_row_gone_from_the_roster() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    record_cedar("123", None);
    let err = binding_holds(key).unwrap_err();
    assert!(err.contains("not on the roster"), "{err}");
    assert!(!retained(key));
    // a whole team gone
    fs::remove_file(crate::registry::entry_path("cedar").unwrap()).unwrap();
    let err = binding_holds(key).unwrap_err();
    assert!(err.contains("not in the registry"), "{err}");
    assert!(!retained(key));
}

#[test]
fn test_retained_refuses_a_missing_or_unbound_record() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    fs::remove_file(session_path_for_key(key)).unwrap();
    let err = binding_holds(key).unwrap_err();
    assert!(err.contains("no session record"), "{err}");
    assert!(!retained(key));
    // a record from before the binding existed: the same name is no evidence
    write_session_key(key, SID, CWD, None).unwrap();
    let err = binding_holds(key).unwrap_err();
    assert!(err.contains("names no member"), "{err}");
    assert!(!retained(key));
    // a record bound to another member of the team
    write_session_key(key, SID, CWD, Some(&binding("cedar", "123", "other"))).unwrap();
    let err = binding_holds(key).unwrap_err();
    assert!(err.contains("bound to cedar.other"), "{err}");
    assert!(!retained(key));
    // a row that is not grok
    write_session_key(key, SID, CWD, Some(&binding("cedar", "123", "worker"))).unwrap();
    crate::registry::record_team(
        "cedar",
        CWD,
        "123",
        &[json!({"name": "worker", "cli": "codex", "sessionId": SID})
            .as_object()
            .unwrap()
            .clone()],
        "",
    )
    .unwrap();
    let err = binding_holds(key).unwrap_err();
    assert!(err.contains("not a grok member"), "{err}");
    // a pane key or a launch key names no member
    assert!(binding_holds("p19").is_err());
    assert!(!retained("l-cafe"));
}

/// A submission never raises a leader: a retained member's send fails
/// until something revives it (the entry's `revive`, not the pool).
#[test]
fn test_submission_does_not_raise_a_leader_for_a_retained_member() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    let (spawns, proc) = revivable_engine();
    let pool = GrokClientPool::new();
    assert_eq!(pool.send_to_key(key, "hello"), None);
    // a confirmation this pool never made — another process's revive —
    // holds no client here and raises none
    let foreign = Confirmation {
        generation: 1,
        ..override_confirmation(key)
    };
    assert_eq!(pool.send_confirmed(&foreign, "hello"), None);
    let err = pool.dispatch_confirmed(&foreign, "task").unwrap_err();
    assert!(err.contains(key), "{err}");
    assert_eq!(*spawns.lock().unwrap(), 0);
    assert!(proc.sent().is_empty());
    assert!(!socket_path_for_key(key).exists());
}

#[test]
fn test_revive_raises_the_leader_loads_the_session_and_sends_nothing() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    let (spawns, proc) = revivable_engine();
    let pool = GrokClientPool::new();
    // a cold runtime read leaves a connect cooldown behind
    assert!(pool.runtime_for_key(key).is_none());
    assert!(pool.state.lock().unwrap().cooldown.contains_key(key));
    assert_eq!(*spawns.lock().unwrap(), 0);

    let revival = pool.revive_key(key).unwrap();
    assert!(revival.raised);
    assert_eq!(revival.turn_open, None); // an empty replay: no turn evidence
    assert_eq!(revival.confirmation.key, key);
    assert_eq!(
        revival.confirmation.binding,
        binding("cedar", "123", "worker")
    );
    assert_eq!(revival.confirmation.session_id, SID);
    assert_eq!(*spawns.lock().unwrap(), 1);
    assert!(!pool.state.lock().unwrap().cooldown.contains_key(key));
    // initialize + session/load of the recorded session; no session/new,
    // no prompt
    assert_eq!(methods(&proc), vec!["initialize", "session/load"]);
    let sent = proc.sent();
    assert_eq!(sent[1]["params"]["sessionId"], SID);
    // the pool now holds the client on that session
    let client = pool.client_for_key(key).unwrap();
    assert_eq!(client.session_id().as_deref(), Some(SID));
    // no longer retained: online
    assert!(!retained(key));

    // a second revive is a no-op on an online member: nothing raised,
    // nothing reloaded, the same client, the same identity confirmed
    let again = pool.revive_key(key).unwrap();
    assert!(!again.raised);
    assert_eq!(again.confirmation, revival.confirmation);
    assert_eq!(again.confirmation.generation, client.generation());
    assert_eq!(*spawns.lock().unwrap(), 1);
    assert_eq!(methods(&proc), vec!["initialize", "session/load"]);
    assert!(Arc::ptr_eq(&client, &pool.client_for_key(key).unwrap()));

    // and the submission after it goes out on that client
    assert_eq!(
        pool.send_confirmed(&revival.confirmation, "hello"),
        Some(PROMPT_QUEUED)
    );
    assert_eq!(
        methods(&proc),
        vec!["initialize", "session/load", "session/prompt"]
    );
    teardown(&client, &proc);
}

#[test]
fn test_revive_refuses_before_raising_when_the_binding_does_not_hold() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    record_cedar("456", Some(SID));
    let (spawns, proc) = revivable_engine();
    let pool = GrokClientPool::new();
    let err = pool.revive_key(key).unwrap_err();
    assert!(matches!(err, ReviveFailure::NotRetained(_)), "{err}");
    assert!(err.to_string().contains("another instance"), "{err}");
    assert_eq!(*spawns.lock().unwrap(), 0);
    assert!(proc.sent().is_empty());
    assert!(!socket_path_for_key(key).exists());
}

#[test]
fn test_revive_reports_a_leader_that_does_not_start_apart_from_a_failed_handshake() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    // the leader exits before binding
    set_daemon_spawn(|_argv, _env| {
        Ok(Box::new(FakeDaemonChild {
            pid: 7778,
            returncode: Some(1),
            panic_on_terminate: false,
        }))
    });
    let pool = GrokClientPool::new();
    let err = pool.revive_key(key).unwrap_err();
    assert!(matches!(err, ReviveFailure::LeaderStart(_)), "{err}");

    // the leader is up, the stdio client never comes
    let spawns = set_listening_daemon_spawn();
    set_stdio_spawn(|_| Err(io::Error::other("no grok binary")));
    let err = pool.revive_key(key).unwrap_err();
    assert!(matches!(err, ReviveFailure::Handshake(_)), "{err}");
    assert_eq!(*spawns.lock().unwrap(), 1);
    // the failure left no client behind, and the member is still retained
    // only once the raised leader is gone — here it listens, so it is
    // online with no client
    assert!(pool.client_for_key(key).is_none());
    assert!(binding_holds(key).is_ok());
}

/// The barrier after a revive: the submission goes out on the client and
/// session the revive confirmed, or not at all.
#[test]
fn test_submission_after_revive_fails_when_the_leader_is_gone_and_raises_no_other() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    let (spawns, proc) = revivable_engine();
    let pool = GrokClientPool::new();
    let revival = pool.revive_key(key).unwrap();
    assert!(revival.raised);
    // the leader exits between the revive and the prompt (a kill: the
    // pool's client goes with it, then the socket)
    pool.drop_key(key);
    fs::remove_file(socket_path_for_key(key)).unwrap();
    assert_eq!(pool.send_confirmed(&revival.confirmation, "hello"), None);
    assert!(pool
        .dispatch_confirmed(&revival.confirmation, "task")
        .is_err());
    assert_eq!(*spawns.lock().unwrap(), 1, "no second leader");
    assert_eq!(methods(&proc), vec!["initialize", "session/load"]);
    assert!(proc.terminated());
}

#[test]
fn test_submission_after_revive_fails_when_the_record_is_swapped_and_rebinds_nothing() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    let (spawns, proc) = revivable_engine();
    let stdio_spawns = Arc::new(Mutex::new(0));
    let counted = Arc::clone(&stdio_spawns);
    let handed = Arc::clone(&proc);
    set_stdio_spawn(move |_| {
        *counted.lock().unwrap() += 1;
        Ok(handed.clone() as Arc<dyn LeaderProc>)
    });
    let pool = GrokClientPool::new();
    let revival = pool.revive_key(key).unwrap();
    assert!(revival.raised);
    assert_eq!(*stdio_spawns.lock().unwrap(), 1);
    // the record now names another session (a respawn under the same
    // name landed between the gate and the prompt)
    let other = "22222222-2222-3333-4444-555555555555";
    write_session_key(key, other, CWD, Some(&binding("cedar", "123", "worker"))).unwrap();
    assert_eq!(pool.send_confirmed(&revival.confirmation, "hello"), None);
    assert!(pool
        .dispatch_confirmed(&revival.confirmation, "task")
        .is_err());
    // no client on the new session, no prompt on the old, no second leader
    assert_eq!(*stdio_spawns.lock().unwrap(), 1);
    assert_eq!(*spawns.lock().unwrap(), 1);
    assert_eq!(methods(&proc), vec!["initialize", "session/load"]);
    assert!(proc.terminated(), "the stale client is closed");
    assert!(!pool.state.lock().unwrap().clients.contains_key(key));
}

/// A pool that never revived the key (a spawning or joining CLI's own)
/// binds once to a leader already listening; it raises none.
#[test]
fn test_submission_on_a_key_never_revived_binds_once_to_a_listening_leader() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    let (spawns, proc) = revivable_engine();
    let _leader = bind_leader_socket(&socket_path_for_key(key));
    let pool = GrokClientPool::new();
    assert_eq!(pool.send_to_key(key, "hello"), Some(PROMPT_QUEUED));
    assert_eq!(*spawns.lock().unwrap(), 0);
    assert_eq!(
        methods(&proc),
        vec!["initialize", "session/load", "session/prompt"]
    );
    let client = pool.client_for_key(key).unwrap();
    teardown(&client, &proc);
}

/// A client rebound behind a revive (a runtime read that reconnected)
/// is not the one the revive confirmed: the submission fails rather than
/// ride a connection its gate never saw.
#[test]
fn test_submission_after_revive_fails_on_a_client_rebound_since() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    let (spawns, proc) = revivable_engine();
    let pool = GrokClientPool::new();
    let revival = pool.revive_key(key).unwrap();
    assert!(revival.raised);
    let confirmed = pool.client_for_key(key).unwrap();
    assert_eq!(revival.confirmation.generation, confirmed.generation());
    // the connection drops and a runtime read reconnects on the same session
    proc.eof();
    let handle = confirmed.reader.lock().unwrap().take();
    if let Some(handle) = handle {
        let _ = handle.join();
    }
    assert!(!confirmed.is_alive());
    let replacement = FakeProc::new(Some(responder(Some(on_prompt_queue_echo()), vec![])));
    let handed = Arc::clone(&replacement);
    set_stdio_spawn(move |_| Ok(handed.clone() as Arc<dyn LeaderProc>));
    let rebound = pool.client_for_key(key).unwrap();
    assert_ne!(rebound.generation(), confirmed.generation());
    assert_eq!(pool.send_confirmed(&revival.confirmation, "hello"), None);
    assert!(pool
        .dispatch_confirmed(&revival.confirmation, "task")
        .is_err());
    assert_eq!(methods(&replacement), vec!["initialize", "session/load"]);
    assert_eq!(*spawns.lock().unwrap(), 1);
    // the rebound client is the runtime's, left alone; a revive confirms
    // it — another confirmation, this connection's — and the submission
    // on that one goes out
    assert!(rebound.is_alive());
    let again = pool.revive_key(key).unwrap();
    assert!(!again.raised);
    assert_eq!(again.confirmation.generation, rebound.generation());
    assert_ne!(again.confirmation, revival.confirmation);
    assert_eq!(
        pool.send_confirmed(&again.confirmation, "hello"),
        Some(PROMPT_QUEUED)
    );
    teardown(&rebound, &replacement);
}

/// The prompts a fake leader was sent, in order.
fn prompts(proc: &FakeProc) -> Vec<String> {
    proc.sent()
        .iter()
        .filter(|msg| msg.get("method").and_then(Value::as_str) == Some("session/prompt"))
        .map(|msg| {
            msg["params"]["prompt"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

/// The registry's instance of the team changes after the revive (a
/// same-named team recreated), the record untouched: the identity the
/// revive confirmed is no longer the key's, and the submission on it is
/// refused before any prompt.
#[test]
fn test_submission_after_revive_refuses_a_registry_instance_changed_since() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    let (spawns, proc) = revivable_engine();
    let pool = GrokClientPool::new();
    let revival = pool.revive_key(key).unwrap();
    let client = pool.client_for_key(key).unwrap();
    record_cedar("456", Some(SID));
    assert!(binding_holds(key).is_err());
    let result = pool.send_confirmed(&revival.confirmation, "stale-instance");
    let sent = methods(&proc);
    teardown(&client, &proc);
    assert_eq!(
        result, None,
        "old instance reached native transport: {sent:?}"
    );
    assert_eq!(sent, vec!["initialize", "session/load"]);
    assert_eq!(*spawns.lock().unwrap(), 1);
}

/// The member's roster row is gone after the revive (a kill landed):
/// the tracked dispatch on the confirmation is refused, nothing sent.
#[test]
fn test_submission_after_revive_refuses_a_roster_row_gone_since() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    let (spawns, proc) = revivable_engine();
    let pool = GrokClientPool::new();
    let revival = pool.revive_key(key).unwrap();
    let client = pool.client_for_key(key).unwrap();
    record_cedar("123", None);
    assert!(binding_holds(key).is_err());
    let result = pool.dispatch_confirmed(&revival.confirmation, "removed-member");
    let sent = methods(&proc);
    teardown(&client, &proc);
    assert!(
        result.is_err(),
        "removed member reached native transport: {result:?} {sent:?}"
    );
    assert_eq!(sent, vec!["initialize", "session/load"]);
    assert_eq!(*spawns.lock().unwrap(), 1);
}

/// The key is validly bound again after the revive — the same name and
/// session under another instance of the team, registry and record
/// agreeing — so the current binding holds, but it is another identity
/// than the one this revive confirmed: the earlier confirmation is
/// refused, the client (the record's still) stays for the next revive,
/// and that revive confirms the new identity, on which a submission
/// goes out.
#[test]
fn test_submission_after_revive_refuses_a_later_valid_binding_on_the_same_session() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    let (spawns, proc) = revivable_engine();
    let pool = GrokClientPool::new();
    let revival = pool.revive_key(key).unwrap();
    let client = pool.client_for_key(key).unwrap();
    record_cedar("456", Some(SID));
    bind_session_key(key, Some(&binding("cedar", "456", "worker"))).unwrap();
    assert!(binding_holds(key).is_ok());
    assert_eq!(
        pool.send_confirmed(&revival.confirmation, "replaced-binding"),
        None
    );
    assert!(pool
        .dispatch_confirmed(&revival.confirmation, "replaced-binding")
        .is_err());
    assert_eq!(methods(&proc), vec!["initialize", "session/load"]);
    assert!(client.is_alive(), "the client is the record's, left alone");
    assert!(Arc::ptr_eq(&client, &pool.client_for_key(key).unwrap()));

    let again = pool.revive_key(key).unwrap();
    assert!(!again.raised);
    assert_eq!(
        again.confirmation.binding,
        binding("cedar", "456", "worker")
    );
    assert_eq!(
        again.confirmation.generation,
        revival.confirmation.generation
    );
    assert_ne!(again.confirmation, revival.confirmation);
    assert_eq!(
        pool.send_confirmed(&again.confirmation, "new-identity"),
        Some(PROMPT_QUEUED)
    );
    assert_eq!(prompts(&proc), vec!["new-identity"]);
    assert_eq!(*spawns.lock().unwrap(), 1);
    teardown(&client, &proc);
}

/// Two requests on one key, each with its own revive, as the hived serves
/// them (one thread per request): A revives; the member is rebound — the
/// same name and session under another instance of the team — and B
/// revives that, on the same connection. A's submission on what A
/// confirmed is refused; B's on what B confirmed goes out. What B
/// confirmed never stood in for A: no shared state a second revive could
/// overwrite from under the first.
#[test]
fn test_two_requests_revive_apart_and_the_earlier_confirmation_is_refused() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    let (spawns, proc) = revivable_engine();
    let pool = Arc::new(GrokClientPool::new());
    let a = pool.revive_key(key).unwrap();
    assert!(a.raised);
    record_cedar("456", Some(SID));
    bind_session_key(key, Some(&binding("cedar", "456", "worker"))).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let request_b = thread::spawn({
        let pool = Arc::clone(&pool);
        let barrier = Arc::clone(&barrier);
        move || {
            let b = pool.revive_key(key).unwrap();
            barrier.wait(); // B has revived; A submits on A's confirmation
            barrier.wait(); // A was refused; B submits on B's
            let sent = pool.send_confirmed(&b.confirmation, "from-b");
            (b, sent)
        }
    });
    barrier.wait();
    assert_eq!(pool.send_confirmed(&a.confirmation, "from-a"), None);
    assert!(pool.dispatch_confirmed(&a.confirmation, "from-a").is_err());
    assert_eq!(methods(&proc), vec!["initialize", "session/load"]);
    barrier.wait();
    let (b, sent) = request_b.join().unwrap();
    assert!(!b.raised);
    assert_eq!(b.confirmation.binding, binding("cedar", "456", "worker"));
    assert_eq!(b.confirmation.generation, a.confirmation.generation);
    assert_eq!(sent, Some(PROMPT_QUEUED));
    assert_eq!(prompts(&proc), vec!["from-b"]);
    assert_eq!(*spawns.lock().unwrap(), 1);
    let client = pool.client_for_key(key).unwrap();
    teardown(&client, &proc);
}

/// The member's alias is rebound to another launch after the revive — a
/// rollback and a bind of the same team, instance, member and session,
/// so the record the key resolves to and the registry still agree with
/// what was confirmed. The confirmation pinned the socket the revive
/// resolved; the key resolves to another one now, and the submission on
/// the earlier confirmation is refused before any prompt reaches the old
/// leader. The client on the old socket is not the key's any more and is
/// closed.
#[test]
fn test_submission_after_revive_refuses_a_member_alias_rebound_to_another_launch() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    let old_sock = bed.tmp.path().join("hive/l-ab12.sock");
    let new_sock = bed.tmp.path().join("hive/l-cd34.sock");
    let _old_listener = bind_leader_socket(&old_sock);
    let _new_listener = bind_leader_socket(&new_sock);
    bind_launch("l-ab12", SID, CWD, "cedar", "123", "worker", "%3").unwrap();
    let (spawns, proc) = revivable_engine();
    let pool = GrokClientPool::new();
    let revival = pool.revive_key(key).unwrap();
    assert!(!revival.raised);
    assert_eq!(revival.confirmation.socket_path, old_sock.to_string_lossy());
    let client = pool.client_for_key(key).unwrap();
    assert_eq!(client.socket_path, old_sock.to_string_lossy());
    rollback_launch("l-ab12", SID, "cedar", "123", "worker", "%3").unwrap();
    bind_launch("l-cd34", SID, CWD, "cedar", "123", "worker", "%4").unwrap();
    assert_eq!(canonical_key(key), "l-cd34");
    assert_eq!(binding_holds(key).unwrap(), revival.confirmation.binding);
    let result = pool.send_confirmed(&revival.confirmation, "review-old-launch");
    let sent = methods(&proc);
    assert_eq!(
        result, None,
        "old launch accepted a prompt after the alias moved to l-cd34: {sent:?}"
    );
    assert!(pool
        .dispatch_confirmed(&revival.confirmation, "review-old-launch")
        .is_err());
    assert_eq!(sent, vec!["initialize", "session/load"]);
    assert!(!client.is_alive(), "the client on the old socket is closed");
    assert!(!pool.state.lock().unwrap().clients.contains_key(key));
    assert_eq!(*spawns.lock().unwrap(), 0);
    teardown(&client, &proc);
}

/// `client_for_key` on a key whose alias moved: the pooled client still
/// connected to the old launch's socket is closed and replaced by one on
/// the socket the key resolves to now, whichever session it serves; a
/// revive then confirms the new socket.
#[test]
fn test_client_for_key_replaces_a_pooled_client_on_a_socket_the_key_no_longer_resolves_to() {
    let mut bed = setup();
    let key = "m-cedar.worker";
    retained_member(&mut bed, key);
    let old_sock = bed.tmp.path().join("hive/l-ab12.sock");
    let new_sock = bed.tmp.path().join("hive/l-cd34.sock");
    let _old_listener = bind_leader_socket(&old_sock);
    let _new_listener = bind_leader_socket(&new_sock);
    bind_launch("l-ab12", SID, CWD, "cedar", "123", "worker", "%3").unwrap();
    let (_spawns, old_proc) = revivable_engine();
    let pool = GrokClientPool::new();
    let old_client = pool.client_for_key(key).unwrap();
    assert_eq!(old_client.socket_path, old_sock.to_string_lossy());
    assert!(Arc::ptr_eq(&old_client, &pool.client_for_key(key).unwrap()));
    rollback_launch("l-ab12", SID, "cedar", "123", "worker", "%3").unwrap();
    bind_launch("l-cd34", SID, CWD, "cedar", "123", "worker", "%4").unwrap();
    let new_proc = FakeProc::new(Some(responder(Some(on_prompt_queue_echo()), vec![])));
    let handed = Arc::clone(&new_proc);
    set_stdio_spawn(move |_| Ok(handed.clone() as Arc<dyn LeaderProc>));
    let new_client = pool.client_for_key(key).unwrap();
    assert!(!Arc::ptr_eq(&old_client, &new_client));
    assert_eq!(new_client.socket_path, new_sock.to_string_lossy());
    assert!(!old_client.is_alive());
    assert_eq!(methods(&old_proc), vec!["initialize", "session/load"]);
    let again = pool.revive_key(key).unwrap();
    assert!(!again.raised);
    assert_eq!(again.confirmation.socket_path, new_sock.to_string_lossy());
    assert_eq!(again.confirmation.generation, new_client.generation());
    assert_eq!(
        pool.send_confirmed(&again.confirmation, "on-the-new-launch"),
        Some(PROMPT_QUEUED)
    );
    assert_eq!(prompts(&new_proc), vec!["on-the-new-launch"]);
    assert!(prompts(&old_proc).is_empty());
    teardown(&old_client, &old_proc);
    teardown(&new_client, &new_proc);
}

#[test]
fn test_zero_turn_session_is_idle_only_after_replay_completes() {
    let _bed = setup();
    let key = "m-cedar.worker";
    write_session_key(key, SID, CWD, None).unwrap();
    let (arrived, load_request) = std::sync::mpsc::channel();
    let proc = FakeProc::new(Some(Box::new(move |msg| match msg["method"].as_str() {
        Some("initialize") => vec![ok(msg, json!({"protocolVersion":1}))],
        Some("session/load") => {
            arrived.send(msg.clone()).unwrap();
            Vec::new()
        }
        _ => Vec::new(),
    })));
    let handed = Arc::clone(&proc);
    set_stdio_spawn(move |_| Ok(handed.clone() as Arc<dyn LeaderProc>));
    let client = Arc::new(GrokStdioClient::new(key).unwrap());
    let pool = GrokClientPool::new();
    pool.hold_for_test(key, client.clone());
    assert!(!client.idle_for_sleep());
    let loading = Arc::clone(&client);
    let handshake = thread::spawn(move || loading.handshake());
    let request = load_request.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(client.turn_open(), None);
    assert!(!client.idle_for_sleep());
    assert_eq!(pool.idle_owned_keys("cedar"), None);
    proc.feed(&ok(&request, json!({})));
    assert!(handshake.join().unwrap());
    assert_eq!(client.turn_open(), None);
    assert!(client.idle_for_sleep());
    assert_eq!(pool.idle_owned_keys("cedar"), Some(vec![key.into()]));
    teardown(&client, &proc);
}
