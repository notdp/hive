use super::transport::find;
use super::*;
use crate::testenv::EnvGuard;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

type SharedClientOverride = Box<dyn Fn() -> Option<Arc<dyn DaemonClient>>>;

thread_local! {
    static SHARED_CLIENT_OVERRIDE: RefCell<Option<SharedClientOverride>> = RefCell::new(None);
}

/// Some(...) when this test thread overrode `shared_client`.
pub(super) fn shared_client_override() -> Option<Option<Arc<dyn DaemonClient>>> {
    SHARED_CLIENT_OVERRIDE.with(|slot| slot.borrow().as_ref().map(|factory| factory()))
}

pub(crate) fn set_shared_client_override(
    factory: impl Fn() -> Option<Arc<dyn DaemonClient>> + 'static,
) {
    SHARED_CLIENT_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(Box::new(factory)));
}

fn override_client<T: DaemonClient + 'static>(fake: Arc<T>) {
    set_shared_client_override(move || {
        let client: Arc<dyn DaemonClient> = fake.clone();
        Some(client)
    });
}

fn bare_client() -> CodexDaemonClient {
    CodexDaemonClient::bare()
}

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

fn recording_override(
    client: &CodexDaemonClient,
    respond: impl Fn(&str) -> Value + Send + 'static,
) -> Calls {
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    client.set_call_override(move |method, params| {
        seen.lock()
            .unwrap()
            .push((method.to_string(), params.clone()));
        respond(method)
    });
    calls
}

// --- paths & records ----------------------------------------------------

#[test]
fn test_shared_socket_path_under_app_server_control() {
    let _guard = EnvGuard::cleared(&["CODEX_HOME"]);
    let path = shared_socket_path();
    assert_eq!(path.file_name().unwrap(), "hive-shared.sock");
    assert_eq!(
        path.parent().unwrap().file_name().unwrap(),
        "app-server-control"
    );
    // macOS unix socket paths cap at 104 bytes; keep headroom.
    assert!(path.to_string_lossy().len() < 104);
}

#[test]
fn test_shared_pidfile_path() {
    let _guard = EnvGuard::new();
    assert_eq!(
        shared_pidfile_path().file_name().unwrap(),
        "hive-shared.pid"
    );
}

#[test]
fn test_pane_thread_record_roundtrip() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    write_pane_thread("%19", "tid-1", "/work").unwrap();
    assert_eq!(
        read_pane_thread("%19"),
        Some(PaneThread {
            thread_id: "tid-1".to_string(),
            cwd: "/work".to_string(),
        })
    );
    assert_eq!(thread_id_for_pane("%19").as_deref(), Some("tid-1"));
    assert_eq!(session_id_for_pane("%19").as_deref(), Some("tid-1")); // threadId == sessionId
    clear_pane_thread("%19").unwrap();
    assert_eq!(read_pane_thread("%19"), None);
    clear_pane_thread("%19").unwrap(); // idempotent
}

#[test]
fn test_read_pane_thread_rejects_garbage() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    let path = pane_thread_path("%3");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "not json").unwrap();
    assert_eq!(read_pane_thread("%3"), None);
    fs::write(&path, json!({"cwd": "/x"}).to_string()).unwrap(); // no threadId
    assert_eq!(read_pane_thread("%3"), None);
}

#[test]
fn test_pane_for_thread_reverse_lookup() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    write_pane_thread("%19", "tid-a", "/work").unwrap();
    write_pane_thread("%7", "tid-b", "/work").unwrap();
    assert_eq!(pane_for_thread("tid-b").as_deref(), Some("%7"));
    assert_eq!(pane_for_thread("tid-a").as_deref(), Some("%19"));
    assert_eq!(pane_for_thread("missing"), None);
    assert_eq!(pane_for_thread(""), None);
}

#[test]
fn test_list_recorded_panes() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    write_pane_thread("%19", "t1", "/w").unwrap();
    write_pane_thread("%7", "t2", "/w").unwrap();
    fs::write(
        tmp.path()
            .join("app-server-control")
            .join("hive-pane-default.thread"),
        "{}",
    )
    .unwrap();
    let mut panes = list_recorded_panes();
    panes.sort();
    assert_eq!(panes, vec!["%19", "%7"]);
}

#[test]
fn test_list_recorded_panes_missing_dir() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    assert!(list_recorded_panes().is_empty());
}

#[test]
fn test_daemon_env_strips_pane_identity() {
    // The shared daemon serves every pane: a frozen TMUX_PANE in its env
    // would let untagged tool shells impersonate whichever pane spawned
    // it. CLAUDE*/ANTHROPIC* go too: an inherited
    // CLAUDE_CODE_MESSAGING_SOCKET resolves a codex tool shell to the
    // spawning claude engine's pane.
    let mut guard = EnvGuard::new();
    guard.set("TMUX_PANE", "%old");
    guard.set("HIVE_CODEX_PANE", "%old");
    guard.set("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/cc-socks/321.sock");
    guard.set("CLAUDE_CODE_ENTRYPOINT", "cli");
    guard.set("ANTHROPIC_API_KEY", "sk-nope");
    guard.set("CODEX_HOME", "/tmp/codex-home");
    let env_map = daemon_env();
    assert!(!env_map.contains_key("TMUX_PANE"));
    assert!(!env_map.contains_key("HIVE_CODEX_PANE"));
    assert!(!env_map
        .keys()
        .any(|key| key.starts_with("CLAUDE") || key.starts_with("ANTHROPIC")));
    assert_eq!(
        env_map.get("CODEX_HOME").map(String::as_str),
        Some("/tmp/codex-home")
    );
}

// --- status mapping -----------------------------------------------------

#[test]
fn test_apply_status_active_ready() {
    let mut rt = ThreadRuntime::default();
    apply_status(&mut rt, &json!({"type": "active", "activeFlags": []}));
    assert!(rt.busy);
    assert_eq!(rt.input_state, "ready");
}

#[test]
fn test_apply_status_active_waiting_on_user_input() {
    let mut rt = ThreadRuntime::default();
    apply_status(
        &mut rt,
        &json!({"type": "active", "activeFlags": ["waitingOnUserInput"]}),
    );
    assert_eq!(rt.input_state, "waiting_user");
}

#[test]
fn test_apply_status_active_waiting_on_approval() {
    let mut rt = ThreadRuntime::default();
    apply_status(
        &mut rt,
        &json!({"type": "active", "activeFlags": ["waitingOnApproval"]}),
    );
    assert_eq!(rt.input_state, "waiting_user");
}

#[test]
fn test_apply_status_idle() {
    let mut rt = ThreadRuntime {
        busy: true,
        ..Default::default()
    };
    apply_status(&mut rt, &json!({"type": "idle"}));
    assert!(!rt.busy);
    assert_eq!(rt.input_state, "ready");
}

#[test]
fn test_apply_status_unknown_kind_preserves_prior_fields() {
    let mut rt = ThreadRuntime {
        busy: true,
        input_state: "ready".to_string(),
        ..Default::default()
    };
    apply_status(&mut rt, &json!({"type": "systemError"}));
    assert!(rt.busy);
    assert_eq!(rt.input_state, "ready");
}

#[test]
fn test_on_notification_status_changed() {
    let client = bare_client();
    client.on_notification(
        "thread/status/changed",
        &json!({"threadId": "t1", "status": {"type": "active", "activeFlags": []}}),
    );
    assert!(client.runtime_for("t1").unwrap().busy);
    client.on_notification(
        "thread/status/changed",
        &json!({"threadId": "t1", "status": {"type": "idle"}}),
    );
    assert!(!client.runtime_for("t1").unwrap().busy);
}

#[test]
fn test_on_notification_ignores_turn_events() {
    // turn/* only reaches the turn-owning client on a shared daemon;
    // folding them here would be dead code pretending to be signal.
    let client = bare_client();
    client.on_notification(
        "turn/started",
        &json!({"threadId": "t1", "turn": {"id": "x"}}),
    );
    client.on_notification("turn/completed", &json!({"threadId": "t1"}));
    assert!(client.threads_is_empty());
}

#[test]
fn test_on_notification_ignores_missing_thread_id() {
    let client = bare_client();
    client.on_notification(
        "thread/status/changed",
        &json!({"status": {"type": "idle"}}),
    );
    assert!(client.threads_is_empty());
}

#[test]
fn test_runtime_for_returns_copy_not_reference() {
    let client = bare_client();
    client.on_notification(
        "thread/status/changed",
        &json!({"threadId": "t1", "status": {"type": "idle"}}),
    );
    let mut snap = client.runtime_for("t1").unwrap();
    snap.busy = true;
    assert!(!client.runtime_for("t1").unwrap().busy); // internal state untouched
}

// --- resume backfill ----------------------------------------------------

#[test]
fn test_resume_backfills_active_runtime_from_thread_status() {
    // Late-join recovery: resume must seed _threads from the thread's
    // status so runtime reads report native busy/inputState instead of None.
    let client = bare_client();
    client.set_call_override(|_method, _params| {
        json!({
            "result": {"thread": {"sessionId": "s", "status": {"type": "active", "activeFlags": []}}}
        })
    });
    assert!(client.resume("t1"));
    let rt = client.runtime_for("t1");
    assert!(rt.is_some() && rt.unwrap().busy);
}

#[test]
fn test_resume_backfills_idle_runtime_from_thread_status() {
    let client = bare_client();
    client.set_call_override(|_method, _params| {
        json!({"result": {"thread": {"sessionId": "s", "status": {"type": "idle"}}}})
    });
    assert!(client.resume("t1"));
    let rt = client.runtime_for("t1").unwrap();
    assert!(!rt.busy);
    assert_eq!(rt.input_state, "ready");
}

#[test]
fn test_resume_returns_false_on_error() {
    let client = bare_client();
    client.set_call_override(|_method, _params| json!({"__error__": "no rollout found"}));
    assert!(!client.resume("t1"));
    assert!(client.threads_is_empty());
}

#[test]
fn test_attach_resumes_each_loaded_thread() {
    let client = bare_client();
    let calls = recording_override(&client, |method| {
        if method == "thread/loaded/list" {
            json!({"result": {"data": ["t1", "t2"]}})
        } else {
            json!({"result": {}})
        }
    });
    client.attach();
    let seen: Vec<String> = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(method, _)| method == "thread/resume")
        .map(|(_, params)| params["threadId"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(seen, vec!["t1", "t2"]);
}

#[test]
fn test_runtime_or_backfill_resumes_once_per_cooldown() {
    let client = bare_client();
    // keep the runtime missing: every resume answers with an error
    let calls = recording_override(&client, |_method| json!({"__error__": "missing"}));
    assert!(client.runtime_or_backfill("t1").is_none());
    assert!(client.runtime_or_backfill("t1").is_none()); // inside cooldown: no 2nd resume
    let resumes = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(method, _)| method == "thread/resume")
        .count();
    assert_eq!(resumes, 1);
}

#[test]
fn test_runtime_or_backfill_returns_backfilled_state() {
    let client = bare_client();
    client.set_call_override(
        |_method, _params| json!({"result": {"thread": {"status": {"type": "idle"}}}}),
    );
    let rt = client.runtime_or_backfill("t1").unwrap();
    assert!(!rt.busy);
    assert_eq!(rt.input_state, "ready");
}

// --- mint / fork protocol -----------------------------------------------

/// A fake daemon for the mint: `thread/start` (or `thread/fork`) answers
/// with a thread whose rollout path is *rollout*, and `thread/section/move`
/// materializes that file the way the real daemon does. Everything else is
/// an empty success.
fn minting_daemon(
    start_method: &'static str,
    thread: Value,
    rollout: PathBuf,
    materializes: bool,
) -> impl Fn(&str) -> Value + Send + 'static {
    move |method| {
        if method == start_method {
            let mut thread = thread.clone();
            thread["path"] = json!(rollout.to_string_lossy());
            json!({"result": {"thread": thread}})
        } else {
            if method == "thread/section/move" && materializes {
                fs::write(&rollout, "{}\n").unwrap();
            }
            json!({"result": {}})
        }
    }
}

#[test]
fn test_start_thread_mints_and_flushes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let rollout = tmp.path().join("rollout-tid-new.jsonl");
    let client = bare_client();
    let calls = recording_override(
        &client,
        minting_daemon(
            "thread/start",
            json!({"id": "tid-new", "status": {"type": "idle"}}),
            rollout.clone(),
            true,
        ),
    );
    assert_eq!(
        client
            .start_thread("/work", "honey.val", "gpt-x")
            .as_deref(),
        Some("tid-new")
    );
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls[0],
        (
            "thread/start".to_string(),
            json!({"cwd": "/work", "model": "gpt-x"})
        )
    );
    // the flush: name (metadata only), then a null-section placement, which
    // is what makes the daemon write the rollout ahead of the first turn —
    // the pane TUI's paginated resume reads that file (real-machine verified)
    assert_eq!(
        calls[1],
        (
            "thread/name/set".to_string(),
            json!({"threadId": "tid-new", "name": "honey.val"})
        )
    );
    assert_eq!(
        calls[2],
        (
            "thread/section/move".to_string(),
            json!({"threadId": "tid-new", "sectionId": null})
        )
    );
    assert_eq!(calls.len(), 3);
    assert!(rollout.is_file());
    // the mint seeds the runtime so a fresh member reads idle, not unknown
    assert!(client.runtime_for("tid-new").is_some());
}

#[test]
fn test_start_thread_fails_when_the_placement_is_refused() {
    // a thread whose rollout never materialized is not attachable: the
    // spawn must fail rather than hand out a thread id the TUI cannot resume
    let tmp = tempfile::TempDir::new().unwrap();
    let rollout = tmp.path().join("rollout-tid-new.jsonl");
    let client = bare_client();
    let _calls = recording_override(&client, move |method| match method {
        "thread/start" => json!({"result": {"thread": {
            "id": "tid-new", "status": {"type": "idle"}, "path": rollout.to_string_lossy()
        }}}),
        "thread/section/move" => {
            json!({"__error__": "ephemeral thread does not support section moves"})
        }
        _ => json!({"result": {}}),
    });
    assert_eq!(client.start_thread("/work", "honey.val", "gpt-x"), None);
}

#[test]
fn test_start_thread_fails_when_the_rollout_never_appears() {
    // every call succeeds but no file lands: whichever call is supposed to
    // materialize the rollout, the file on disk is the contract the TUI needs
    let tmp = tempfile::TempDir::new().unwrap();
    let rollout = tmp.path().join("rollout-tid-new.jsonl");
    let client = bare_client();
    let _calls = recording_override(
        &client,
        minting_daemon(
            "thread/start",
            json!({"id": "tid-new", "status": {"type": "idle"}}),
            rollout.clone(),
            false,
        ),
    );
    assert_eq!(client.start_thread("/work", "honey.val", "gpt-x"), None);
    assert!(!rollout.exists());
}

#[test]
fn test_start_thread_fails_without_a_rollout_path() {
    // a daemon that reports no path leaves nothing to verify: refuse rather
    // than trust a thread the TUI may not find
    let client = bare_client();
    let _calls = recording_override(&client, |method| {
        if method == "thread/start" {
            json!({"result": {"thread": {"id": "tid-new", "status": {"type": "idle"}}}})
        } else {
            json!({"result": {}})
        }
    });
    assert_eq!(client.start_thread("/work", "honey.val", "gpt-x"), None);
}

#[test]
fn test_start_thread_without_model_omits_param() {
    let tmp = tempfile::TempDir::new().unwrap();
    let rollout = tmp.path().join("rollout-t.jsonl");
    let client = bare_client();
    let calls = recording_override(
        &client,
        minting_daemon("thread/start", json!({"id": "t"}), rollout, true),
    );
    assert_eq!(client.start_thread("/work", "n", "").as_deref(), Some("t"));
    let calls = calls.lock().unwrap();
    let (_, start_params) = calls
        .iter()
        .find(|(method, _)| method == "thread/start")
        .unwrap();
    assert!(start_params.get("model").is_none());
}

#[test]
fn test_start_thread_fails_when_flush_fails() {
    // An unflushed thread is not attachable by the TUI; minting must not
    // report success for a thread `codex resume` would refuse.
    let client = bare_client();
    client.set_call_override(|method, _params| {
        if method == "thread/start" {
            json!({"result": {"thread": {"id": "t", "path": "/nonexistent/rollout-t.jsonl"}}})
        } else {
            json!({"__error__": "boom"})
        }
    });
    assert_eq!(client.start_thread("/work", "n", ""), None);
}

#[test]
fn test_start_thread_fails_on_rpc_error() {
    let client = bare_client();
    client.set_call_override(|_method, _params| json!({"__error__": "nope"}));
    assert_eq!(client.start_thread("/work", "n", ""), None);
}

#[test]
fn test_fork_thread_returns_fork_id_and_flushes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let rollout = tmp.path().join("rollout-tid-fork.jsonl");
    let client = bare_client();
    let calls = recording_override(
        &client,
        minting_daemon(
            "thread/fork",
            json!({"id": "tid-fork", "forkedFromId": "tid-src"}),
            rollout.clone(),
            true,
        ),
    );
    assert_eq!(
        client.fork_thread("tid-src", "clone").as_deref(),
        Some("tid-fork")
    );
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls[0],
        ("thread/fork".to_string(), json!({"threadId": "tid-src"}))
    );
    assert_eq!(
        calls[1],
        (
            "thread/name/set".to_string(),
            json!({"threadId": "tid-fork", "name": "clone"})
        )
    );
    assert_eq!(
        calls[2],
        (
            "thread/section/move".to_string(),
            json!({"threadId": "tid-fork", "sectionId": null})
        )
    );
    assert!(rollout.is_file());
}

#[test]
fn test_fork_thread_fails_on_rpc_error() {
    let client = bare_client();
    client.set_call_override(|_method, _params| json!({"__error__": "no rollout found"}));
    assert_eq!(client.fork_thread("tid-src", "clone"), None);
}

// --- pane-keyed API over the shared client ------------------------------

fn record(guard: &mut EnvGuard, tmp: &Path, pane: &str, tid: &str) {
    guard.set("CODEX_HOME", tmp);
    write_pane_thread(pane, tid, "/work").unwrap();
}

#[test]
fn test_send_to_pane_turn_starts_even_when_busy() {
    // Busy is not bounced to the composer: turn/start carries steer
    // semantics in core, so hive hands a busy thread straight to the RPC.
    // The fake deliberately omits runtime methods: send_to_pane must not
    // consult them.
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    record(&mut guard, tmp.path(), "%1", "t1");

    struct FakeClient {
        sent: Mutex<Vec<(String, String)>>,
    }
    impl DaemonClient for FakeClient {
        fn turn_start(&self, tid: &str, text: &str) -> Result<Value, String> {
            self.sent
                .lock()
                .unwrap()
                .push((tid.to_string(), text.to_string()));
            Ok(json!({"result": {}}))
        }
    }
    let fake = Arc::new(FakeClient {
        sent: Mutex::new(Vec::new()),
    });
    override_client(fake.clone());
    assert_eq!(send_to_pane("%1", "hi"), Some(TURN_START_ACCEPTED));
    assert_eq!(
        *fake.sent.lock().unwrap(),
        vec![("t1".to_string(), "hi".to_string())]
    );
}

#[test]
fn test_send_to_pane_fails_without_record() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    set_shared_client_override(|| -> Option<Arc<dyn DaemonClient>> {
        panic!("no record -> the daemon must not even be dialed")
    });
    assert_eq!(send_to_pane("%1", "hi"), None);
}

#[test]
fn test_send_to_pane_fails_without_daemon() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    record(&mut guard, tmp.path(), "%1", "t1");
    set_shared_client_override(|| None);
    assert_eq!(send_to_pane("%1", "hi"), None);
}

#[test]
fn test_send_to_pane_fails_on_rpc_error_response() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    record(&mut guard, tmp.path(), "%1", "t1");

    struct FakeClient;
    impl DaemonClient for FakeClient {
        fn turn_start(&self, _tid: &str, _text: &str) -> Result<Value, String> {
            Ok(json!({"error": {"code": -1, "message": "boom"}}))
        }
    }
    override_client(Arc::new(FakeClient));
    assert_eq!(send_to_pane("%1", "hi"), None);
}

#[test]
fn test_send_to_pane_fails_on_rpc_exception() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    record(&mut guard, tmp.path(), "%1", "t1");

    struct FakeClient;
    impl DaemonClient for FakeClient {
        fn turn_start(&self, _tid: &str, _text: &str) -> Result<Value, String> {
            Err("socket reset".to_string())
        }
    }
    override_client(Arc::new(FakeClient));
    assert_eq!(send_to_pane("%1", "hi"), None);
}

#[test]
fn test_runtime_for_pane_reads_recorded_thread() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    record(&mut guard, tmp.path(), "%1", "t1");

    struct FakeClient;
    impl DaemonClient for FakeClient {
        fn runtime_or_backfill(&self, tid: &str) -> Option<ThreadRuntime> {
            assert_eq!(tid, "t1");
            Some(ThreadRuntime {
                busy: true,
                ..Default::default()
            })
        }
    }
    override_client(Arc::new(FakeClient));
    let rt = runtime_for_pane("%1");
    assert!(rt.is_some() && rt.unwrap().busy);
}

#[test]
fn test_runtime_for_pane_none_without_record() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    set_shared_client_override(|| -> Option<Arc<dyn DaemonClient>> {
        panic!("no record -> no daemon dial")
    });
    assert_eq!(runtime_for_pane("%1"), None);
}

#[test]
fn test_compact_pane_compacts_when_idle() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    record(&mut guard, tmp.path(), "%1", "t1");

    struct FakeClient {
        started: Mutex<Vec<String>>,
    }
    impl DaemonClient for FakeClient {
        fn runtime_or_backfill(&self, _tid: &str) -> Option<ThreadRuntime> {
            Some(ThreadRuntime::default())
        }
        fn compact_start(&self, tid: &str) -> Value {
            self.started.lock().unwrap().push(tid.to_string());
            json!({"result": {}})
        }
    }
    let fake = Arc::new(FakeClient {
        started: Mutex::new(Vec::new()),
    });
    override_client(fake.clone());
    assert_eq!(compact_pane("%1"), "compacted");
    assert_eq!(*fake.started.lock().unwrap(), vec!["t1".to_string()]);
}

#[test]
fn test_compact_pane_busy_defers_without_aborting_turn() {
    // A Compact turn aborts any running turn, so a busy agent must never
    // be compacted out from under its in-flight work.
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    record(&mut guard, tmp.path(), "%1", "t1");

    struct FakeClient;
    impl DaemonClient for FakeClient {
        fn runtime_or_backfill(&self, _tid: &str) -> Option<ThreadRuntime> {
            Some(ThreadRuntime {
                busy: true,
                ..Default::default()
            })
        }
        fn compact_start(&self, _tid: &str) -> Value {
            panic!("must not compact a busy agent (would abort its turn)")
        }
    }
    override_client(Arc::new(FakeClient));
    assert_eq!(compact_pane("%1"), "busy");
}

#[test]
fn test_compact_pane_unavailable_without_record() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    assert_eq!(compact_pane("%1"), "unavailable");
}

// --- interrupt ----------------------------------------------------------

/// A client whose thread/read answers with *turns*, recording every call.
fn client_reading(turns: Value) -> (CodexDaemonClient, Calls) {
    let client = bare_client();
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    client.set_call_override(move |method, params| {
        seen.lock()
            .unwrap()
            .push((method.to_string(), params.clone()));
        json!({"result": {"thread": {"turns": turns}}})
    });
    (client, calls)
}

#[test]
fn test_active_turn_id_reads_the_in_progress_turn() {
    let (client, calls) = client_reading(json!([
        {"id": "old", "status": "completed"},
        {"id": "live", "status": "inProgress"},
    ]));
    assert_eq!(
        client.active_turn_id("t1").unwrap().as_deref(),
        Some("live")
    );
    assert_eq!(
        *calls.lock().unwrap(),
        vec![(
            "thread/read".to_string(),
            json!({"threadId": "t1", "includeTurns": true})
        )]
    );
}

#[test]
fn test_active_turn_id_none_when_every_turn_is_finished() {
    let (client, _calls) = client_reading(json!([{"id": "old", "status": "completed"}]));
    assert_eq!(client.active_turn_id("t1"), Ok(None));
}

#[test]
fn test_active_turn_id_errs_without_turns() {
    // `includeTurns` makes thread.turns part of the answer; a result without
    // it is a schema error, not a thread with nothing in progress.
    let client = bare_client();
    client.set_call_override(|_method, _params| json!({"result": {"thread": {"id": "t1"}}}));
    assert!(client
        .active_turn_id("t1")
        .unwrap_err()
        .contains("thread.turns"));
}

#[test]
fn test_active_turn_id_errs_when_the_in_progress_turn_has_no_id() {
    let (client, _calls) = client_reading(json!([{"status": "inProgress"}]));
    assert!(client
        .active_turn_id("t1")
        .unwrap_err()
        .contains("without an id"));
    let (client, _calls) = client_reading(json!([{"id": "", "status": "inProgress"}]));
    assert!(client.active_turn_id("t1").is_err());
}

#[test]
fn test_active_turn_id_errs_on_rpc_error() {
    // No result is no answer, distinct from "no turn is open".
    let client = bare_client();
    client.set_call_override(|_method, _params| json!({"__error__": "boom"}));
    assert!(client.active_turn_id("t1").unwrap_err().contains("boom"));
}

#[test]
fn test_turn_open_for_thread_reads_the_daemons_answer() {
    struct FakeClient {
        answer: Result<Option<String>, String>,
    }
    impl DaemonClient for FakeClient {
        fn active_turn_id(&self, tid: &str) -> Result<Option<String>, String> {
            assert_eq!(tid, "t1");
            self.answer.clone()
        }
    }
    override_client(Arc::new(FakeClient {
        answer: Ok(Some("live".to_string())),
    }));
    assert_eq!(turn_open_for_thread("t1"), Some(true));
    override_client(Arc::new(FakeClient { answer: Ok(None) }));
    assert_eq!(turn_open_for_thread("t1"), Some(false));
    // An RPC error is no answer, never "idle".
    override_client(Arc::new(FakeClient {
        answer: Err("boom".to_string()),
    }));
    assert_eq!(turn_open_for_thread("t1"), None);
}

#[test]
fn test_turn_open_for_thread_none_without_daemon() {
    set_shared_client_override(|| None);
    assert_eq!(turn_open_for_thread("t1"), None);
}

#[test]
fn test_turn_interrupt_carries_thread_and_turn_id() {
    // The turnId is mandatory on this RPC and is checked against the live
    // turn, so it must be passed through verbatim.
    let client = bare_client();
    let calls = recording_override(&client, |_method| json!({"result": {}}));
    assert!(client.turn_interrupt("t1", "live").get("result").is_some());
    assert_eq!(
        *calls.lock().unwrap(),
        vec![(
            "turn/interrupt".to_string(),
            json!({"threadId": "t1", "turnId": "live"})
        )]
    );
}

#[test]
fn test_interrupt_pane_aborts_the_running_turn() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    record(&mut guard, tmp.path(), "%1", "t1");

    struct FakeClient {
        aborted: Mutex<Vec<(String, String)>>,
    }
    impl DaemonClient for FakeClient {
        fn active_turn_id(&self, tid: &str) -> Result<Option<String>, String> {
            assert_eq!(tid, "t1");
            Ok(Some("live".to_string()))
        }
        fn turn_interrupt(&self, tid: &str, turn_id: &str) -> Result<Value, String> {
            self.aborted
                .lock()
                .unwrap()
                .push((tid.to_string(), turn_id.to_string()));
            Ok(json!({"result": {}}))
        }
    }
    let fake = Arc::new(FakeClient {
        aborted: Mutex::new(Vec::new()),
    });
    override_client(fake.clone());
    assert_eq!(interrupt_pane("%1"), Some(TURN_INTERRUPT_ACCEPTED));
    assert_eq!(
        *fake.aborted.lock().unwrap(),
        vec![("t1".to_string(), "live".to_string())]
    );
}

#[test]
fn test_interrupt_pane_reports_an_idle_thread_without_interrupting() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    record(&mut guard, tmp.path(), "%1", "t1");

    struct FakeClient;
    impl DaemonClient for FakeClient {
        fn active_turn_id(&self, _tid: &str) -> Result<Option<String>, String> {
            Ok(None)
        }
        fn turn_interrupt(&self, _tid: &str, _turn_id: &str) -> Result<Value, String> {
            panic!("no running turn -> nothing to interrupt")
        }
    }
    override_client(Arc::new(FakeClient));
    assert_eq!(interrupt_pane("%1"), Some(NO_RUNNING_TURN));
}

#[test]
fn test_interrupt_pane_fails_without_record() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    set_shared_client_override(|| -> Option<Arc<dyn DaemonClient>> {
        panic!("no record -> the daemon must not even be dialed")
    });
    assert_eq!(interrupt_pane("%1"), None);
}

#[test]
fn test_interrupt_pane_fails_without_daemon() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    record(&mut guard, tmp.path(), "%1", "t1");
    set_shared_client_override(|| None);
    assert_eq!(interrupt_pane("%1"), None);
}

#[test]
fn test_interrupt_pane_fails_on_rpc_error_response() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    record(&mut guard, tmp.path(), "%1", "t1");

    struct FakeClient;
    impl DaemonClient for FakeClient {
        fn active_turn_id(&self, _tid: &str) -> Result<Option<String>, String> {
            Ok(Some("live".to_string()))
        }
        fn turn_interrupt(&self, _tid: &str, _turn_id: &str) -> Result<Value, String> {
            Ok(json!({"__error__": {"code": -32600, "message": "expected active turn id"}}))
        }
    }
    override_client(Arc::new(FakeClient));
    assert_eq!(interrupt_pane("%1"), None);
}

#[test]
fn test_interrupt_pane_fails_on_rpc_exception() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    record(&mut guard, tmp.path(), "%1", "t1");

    struct FakeClient;
    impl DaemonClient for FakeClient {
        fn active_turn_id(&self, _tid: &str) -> Result<Option<String>, String> {
            Err("socket reset".to_string())
        }
    }
    override_client(Arc::new(FakeClient));
    assert_eq!(interrupt_pane("%1"), None);
}

#[test]
fn test_connect_true_when_client_established() {
    struct FakeClient;
    impl DaemonClient for FakeClient {}
    override_client(Arc::new(FakeClient));
    assert!(connect());
}

#[test]
fn test_connect_false_when_no_daemon() {
    set_shared_client_override(|| None);
    assert!(!connect());
}

#[test]
fn test_start_member_thread_delegates_to_client() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path()); // freshen must not touch the real cache

    struct FakeClient;
    impl DaemonClient for FakeClient {
        fn start_thread(&self, cwd: &str, name: &str, model: &str) -> Option<String> {
            if (cwd, name, model) == ("/w", "n", "m") {
                Some("tid-x".to_string())
            } else {
                None
            }
        }
    }
    override_client(Arc::new(FakeClient));
    assert_eq!(
        start_member_thread("/w", "n", "m").as_deref(),
        Some("tid-x")
    );
    set_shared_client_override(|| None);
    assert_eq!(start_member_thread("/w", "n", ""), None);
}

#[test]
fn test_fork_member_thread_delegates_to_client() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());

    struct FakeClient;
    impl DaemonClient for FakeClient {
        fn fork_thread(&self, tid: &str, name: &str) -> Option<String> {
            if (tid, name) == ("src", "n") {
                Some("tid-f".to_string())
            } else {
                None
            }
        }
    }
    override_client(Arc::new(FakeClient));
    assert_eq!(fork_member_thread("src", "n").as_deref(), Some("tid-f"));
    set_shared_client_override(|| None);
    assert_eq!(fork_member_thread("src", "n"), None);
}

// --- directory trust ----------------------------------------------------

#[test]
fn test_ensure_dir_trusted_creates_config() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    ensure_dir_trusted("/work/dir").unwrap();
    let text = fs::read_to_string(tmp.path().join("config.toml")).unwrap();
    assert!(text.contains("[projects.\"/work/dir\"]"));
    assert!(text.contains("trust_level = \"trusted\""));
}

#[test]
fn test_ensure_dir_trusted_appends_to_existing_config() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    let config = tmp.path().join("config.toml");
    fs::write(&config, "model = \"gpt-x\"\n").unwrap();
    ensure_dir_trusted("/work/dir").unwrap();
    let text = fs::read_to_string(&config).unwrap();
    assert!(text.starts_with("model = \"gpt-x\"\n"));
    assert!(text.contains("[projects.\"/work/dir\"]\ntrust_level = \"trusted\""));
}

#[test]
fn test_ensure_dir_trusted_idempotent() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    let config = tmp.path().join("config.toml");
    ensure_dir_trusted("/work/dir").unwrap();
    let first = fs::read_to_string(&config).unwrap();
    let before = fs::metadata(&config).unwrap().modified().unwrap();
    ensure_dir_trusted("/work/dir").unwrap();
    assert_eq!(fs::read_to_string(&config).unwrap(), first);
    assert_eq!(fs::metadata(&config).unwrap().modified().unwrap(), before); // no rewrite on no-op
}

#[test]
fn test_ensure_dir_trusted_upgrades_existing_entry() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    let config = tmp.path().join("config.toml");
    fs::write(
        &config,
        "[projects.\"/work/dir\"]\ntrust_level = \"untrusted\"\n\n[other]\nk = 1\n",
    )
    .unwrap();
    ensure_dir_trusted("/work/dir").unwrap();
    let text = fs::read_to_string(&config).unwrap();
    assert!(text.contains("trust_level = \"trusted\""));
    assert!(!text.contains("trust_level = \"untrusted\""));
    assert_eq!(text.matches("[projects.\"/work/dir\"]").count(), 1); // no duplicate table
    assert!(text.contains("[other]"));
}

#[test]
fn test_ensure_dir_trusted_adds_missing_key_to_existing_section() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    let config = tmp.path().join("config.toml");
    fs::write(&config, "[projects.\"/work/dir\"]\nother = 1\n").unwrap();
    ensure_dir_trusted("/work/dir").unwrap();
    let text = fs::read_to_string(&config).unwrap();
    assert_eq!(text.matches("[projects.\"/work/dir\"]").count(), 1);
    assert!(text.contains("trust_level = \"trusted\""));
    assert!(text.contains("other = 1"));
}

#[test]
fn test_ensure_dir_trusted_escapes_quotes() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    ensure_dir_trusted("/work/we\"ird").unwrap();
    let text = fs::read_to_string(tmp.path().join("config.toml")).unwrap();
    assert!(text.contains("[projects.\"/work/we\\\"ird\"]"));
}

#[test]
fn test_ensure_dir_trusted_matches_literal_string_header() {
    // A hand-edited literal-string header must not gain a duplicate table
    // — duplicate tables make the whole config.toml unparsable.
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    let config = tmp.path().join("config.toml");
    fs::write(
        &config,
        "[projects.'/work/dir']\ntrust_level = \"trusted\"\n",
    )
    .unwrap();
    ensure_dir_trusted("/work/dir").unwrap();
    let text = fs::read_to_string(&config).unwrap();
    assert_eq!(text.matches("/work/dir").count(), 1);
}

// --- transport: reader must survive daemon silence ----------------------

#[test]
fn test_wsconn_disarms_the_handshake_timeout_once_connected() {
    // The handshake timeout must not stay armed on post-handshake reads.
    //
    // Guards the mint-hang regression: the daemon legally goes silent for
    // 5.00s mid thread/start (its models refresh stalls on a stale cache),
    // and an armed 5.0s socket timeout killed the reader right before the
    // response.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ws.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let (release, gate) = mpsc::channel::<()>();
    let server = thread::spawn(move || {
        let (mut conn, _addr) = listener.accept().unwrap();
        let mut data: Vec<u8> = Vec::new();
        while find(&data, b"\r\n\r\n").is_none() {
            let mut buf = [0u8; 4096];
            let n = conn.read(&mut buf).unwrap();
            data.extend_from_slice(&buf[..n]);
        }
        conn.write_all(b"HTTP/1.1 101 Switching Protocols\r\n\r\n")
            .unwrap();
        gate.recv().unwrap(); // stay silent until the client has been inspected
        let payload = br#"{"id":1,"result":{}}"#;
        let mut frame = vec![0x81u8, payload.len() as u8];
        frame.extend_from_slice(payload);
        conn.write_all(&frame).unwrap();
    });
    let mut conn = WsConn::connect(&path, Duration::from_millis(300)).unwrap();
    // connect armed the timeout for the handshake and disarmed it after
    assert_eq!(conn.stream.read_timeout().unwrap(), None);
    assert_eq!(conn.stream.write_timeout().unwrap(), None);
    release.send(()).unwrap();
    let txt = conn.recv_text().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&txt).unwrap(),
        json!({"id": 1, "result": {}})
    );
    conn.close();
    server.join().unwrap();
}

// --- models cache freshening --------------------------------------------

#[test]
fn test_freshen_models_cache_renews_stamp_and_keeps_data() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    let path = tmp.path().join("models_cache.json");
    fs::write(
        &path,
        json!({
            "fetched_at": "2026-08-26T05:00:00.000000Z",
            "etag": "W/\"abc\"",
            "client_version": "0.149.1",
            "models": [{"slug": "m1"}],
        })
        .to_string(),
    )
    .unwrap();
    assert!(freshen_models_cache());
    let entry: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_ne!(entry["fetched_at"], json!("2026-08-26T05:00:00.000000Z"));
    assert!(entry["fetched_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(entry["etag"], json!("W/\"abc\""));
    assert_eq!(entry["models"], json!([{"slug": "m1"}]));
}

#[test]
fn test_freshen_models_cache_tolerates_missing_and_garbage() {
    let mut guard = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    guard.set("CODEX_HOME", tmp.path());
    assert!(!freshen_models_cache()); // no file
    fs::write(tmp.path().join("models_cache.json"), "not json").unwrap();
    assert!(!freshen_models_cache());
}
