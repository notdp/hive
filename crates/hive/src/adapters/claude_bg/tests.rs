use super::attach::{
    attach_pipe, close_pipe, engine_screen_size, wait_client_ready, wait_engine_behind, Client,
};
use super::testhook::{FakePipe, Hook};
use super::*;
use crate::testenv::EnvGuard;
use serde_json::json;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

// --- fixtures -----------------------------------------------------------

/// An isolated claude config tree: CLAUDE_HOME pointed at a tempdir for the
/// test's lifetime.
struct Home {
    config: PathBuf,
    dir: tempfile::TempDir,
    env: EnvGuard,
}

fn claude_home() -> Home {
    let mut env = EnvGuard::cleared(&crate::testenv::CLAUDE_VARS);
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("claude-home");
    env.set("CLAUDE_HOME", &config);
    Home { config, dir, env }
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

/// The parts of a `wire` that most tests leave at their defaults.
///
/// *engine* is what the attach finds behind the pipe (an idle cafe1234 by
/// default). *baseline* is what the screen shows before anything is typed
/// — the pipe reads it first and only counts an echo that was not already
/// there. *draft* is what the dim-aware composer parser reports before
/// the C-u.
struct Wire<'a> {
    engine: Option<EngineSession>,
    baseline: &'a str,
    draft: bool,
}

impl Default for Wire<'_> {
    fn default() -> Self {
        Wire {
            engine: None,
            baseline: "> ",
            draft: false,
        }
    }
}

/// Attach *pipe*, feed the screen from *screens*, transcript from a file.
fn wire(
    hook: &mut Hook,
    pipe: &FakePipe,
    screens: &[&str],
    transcript: Option<PathBuf>,
    opts: Wire,
) {
    hook.attach_pipe = Some(pipe.clone());
    hook.client_ready = Some(true);
    hook.wait_engine_behind = Some(Some(
        opts.engine
            .unwrap_or_else(|| fake_engine("cafe1234", "idle")),
    ));
    hook.transcript_cursor = Some((transcript, 0));
    hook.composer_draft = Some(opts.draft);
    hook.no_sleep = true;
    let mut st = pipe.state.lock().unwrap();
    st.stream = opts.baseline.to_string();
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
        Some(PaneJob {
            job_id: "cafe1234".into(),
            session_id: "sess-19".into(),
            cwd: "/w/a".into(),
        })
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
    let hook = Hook {
        list_jobs_rows: Some(Some(rows)),
        ..Default::default()
    };
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
    let mut home = claude_home();
    home.env.set("CLAUDE_CODE_CHILD_SESSION", "1");
    home.env.set("ANTHROPIC_MODEL", "x");
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
        b"backgrounded \xc2\xb7 not-a-job-id \xc2\xb7 probe\n",  // reworded / renamed announcement
        b"started probe in the background\n",                    // no announcement at all
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
    let hook = Hook {
        engine_for_job: Some(VecDeque::from(vec![
            None,
            Some(fake_engine("cafe1234", "idle")),
        ])),
        wake_result: Some(true),
        no_sleep: true,
        ..Default::default()
    };
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
    let hook = Hook {
        engine_for_job: Some(VecDeque::from(vec![None])),
        wake_result: Some(false),
        no_sleep: true,
        ..Default::default()
    };
    let _g = testhook::install(hook);

    assert!(ensure_engine("cafe1234", Some(0.0), "claude").is_none());
}

// --- the waits behind an attach -----------------------------------------

/// A fake client whose `poll` answers *exit* (None: still running).
fn client(exit: Option<i32>) -> (FakePipe, Client) {
    let pipe = FakePipe::default();
    pipe.state.lock().unwrap().poll = exit;
    (pipe.clone(), Client::Fake(pipe))
}

/// What is left of the scripted `engine_for_job` answers: the hook pops one
/// per poll while more than one remains, so the length counts the polls.
fn engine_queue() -> Vec<Option<EngineSession>> {
    testhook::with(|h| h.engine_for_job.clone().unwrap().into_iter().collect()).unwrap()
}

#[test]
fn test_wait_engine_behind_polls_until_the_entry_appears() {
    // Two misses, then the entry: the wait keeps polling while the client
    // lives, and the answer is the entry the third poll found.
    let hook = Hook {
        engine_for_job: Some(VecDeque::from(vec![
            None,
            None,
            Some(fake_engine("cafe1234", "idle")),
        ])),
        no_sleep: true,
        ..Default::default()
    };
    let _g = testhook::install(hook);
    let (_pipe, mut proc) = client(None);

    let engine = wait_engine_behind("cafe1234", &mut proc).unwrap();

    assert_eq!(engine.job_id, "cafe1234");
    assert_eq!(engine_queue().len(), 1, "both misses were consumed");
}

#[test]
fn test_wait_engine_behind_gives_up_once_the_client_exits() {
    // The entry is one more poll away, but the client already exited: the
    // wait stops on the first miss instead of finding it.
    let hook = Hook {
        engine_for_job: Some(VecDeque::from(vec![
            None,
            None,
            Some(fake_engine("cafe1234", "idle")),
        ])),
        no_sleep: true,
        ..Default::default()
    };
    let _g = testhook::install(hook);
    let (_pipe, mut proc) = client(Some(1));

    assert_eq!(wait_engine_behind("cafe1234", &mut proc), None);
    assert_eq!(engine_queue().len(), 2, "polled exactly once");
}

#[test]
fn test_wait_engine_behind_times_out_with_the_client_still_alive() {
    let hook = Hook {
        engine_for_job: Some(VecDeque::from(vec![
            None,
            None,
            Some(fake_engine("cafe1234", "idle")),
        ])),
        engine_ready_timeout: Some(0.0),
        no_sleep: true,
        ..Default::default()
    };
    let _g = testhook::install(hook);
    let (_pipe, mut proc) = client(None);

    assert_eq!(wait_engine_behind("cafe1234", &mut proc), None);
    assert_eq!(engine_queue().len(), 2, "polled exactly once");
}

/// A live attach-journal entry naming *pid*, with the procStart the journal
/// check verifies against `ps`.
fn attach_journal_entry(home: &Home, pid: i32) {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .unwrap();
    let proc_start = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(!proc_start.is_empty(), "ps knows pid {pid}");
    let dir = home.config.join("daemon").join("attach-journal");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("gesture.json"),
        json!({"gestureId": "gesture", "surface": "bg_cli", "pid": pid, "procStart": proc_start})
            .to_string(),
    )
    .unwrap();
}

#[test]
fn test_wait_client_ready_once_the_journal_names_the_client() {
    let home = claude_home();
    let hook = Hook {
        no_sleep: true,
        ..Default::default()
    };
    let _g = testhook::install(hook);
    let (pipe, mut proc) = client(None);
    pipe.state.lock().unwrap().pid = Some(me() as i32);
    attach_journal_entry(&home, me() as i32);

    assert!(wait_client_ready(&mut proc));
}

#[test]
fn test_wait_client_ready_is_false_once_the_client_exits() {
    // The journal entry alone would say ready; an exited client wins.
    let home = claude_home();
    let hook = Hook {
        no_sleep: true,
        ..Default::default()
    };
    let _g = testhook::install(hook);
    let (pipe, mut proc) = client(Some(0));
    pipe.state.lock().unwrap().pid = Some(me() as i32);
    attach_journal_entry(&home, me() as i32);

    assert!(!wait_client_ready(&mut proc));
}

#[test]
fn test_wait_client_ready_times_out_on_a_journal_naming_someone_else() {
    let home = claude_home();
    let hook = Hook {
        client_ready_timeout: Some(0.05),
        no_sleep: true,
        ..Default::default()
    };
    let _g = testhook::install(hook);
    let (pipe, mut proc) = client(None);
    pipe.state.lock().unwrap().pid = Some(me() as i32 + 1);
    attach_journal_entry(&home, me() as i32);

    assert!(!wait_client_ready(&mut proc));
}

#[test]
fn test_engine_screen_size_is_the_bound_panes_size() {
    let _home = claude_home();
    write_pane_job("%5", "cafe1234", "sid", "/w").unwrap();
    let argv: std::rc::Rc<std::cell::RefCell<Vec<Vec<String>>>> = Default::default();
    let recorded = std::rc::Rc::clone(&argv);
    let answers = std::rc::Rc::new(std::cell::RefCell::new(VecDeque::from(vec![
        "132\t43".to_string(),
        "wide\ttall".to_string(),
    ])));
    let script = std::rc::Rc::clone(&answers);
    crate::tmux::set_run_override(move |args, _check, _timeout| {
        recorded.borrow_mut().push(args.to_vec());
        Ok(crate::tmux::Run {
            returncode: 0,
            stdout: script.borrow_mut().pop_front().unwrap_or_default(),
            stderr: String::new(),
        })
    });

    assert_eq!(engine_screen_size("cafe1234"), (132, 43));
    assert_eq!(
        argv.borrow()[0],
        vec![
            "display-message",
            "-t",
            "%5",
            "-p",
            "#{pane_width}\t#{pane_height}"
        ]
    );
    // an unparsable answer falls back to claude's own pty-host size
    assert_eq!(
        engine_screen_size("cafe1234"),
        (_DEFAULT_PTY_COLS, _DEFAULT_PTY_ROWS)
    );
    // a job on nobody's pane never asks tmux
    assert_eq!(
        engine_screen_size("beef5678"),
        (_DEFAULT_PTY_COLS, _DEFAULT_PTY_ROWS)
    );
    assert_eq!(argv.borrow().len(), 2);
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

    let client = attach_pipe("cafe1234", &bin).unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !out.join("argv").exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let argv = fs::read_to_string(out.join("argv")).unwrap();
    assert_eq!(argv.lines().collect::<Vec<_>>(), vec!["attach", "cafe1234"]);
    close_pipe(client);
}

#[test]
fn test_pipe_env_is_washed_of_claude_vars() {
    let mut home = claude_home();
    home.env.set("CLAUDE_CODE_CHILD_SESSION", "1");
    home.env.set("ANTHROPIC_API_KEY", "secret");
    let env = bg_env(None);
    assert!(!env.contains_key("CLAUDE_CODE_CHILD_SESSION"));
    assert!(!env.contains_key("ANTHROPIC_API_KEY"));
}

#[test]
fn test_bg_env_carries_no_identity_of_the_spawner_or_of_hive() {
    // The spawner may be a codex or grok member; its session id keys *its*
    // roster row, so a job inheriting it would sign as the spawner. Hive
    // hands the job no identity of its own either — the engine mints one.
    let mut home = claude_home();
    home.env.set("CODEX_THREAD_ID", "tid-1");
    home.env.set("GROK_SESSION_ID", "s-spawner");
    let env = bg_env(Some(&HashMap::from([(
        "CR_WORKSPACE".to_string(),
        "/tmp/cr".to_string(),
    )])));
    assert!(!env.contains_key("CODEX_THREAD_ID"));
    assert!(!env.contains_key("GROK_SESSION_ID"));
    // and hive pins nothing of its own beyond the config tree and the
    // caller's extras: the engine's identity is the sessionId it mints
    let inherited: std::collections::HashSet<String> =
        std::env::vars().map(|(key, _)| key).collect();
    let mut pinned: Vec<&str> = env
        .keys()
        .filter(|key| !inherited.contains(*key))
        .map(String::as_str)
        .collect();
    pinned.sort_unstable();
    assert_eq!(pinned, ["CLAUDE_CONFIG_DIR", "CR_WORKSPACE"]);
    assert_eq!(env.get("CR_WORKSPACE").map(String::as_str), Some("/tmp/cr"));
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
        Wire::default(),
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
        Wire::default(),
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
        Wire::default(),
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
    wire(&mut hook, &pipe, &[wrapped], Some(path), Wire::default());
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
        Wire {
            baseline: stale,
            ..Default::default()
        },
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
    wire(
        &mut hook,
        &pipe,
        &[&frame],
        Some(path),
        Wire {
            baseline: stale,
            ..Default::default()
        },
    );
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
    wire(&mut hook, &pipe, &[&viewport], Some(path), Wire::default());
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
        Wire {
            baseline: earlier,
            ..Default::default()
        },
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
    let hook = Hook {
        attach_pipe: Some(pipe.clone()),
        engine_for_job: Some(VecDeque::from(vec![None])),
        no_sleep: true,
        ..Default::default()
    };
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
        Wire::default(),
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
        Wire::default(),
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
        Wire::default(),
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
    wire(&mut hook, &pipe, &["> /cost"], Some(path), Wire::default());
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
    wire(&mut hook, &pipe, &["> ping"], Some(path), Wire::default());
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
        Wire {
            engine: Some(fake_engine("cafe1234", "busy")),
            ..Default::default()
        },
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
    wire(&mut hook, &pipe, &[""], Some(path), Wire::default());
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
        Wire {
            engine: Some(fake_engine("cafe1234", "busy")),
            ..Default::default()
        },
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
        Wire {
            engine: Some(fake_engine("cafe1234", "busy")),
            ..Default::default()
        },
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
    wire(&mut hook, &pipe, &["> ping"], Some(path), Wire::default());
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
        Wire {
            draft: true,
            ..Default::default()
        },
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
        Wire::default(),
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
        Wire {
            draft: true,
            ..Default::default()
        },
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
    wire(
        &mut hook,
        &pipe,
        &["> /cost"],
        Some(path),
        Wire {
            draft: true,
            ..Default::default()
        },
    );
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
        Wire {
            draft: true,
            ..Default::default()
        },
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
    let hook = Hook {
        pane_for_job: Some(Some("%7".to_string())),
        view_probe: Some(Ok(("cafe1234".to_string(), "certain".to_string()))),
        suspected_draft: Some(true),
        ..Default::default()
    };
    let _g = testhook::install(hook);

    assert!(composer_has_draft("cafe1234"));
    assert_eq!(
        testhook::with(|h| h.suspected_calls.clone()).unwrap(),
        vec![("%7".to_string(), "claude".to_string())]
    );
}

#[test]
fn test_the_draft_gate_is_closed_when_the_viewer_shows_someone_else() {
    let hook = Hook {
        pane_for_job: Some(Some("%7".to_string())),
        view_probe: Some(Ok(("other999".to_string(), "certain".to_string()))),
        suspected_draft: Some(true), // must not capture
        ..Default::default()
    };
    let _g = testhook::install(hook);

    assert!(!composer_has_draft("cafe1234"));
    assert!(testhook::with(|h| h.suspected_calls.clone())
        .unwrap()
        .is_empty());
}

#[test]
fn test_the_draft_gate_is_closed_without_a_pane() {
    let hook = Hook {
        pane_for_job: Some(None),
        ..Default::default()
    };
    let _g = testhook::install(hook);
    assert!(!composer_has_draft("cafe1234"));
}

#[test]
fn test_a_probe_failure_closes_the_draft_gate() {
    let hook = Hook {
        pane_for_job: Some(Some("%7".to_string())),
        view_probe: Some(Err(())), // tmux gone
        ..Default::default()
    };
    let _g = testhook::install(hook);
    assert!(!composer_has_draft("cafe1234"));
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
    let hook = Hook {
        // pre-check, then confirm poll
        engine_for_job: Some(VecDeque::from(vec![
            Some(named_engine("hive-183")),
            Some(named_engine("honey.worker")),
        ])),
        rename_result: Some(true),
        no_sleep: true,
        ..Default::default()
    };
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
    let hook = Hook {
        engine_for_job: Some(VecDeque::from(vec![Some(named_engine("honey.worker"))])),
        rename_result: Some(true), // any frame would be recorded
        ..Default::default()
    };
    let _g = testhook::install(hook);

    assert!(ensure_job_named("cafe1234", "honey.worker"));
    assert!(testhook::with(|h| h.renames.clone()).unwrap().is_empty());
}

#[test]
fn test_a_refused_rename_frame_reports_failure() {
    let hook = Hook {
        engine_for_job: Some(VecDeque::from(vec![Some(named_engine("hive-183"))])),
        rename_result: Some(false),
        ..Default::default()
    };
    let _g = testhook::install(hook);

    assert!(!ensure_job_named("cafe1234", "honey.worker"));
}

#[test]
fn test_a_rename_the_registry_never_confirms_reports_failure() {
    let hook = Hook {
        engine_for_job: Some(VecDeque::from(vec![Some(named_engine("hive-183"))])),
        rename_result: Some(true),
        rename_confirm_timeout: Some(0.2),
        rename_poll_interval: Some(0.05),
        ..Default::default()
    };
    let _g = testhook::install(hook);

    assert!(!ensure_job_named("cafe1234", "honey.worker"));
}

#[test]
fn test_naming_an_engineless_job_reports_failure() {
    let hook = Hook {
        engine_for_job: Some(VecDeque::from(vec![None])),
        ..Default::default()
    };
    let _g = testhook::install(hook);
    assert!(!ensure_job_named("cafe1234", "honey.worker"));
}

#[test]
fn test_the_registry_name_is_read_into_the_engine_session() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("s.sock");
    fs::write(&sock, "").unwrap();
    let entry = json!({
        "kind": "bg",
        "pid": 1,
        "jobId": "cafe1234",
        "messagingSocketPath": sock.to_str().unwrap(),
        "name": "honey.worker",
    });
    let engine = entry_to_engine(entry.as_object().unwrap()).unwrap();
    assert_eq!(engine.name, "honey.worker");
}

#[test]
fn test_bg_env_keeps_color_forcing_for_the_renderer() {
    // Color is the engine's to keep — a cold-spawned engine renders its
    // TUI with this env for its whole life. Safety against colored output
    // lives at the parse sites (ANSI strip), never in the env.
    let mut home = claude_home();
    home.env.set("FORCE_COLOR", "3");
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
