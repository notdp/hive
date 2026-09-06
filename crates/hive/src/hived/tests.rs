use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Map, Value};

use crate::adapters::claude_bg::PaneJob;
use crate::adapters::grok_leader::SessionRecord;
use crate::agent::{Agent, DeliveryError};
use crate::runtime_snapshot::RuntimeSnapshot;
use crate::team::Team;
use crate::testenv::EnvGuard;
use crate::{bus, devlog};

use super::testhook::{self, FakeAdapter, Hook};
use super::*;
use crate::adapters::claude_bg::EngineSession;
use crate::adapters::claude_view::PaneView;
use crate::adapters::codex_app_server::ThreadRuntime;
use crate::adapters::grok_leader::SessionRuntime;

/// Collectors the hook closures push into: `(target, option, value)` tmux
/// writes, `(event, payload)` notify emits, `(argv, stderr path)` spawns and
/// `(event, fields)` debug emits.
type OptionWrites = Arc<Mutex<Vec<(String, String, String)>>>;
type EventSink = Arc<Mutex<Vec<(String, Map<String, Value>)>>>;
type SpawnSink = Arc<Mutex<Vec<(Vec<String>, PathBuf)>>>;
type DebugEventSink = Arc<Mutex<Vec<(String, Vec<(String, Value)>)>>>;
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

// ---- busy / phantom-redraw gate ----------------------------------------

/// Output monitor with a fixed busy verdict and output age.
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

/// The autouse fixture: fresh path cache, `native_daemon_busy` → None.
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
    assert_eq!(transcript_progressed_recently("%1", 3.0), None);
}

#[test]
fn test_progressed_returns_none_when_stat_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let ghost = tmp.path().join("missing.jsonl");
    let mut hook = gate_hook();
    stub_path(&mut hook, Some(ghost.to_string_lossy().to_string()));
    let _guard = testhook::install(hook);
    assert_eq!(transcript_progressed_recently("%1", 3.0), None);
}

#[test]
fn test_progressed_returns_true_when_mtime_fresh() {
    let tmp = tempfile::tempdir().unwrap();
    let fresh = write_file(tmp.path(), "fresh.jsonl", "x");
    let mut hook = gate_hook();
    stub_path(&mut hook, Some(fresh.to_string_lossy().to_string()));
    let _guard = testhook::install(hook);
    assert_eq!(transcript_progressed_recently("%1", 3.0), Some(true));
}

#[test]
fn test_progressed_returns_false_when_mtime_stale() {
    let tmp = tempfile::tempdir().unwrap();
    let stale = write_file(tmp.path(), "stale.jsonl", "x");
    backdate(&stale, 60.0);
    let mut hook = gate_hook();
    stub_path(&mut hook, Some(stale.to_string_lossy().to_string()));
    let _guard = testhook::install(hook);
    assert_eq!(transcript_progressed_recently("%1", 3.0), Some(false));
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
    assert_eq!(transcript_progressed_recently("%1", 3.0), Some(true));
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
    assert_eq!(transcript_progressed_recently("%1", 3.0), Some(false));
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
    assert_eq!(transcript_progressed_recently("%1", 3.0), Some(false));
}

#[test]
fn test_progressed_returns_false_when_fresh_resolve_yields_no_path() {
    let tmp = tempfile::tempdir().unwrap();
    let stale = write_file(tmp.path(), "stale.jsonl", "x");
    backdate(&stale, 60.0);
    let mut hook = gate_hook();
    stub_path_with_force(&mut hook, Some(stale.to_string_lossy().to_string()), None);
    let _guard = testhook::install(hook);
    assert_eq!(transcript_progressed_recently("%1", 3.0), Some(false));
}

#[test]
fn test_truly_busy_true_when_app_server_busy() {
    let mut hook = gate_hook();
    stub_path(&mut hook, None);
    stub_app_server_busy(&mut hook, Some(true));
    let _guard = testhook::install(hook);
    assert!(pane_is_truly_busy("%1", Some(&FakeMonitor::new(false))));
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
    assert!(!pane_is_truly_busy("%1", Some(&FakeMonitor::new(true))));
}

#[test]
fn test_truly_busy_falls_through_when_no_app_server() {
    let tmp = tempfile::tempdir().unwrap();
    let fresh = write_file(tmp.path(), "fresh.jsonl", "x");
    let mut hook = gate_hook();
    stub_path(&mut hook, Some(fresh.to_string_lossy().to_string()));
    stub_app_server_busy(&mut hook, None);
    let _guard = testhook::install(hook);
    assert!(pane_is_truly_busy("%1", Some(&FakeMonitor::new(true))));
}

#[test]
fn test_is_output_busy_true_when_app_server_busy() {
    let mut hook = gate_hook();
    stub_path(&mut hook, None);
    stub_app_server_busy(&mut hook, Some(true));
    let _guard = testhook::install(hook);
    assert!(is_output_busy("%1", Some(&FakeMonitor::new(false)), None));
}

#[test]
fn test_is_output_busy_false_when_app_server_idle() {
    let mut hook = gate_hook();
    stub_path(&mut hook, None);
    stub_app_server_busy(&mut hook, Some(false));
    let _guard = testhook::install(hook);
    assert!(!is_output_busy("%1", Some(&FakeMonitor::new(true)), None));
}

#[test]
fn test_truly_busy_false_when_monitor_idle() {
    let mut hook = gate_hook();
    stub_path(&mut hook, None);
    let _guard = testhook::install(hook);
    assert!(!pane_is_truly_busy("%1", Some(&FakeMonitor::new(false))));
}

#[test]
fn test_truly_busy_falls_back_to_monitor_when_path_unknown() {
    // Fallback contract: never silently disable notify for panes the
    // gate can't introspect.
    let mut hook = gate_hook();
    stub_path(&mut hook, None);
    let _guard = testhook::install(hook);
    assert!(pane_is_truly_busy("%1", Some(&FakeMonitor::new(true))));
}

#[test]
fn test_truly_busy_true_when_monitor_busy_and_transcript_fresh() {
    let tmp = tempfile::tempdir().unwrap();
    let fresh = write_file(tmp.path(), "fresh.jsonl", "x");
    let mut hook = gate_hook();
    stub_path(&mut hook, Some(fresh.to_string_lossy().to_string()));
    let _guard = testhook::install(hook);
    assert!(pane_is_truly_busy("%1", Some(&FakeMonitor::new(true))));
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
    assert!(!pane_is_truly_busy("%1", Some(&FakeMonitor::new(true))));
}

#[test]
fn test_truly_busy_false_when_monitor_none() {
    let _guard = testhook::install(gate_hook());
    assert!(!pane_is_truly_busy("%1", None));
}

#[test]
fn test_truly_busy_false_when_pane_id_empty() {
    let mut hook = gate_hook();
    stub_path(&mut hook, None);
    let _guard = testhook::install(hook);
    assert!(!pane_is_truly_busy("", Some(&FakeMonitor::new(true))));
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
    assert!(is_output_busy("%1", Some(&monitor), Some(5.0)));
    assert!(!is_output_busy("%1", Some(&monitor), Some(1.0)));
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
    assert!(is_output_busy("%1", Some(&monitor), Some(5.0)));
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
    assert!(!is_output_busy("%1", Some(&monitor), Some(5.0)));
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

    assert_eq!(resolve_transcript_path_cached("%1", false), None);
    assert_eq!(resolve_transcript_path_cached("%1", false), None);
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

    resolve_transcript_path_cached("%1", false);
    assert_eq!(CALLS.load(Ordering::SeqCst), 1);

    transcript_path_cache().lock().unwrap().insert(
        "%1".to_string(),
        (String::new(), monotonic() - 1.0, String::new()),
    );
    resolve_transcript_path_cached("%1", false);
    assert_eq!(CALLS.load(Ordering::SeqCst), 2);
}

// ---- runtime snapshots -------------------------------------------------

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
        Some(monotonic() - SESSION_SNAPSHOT_FRESHNESS_S - 1.0),
        Some(SESSION_SNAPSHOT_FRESHNESS_S),
    )
}

#[test]
fn test_runtime_snapshot_payload_reads_store_without_live_probe() {
    let _guard = testhook::install(Hook::default());
    seed_snapshot("%1", "sid-tick", 10.0, None);

    let payload = runtime_snapshot_payload("%1");

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

    let payload = runtime_snapshot_payload("%1");

    assert_eq!(payload["ok"], Value::Bool(true));
    assert_eq!(payload["snapshot"]["sessionId"], Value::from("sid-old"));
    assert_eq!(payload["snapshot"]["_sessionIdFresh"], Value::Bool(false));
}

#[test]
fn test_claude_turn_open_reads_busy_unless_the_status_is_stale() {
    use crate::adapters::claude_bg::{runtime_from_engine, STATUS_STALE_AFTER_SECONDS};
    let engine = |status: &str, updated_at: f64| EngineSession {
        status: status.to_string(),
        status_updated_at: updated_at,
        ..crate::agent::testhook::fake_engine(4242, "b9beb2b8", "sess-1")
    };
    let now = 1_000_000.0;
    let open = |status: &str, updated_at: f64| {
        claude_turn_open(&runtime_from_engine(&engine(status, updated_at), Some(now)))
    };
    assert_eq!(open("busy", now - 1.0), Ok(true));
    assert_eq!(open("idle", now - 1.0), Ok(false));
    assert_eq!(open("waiting", now - 1.0), Ok(false));
    // An engine with no status record answers "not busy" — its runtime
    // folds an unknown status to busy=false, which is still the daemon's
    // own word.
    assert_eq!(open("", 0.0), Ok(false));
    // A status that stopped advancing is a wedged engine, no answer.
    let stale = open("busy", now - STATUS_STALE_AFTER_SECONDS - 1.0).unwrap_err();
    assert!(stale.contains("stale status"), "{stale}");
    let blank = claude_turn_open(&Map::new()).unwrap_err();
    assert!(blank.contains("no busy flag"), "{blank}");
}

fn turn_open_team(name: &str) -> Team {
    let with_session = |agent: Agent, sid: &str| Agent {
        session_id: Some(sid.to_string()),
        ..agent
    };
    fake_team(
        name,
        vec![
            with_session(fake_agent("c", "%4", "codex"), "thr-1"),
            with_session(fake_agent("c-mute", "%5", "codex"), "thr-mute"),
            fake_agent("c-blank", "", "codex"),
            with_session(fake_agent("k", "%6", "claude"), "cafe1234"),
            with_session(fake_agent("k-idle", "%7", "claude"), "beef5678"),
            with_session(fake_agent("k-stale", "%8", "claude"), "dead0000"),
            with_session(fake_agent("k-gone", "%9", "claude"), "0000aaaa"),
            with_session(
                fake_agent("k-joined", "%10", "claude"),
                "0f4e2a9c-6b1d-4e0a-9c3b-1d2e3f4a5b6c",
            ),
            fake_agent("g", "%3", "grok"),
            fake_agent("g-idle", "%12", "grok"),
            fake_agent("g-seen", "%13", "grok"),
            fake_agent("quiet", "", "grok"),
            fake_agent("sh", "%11", "bash"),
        ],
    )
}

#[test]
fn test_turn_open_payload_asks_each_engine_directly() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let debug_events: DebugEventSink = Arc::new(Mutex::new(Vec::new()));
    let debug_sink = Arc::clone(&debug_events);
    let hook = Hook {
        team_load: Some(Arc::new(|name| Ok(turn_open_team(name)))),
        // codex: the app-server's `thread/read` on the roster thread id.
        cas_turn_open_for_thread: Some(Arc::new(|thread| match thread {
            "thr-1" => Some(true),
            "thr-mute" => None,
            _ => panic!("unexpected thread {thread}"),
        })),
        // claude: the bg job's engine record, keyed by the roster job id.
        cb_engine_session_for_job: Some(Arc::new(move |job| {
            let engine = |status: &str, updated_at: f64| EngineSession {
                status: status.to_string(),
                status_updated_at: updated_at,
                ..crate::agent::testhook::fake_engine(4242, job, "sess-1")
            };
            match job {
                "cafe1234" => Some(engine("busy", now)),
                "beef5678" => Some(engine("idle", now)),
                "dead0000" => Some(engine("busy", 1.0)),
                "0000aaaa" => None,
                _ => panic!("unexpected job {job}"),
            }
        })),
        // grok: the leader pool's push-fed turn evidence.
        gl_turn_open_for_key: Some(Arc::new(|key| match key {
            "m-honey.g" => Some(Some(true)),
            "m-honey.g-idle" => Some(Some(false)),
            // a client on the key (loaded and reporting a command table,
            // an announcement) that has seen no turn event
            "m-honey.g-seen" => Some(None),
            _ => None,
        })),
        notify_debug_emit: Some(Arc::new(move |ws, event, fields| {
            assert_eq!(ws, "/ws");
            debug_sink.lock().unwrap().push((
                event.to_string(),
                fields
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect(),
            ));
        })),
        ..Default::default()
    };
    let _guard = testhook::install(hook);
    let open = |agent: &str| turn_open_payload("/ws", "honey", agent).unwrap()["open"].clone();
    // A null answer carries its reason, and the same reason went out as a
    // `turn_open.null` event for the member.
    let null_reason = |agent: &str, cli: &str| -> String {
        let payload = turn_open_payload("/ws", "honey", agent).unwrap();
        assert_eq!(payload["open"], Value::Null, "{agent}");
        let reason = payload["reason"].as_str().unwrap().to_string();
        let events = debug_events.lock().unwrap();
        let (_, fields) = events
            .iter()
            .rev()
            .find(|(event, _)| event == "turn_open.null")
            .expect("turn_open.null emitted");
        let field = |k: &str| fields.iter().find(|(key, _)| key == k).unwrap().1.clone();
        assert_eq!(field("team"), Value::from("honey"));
        assert_eq!(field("cli"), Value::from(cli));
        assert_eq!(field("agent"), Value::from(agent));
        assert_eq!(field("reason"), Value::from(reason.as_str()));
        reason
    };

    let payload = turn_open_payload("/ws", "honey", "c").unwrap();
    assert_eq!(payload["ok"], Value::Bool(true));
    assert_eq!(payload["agent"], Value::from("c"));
    assert_eq!(payload["open"], Value::Bool(true));
    assert!(payload.get("reason").is_none());
    // No daemon answer, or no thread to ask about, is no answer.
    assert!(null_reason("c-mute", "codex").contains("thr-mute"));
    assert!(null_reason("c-blank", "codex").contains("no session id"));

    assert_eq!(open("k"), Value::Bool(true));
    assert_eq!(open("k-idle"), Value::Bool(false));
    // A stale status is a wedged engine's last word; no engine entry is a
    // daemon that cannot be asked; a row naming the engine session rather
    // than a job has no engine record to read.
    assert!(null_reason("k-stale", "claude").contains("stale status"));
    assert!(null_reason("k-gone", "claude").contains("no engine entry for job 0000aaaa"));
    assert!(null_reason("k-joined", "claude").contains("not a bg job id"));

    assert_eq!(open("g"), Value::Bool(true));
    assert_eq!(open("g-idle"), Value::Bool(false));
    // A leader client that has seen no turn event is no answer, and so is
    // a key with no client at all.
    assert!(null_reason("g-seen", "grok").contains("no turn evidence yet"));
    assert!(null_reason("quiet", "grok").contains("no leader client for m-honey.quiet"));

    // An engine hive cannot ask has no answer; a member off the roster is
    // an error, not a null.
    assert!(null_reason("sh", "bash").contains("no turn evidence"));
    assert!(turn_open_payload("/ws", "honey", "nobody").is_err());
    // Nothing positive emitted an event.
    let events = debug_events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|(event, _)| event == "turn_open.null")
            .count(),
        8
    );
}

#[test]
fn test_turn_open_payload_records_a_failed_team_load() {
    let debug_events: DebugEventSink = Arc::new(Mutex::new(Vec::new()));
    let debug_sink = Arc::clone(&debug_events);
    let _guard = testhook::install(Hook {
        team_load: Some(Arc::new(|name| Err(anyhow::anyhow!("no entry for {name}")))),
        notify_debug_emit: Some(Arc::new(move |_ws, event, fields| {
            debug_sink.lock().unwrap().push((
                event.to_string(),
                fields
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect(),
            ));
        })),
        ..Default::default()
    });
    let err = turn_open_payload("/ws", "honey", "g")
        .unwrap_err()
        .to_string();
    assert!(err.contains("no entry for honey"), "{err}");
    let events = debug_events.lock().unwrap();
    let (event, fields) = &events[0];
    assert_eq!(event, "turn_open.null");
    let field = |k: &str| fields.iter().find(|(key, _)| key == k).unwrap().1.clone();
    assert_eq!(field("agent"), Value::from("g"));
    assert!(field("reason")
        .as_str()
        .unwrap()
        .starts_with("team load failed: no entry for honey"));
}

#[test]
fn test_handle_request_turn_open_answers_for_the_team() {
    let hook = Hook {
        team_load: Some(Arc::new(|name| {
            Ok(fake_team(name, vec![fake_agent("g", "%3", "grok")]))
        })),
        gl_turn_open_for_key: Some(Arc::new(|key| {
            assert_eq!(key, "m-honey.g");
            Some(Some(false))
        })),
        ..Default::default()
    };
    let _guard = testhook::install(hook);
    let request = json_obj(&[
        ("action", Value::from("turn-open")),
        ("team", Value::from("honey")),
        ("agent", Value::from("g")),
    ]);
    let (response, keep_serving) = handle_request(
        "/ws",
        "honey",
        "dev:1",
        "@7",
        "2026-01-01T00:00:00Z",
        &request,
    );
    assert!(keep_serving);
    assert_eq!(response["ok"], Value::Bool(true));
    assert_eq!(response["open"], Value::Bool(false));
    let missing = json_obj(&[
        ("action", Value::from("turn-open")),
        ("agent", Value::from("nobody")),
    ]);
    let (response, _) = handle_request(
        "/ws",
        "honey",
        "dev:1",
        "@7",
        "2026-01-01T00:00:00Z",
        &missing,
    );
    assert_eq!(response["ok"], Value::Bool(false));
}

#[test]
fn test_runtime_snapshot_payload_returns_none_when_snapshot_missing() {
    let _guard = testhook::install(Hook::default());

    let payload = runtime_snapshot_payload("%1");

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
        resolve_transcript_path_cached("%1", false),
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
        resolve_transcript_path_cached("%1", false),
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
        resolve_transcript_path_cached("%1", false),
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

    let runtime = agent_runtime_payload("%1", Some(&stale));

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
        agent_runtime_payload("%1", None)["sessionId"],
        Value::from("sid-new")
    );

    let store = runtime_snapshots().lock().unwrap();
    let field = &store.get("%1").unwrap().sessionId;
    assert_eq!(field.freshness_s, Some(SESSION_SNAPSHOT_FRESHNESS_S));
    assert!(field.is_fresh(Some(field.observed_at + 1.0)));
    assert!(!field.is_fresh(Some(field.observed_at + field.freshness_s.unwrap() + 1.0)));
}

// ---- claude runtime ----------------------------------------------------

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
    record: Option<PaneJob>,
    engine: Option<EngineSession>,
    rows: Option<Vec<Map<String, Value>>>,
) {
    hook.cb_read_pane_job = Some(Arc::new(move |_p| record.clone()));
    hook.cb_engine_session_for_job = Some(Arc::new(move |_j| engine.clone()));
    hook.cb_list_jobs = Some(Arc::new(move || rows.clone()));
}

fn record(job: &str, sid: &str) -> Option<PaneJob> {
    Some(PaneJob {
        job_id: job.to_string(),
        session_id: sid.to_string(),
        cwd: "/w".to_string(),
    })
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

    let rt = claude_bg_runtime("%1").unwrap();

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

    let rt = claude_bg_runtime("%1").unwrap();

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

    let rt = claude_bg_runtime("%1").unwrap();

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

    let rt = claude_bg_runtime("%1").unwrap();

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

    let rt = claude_bg_runtime("%1").unwrap();

    assert_eq!(rt["cliAlive"], Value::Bool(true)); // benefit of the doubt: never a reap signal
    assert_eq!(rt["inputState"], Value::from("unknown"));
    assert_eq!(rt["inputReason"], Value::from("ledger_unavailable"));
}

#[test]
fn test_bg_runtime_none_for_unmanaged_pane() {
    let mut hook = Hook::default();
    pin(&mut hook, None, None, Some(vec![]));
    let _guard = testhook::install(hook);
    assert!(claude_bg_runtime("%1").is_none());
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

    claude_bg_runtime("%1");
    claude_bg_runtime("%1");

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

    let rt = agent_runtime_payload("%1", None);

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
    assert_eq!(claude_registry_busy("%1"), Some(true));
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
    assert_eq!(claude_registry_busy("%1"), Some(true));
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
    assert_eq!(claude_registry_busy("%1"), None);
}

/// A live interactive (non-member) claude on the pane tty: no job
/// record, a resolvable session, and *status* as its registry entry's
/// report.
fn interactive_claude_pane(tmp: &Path, status: Option<(String, String)>, transcript: bool) -> Hook {
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

    let rt = agent_runtime_payload("%7", None);

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
        let mut hook =
            interactive_claude_pane(tmp.path(), Some((status.to_string(), String::new())), true);
        forbid_gate(&mut hook, "the registry answered; the gate must not run");
        let _guard = testhook::install(hook);

        let rt = agent_runtime_payload("%7", None);

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

    let rt = agent_runtime_payload("%7", None);

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
    let mut records: HashMap<String, PaneJob> = HashMap::new();
    records.insert("%9".to_string(), record("dead0001", "s").unwrap());
    records.insert("%1".to_string(), record("live0001", "s").unwrap());
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

    claude_supervisor_tick("/tmp/ws");

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

    claude_supervisor_tick("/tmp/ws");

    // unknown is not dead: nothing pruned, nothing parked
    assert!(cleared.lock().unwrap().is_empty());
}

// ---- codex runtime -----------------------------------------------------

fn thread_runtime(busy: bool, input_state: &str) -> ThreadRuntime {
    ThreadRuntime {
        busy,
        input_state: input_state.to_string(),
        ..Default::default()
    }
}

#[test]
fn test_codex_app_server_runtime_maps_fields() {
    let rt = thread_runtime(true, "ready");
    let hook = Hook {
        cas_runtime_for_pane: Some(Arc::new(move |_p| Some(rt.clone()))),
        ..Default::default()
    };
    let _guard = testhook::install(hook);
    let out = codex_app_server_runtime("%5").unwrap();
    assert_eq!(out["busy"], Value::Bool(true));
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
    assert!(codex_app_server_runtime("%5").is_none());
}

#[test]
fn test_codex_app_server_runtime_waiting_user() {
    let rt = thread_runtime(true, "waiting_user");
    let hook = Hook {
        cas_runtime_for_pane: Some(Arc::new(move |_p| Some(rt.clone()))),
        ..Default::default()
    };
    let _guard = testhook::install(hook);
    let out = codex_app_server_runtime("%5").unwrap();
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
    crate::agent::testhook::fake_agent(name, "", pane_id, cli)
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

    let diag = doctor_payload(&tmp.path().to_string_lossy(), "t", "a", true, None).unwrap();

    let mut expected = Map::new();
    expected.insert("socket".to_string(), Value::from("/x/hive-shared.sock"));
    expected.insert("alive".to_string(), Value::Bool(true));
    expected.insert("threadId".to_string(), Value::from("tid-5"));
    assert_eq!(diag["codexDaemon"], Value::Object(expected));
}

// ---- grok runtime ------------------------------------------------------

fn session_runtime(busy: bool, input_state: &str) -> SessionRuntime {
    SessionRuntime {
        busy,
        input_state: input_state.to_string(),
        ..Default::default()
    }
}

#[test]
fn test_grok_leader_runtime_maps_fields() {
    let rt = session_runtime(true, "ready");
    let hook = Hook {
        gl_runtime_for_pane: Some(Arc::new(move |_p| Some(rt.clone()))),
        ..Default::default()
    };
    let _guard = testhook::install(hook);
    let out = grok_leader_runtime("%5").unwrap();
    assert_eq!(out["busy"], Value::Bool(true));
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
    assert!(grok_leader_runtime("%5").is_none());
}

#[test]
fn test_grok_leader_runtime_defaults_empty_input_state_to_ready() {
    let rt = session_runtime(true, "");
    let hook = Hook {
        gl_runtime_for_pane: Some(Arc::new(move |_p| Some(rt.clone()))),
        ..Default::default()
    };
    let _guard = testhook::install(hook);
    assert_eq!(
        grok_leader_runtime("%5").unwrap()["inputState"],
        Value::from("ready")
    );
}

#[test]
fn test_grok_leader_runtime_waiting_user() {
    let rt = session_runtime(true, "waiting_user");
    let hook = Hook {
        gl_runtime_for_pane: Some(Arc::new(move |_p| Some(rt.clone()))),
        ..Default::default()
    };
    let _guard = testhook::install(hook);
    let out = grok_leader_runtime("%5").unwrap();
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
        Some(session_runtime(true, "ready")),
        Some("sid-grok-1".to_string()),
    );
    let _guard = testhook::install(hook);
    let rt = agent_runtime_payload("%5", None);
    assert_eq!(rt["cliAlive"], Value::Bool(true));
    assert_eq!(rt["busy"], Value::Bool(true));
    assert_eq!(rt["_runtimeSource"], Value::from("grok-leader"));
    assert_eq!(rt["sessionId"], Value::from("sid-grok-1"));
}

#[test]
fn test_agent_payload_grok_session_unresolved_without_record() {
    let hook = live_grok_pane(Some(session_runtime(false, "ready")), None);
    let _guard = testhook::install(hook);
    assert_eq!(
        agent_runtime_payload("%5", None)["sessionId"],
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

    let rt = agent_runtime_payload("%5", None);
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
        assert_eq!(native_daemon_busy("%5"), Some(busy));
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
    assert_eq!(native_daemon_busy("%5"), None);
}

// ---- claude view tick --------------------------------------------------

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
    options: OptionWrites,
    events: EventSink,
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
    let options: OptionWrites = Arc::new(Mutex::new(Vec::new()));
    let events: EventSink = Arc::new(Mutex::new(Vec::new()));

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
    claude_view_tick("/tmp/ws", "probe", &members, &mut env.state);
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

// ---- claude job names --------------------------------------------------

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

    claude_name_tick(&members, "honey", &mut state);
    claude_name_tick(&members, "honey", &mut state);

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

    claude_name_tick(
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
        claude_name_tick(&members, "honey", &mut state);
        assert!(state.named.is_empty());
    }

    let (_guard, _started) = name_wire(
        HashMap::from([("%183".to_string(), "485865b2".to_string())]),
        HashMap::from([("485865b2".to_string(), named_engine("485865b2", "hive-183"))]),
    );
    claude_name_tick(&members, "honey", &mut state);
    assert_eq!(state.named, HashSet::from(["485865b2".to_string()]));
}

#[test]
fn test_non_claude_members_are_not_renamed() {
    let (_guard, started) = name_wire(
        HashMap::from([("%184".to_string(), "job".to_string())]),
        HashMap::new(),
    );

    claude_name_tick(
        &name_members("%184", "grok", "validator"),
        "honey",
        &mut ClaudeTickState::default(),
    );

    assert!(started.lock().unwrap().is_empty());
}

// ---- grok daemon cleanup -----------------------------------------------

/// Daemon keys on disk; records emit/drop/kill call order.
struct ReapEnv {
    calls: Arc<Mutex<Vec<String>>>,
    keys: Arc<Mutex<Vec<String>>>,
    tmp: tempfile::TempDir,
    _env: EnvGuard,
    _guard: testhook::Guard,
}

fn reap_env(pane_alive: bool) -> ReapEnv {
    let mut env = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let keys: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let keys_src = Arc::clone(&keys);
    let socket_dir = tmp.path().to_path_buf();
    let kill_sink = Arc::clone(&calls);
    let drop_sink = Arc::clone(&calls);
    let emit_sink = Arc::clone(&calls);
    let hook = Hook {
        gl_list_daemon_keys: Some(Arc::new(move || keys_src.lock().unwrap().clone())),
        gl_socket_path_for_key: Some(Arc::new(move |key| socket_dir.join(format!("{key}.sock")))),
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
        _env: env,
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

    cleanup_dead_daemons("/tmp/ws", "honey");

    assert!(env.calls.lock().unwrap().is_empty());
}

#[test]
fn test_cleanup_reaps_dead_pane_and_logs_before_kill() {
    let env = reap_env(false);
    *env.keys.lock().unwrap() = vec!["p4".to_string()];

    cleanup_dead_daemons("/tmp/ws", "honey");

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

    cleanup_dead_daemons("/tmp/ws", "honey");

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

    cleanup_dead_daemons("/tmp/ws", "honey");

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

    cleanup_dead_daemons("/tmp/ws", "honey");

    assert!(env.calls.lock().unwrap().is_empty());
}

#[test]
fn test_cleanup_leaves_another_team_s_member_daemon_alone() {
    // The leader directory is global; a registry is scoped to one
    // $HIVE_HOME. A hived on a disposable home (the acceptance lane) sees
    // the live team's key, finds no entry for that team in its own
    // registry, and would otherwise reap a member that is serving
    // someone. Reaping is per-team authority.
    let env = reap_env(true);
    *env.keys.lock().unwrap() = vec!["m-honey.sage".to_string()];
    write_pidfile(env.tmp.path(), "m-honey.sage", 999.0);

    cleanup_dead_daemons("/tmp/ws", "acc-throwaway");

    assert!(
        env.calls.lock().unwrap().is_empty(),
        "a hived must not reap a daemon belonging to a team it does not run"
    );
}

#[test]
fn test_cleanup_member_daemon_missing_registry_reaps_after_grace() {
    let env = reap_env(true);
    *env.keys.lock().unwrap() = vec!["m-honey.rex".to_string()];

    // newborn: inside the grace window, spawn registration may be in
    // flight
    write_pidfile(env.tmp.path(), "m-honey.rex", 5.0);
    cleanup_dead_daemons("/tmp/ws", "honey");
    assert!(env.calls.lock().unwrap().is_empty());

    // past the grace window with no registry entry: orphan
    write_pidfile(env.tmp.path(), "m-honey.rex", 999.0);
    cleanup_dead_daemons("/tmp/ws", "honey");
    assert!(env
        .calls
        .lock()
        .unwrap()
        .contains(&"kill m-honey.rex".to_string()));
}

// ---- codex shared-daemon supervisor ------------------------------------

#[derive(Clone)]
struct SuperState {
    panes: Vec<(String, String, String)>, // pane_id, agent, cli
    recorded: Vec<String>,
    record_sockets: HashMap<String, String>, // pane -> tmuxSocket; absent = legacy record
    own_socket: Option<String>,
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
        record_sockets: HashMap::new(),
        own_socket: Some(
            crate::tmux::default_socket_path()
                .to_string_lossy()
                .into_owned(),
        ),
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
    let s_sockets = Arc::clone(&state);
    let s_own = Arc::clone(&state);
    let s_threads = Arc::clone(&state);
    let s_alive = Arc::clone(&state);
    let s_spawn = Arc::clone(&state);
    let s_cli = Arc::clone(&state);
    let s_cmd = Arc::clone(&state);
    let hook = Hook {
        list_panes_all: Some(Arc::new(list_panes)),
        cas_list_recorded_panes: Some(Arc::new(move || s_recorded.recorded.clone())),
        cas_pane_thread_socket: Some(Arc::new(move |pane| {
            s_sockets.record_sockets.get(pane).cloned()
        })),
        tmux_socket_path: Some(Arc::new(move || s_own.own_socket.clone())),
        cas_clear_pane_thread: Some(Arc::new(move |pane| {
            clear_sink.lock().unwrap().push(format!("clear {pane}"))
        })),
        cas_thread_id_for_pane: Some(Arc::new(move |pane| s_threads.threads.get(pane).cloned())),
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
    codex_supervisor_tick("/tmp/ws", "t");
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn test_supervisor_prunes_records_of_dead_panes() {
    let mut state = super_state();
    state.recorded = vec!["%1".to_string(), "%dead".to_string()];
    let (_guard, calls) = super_env(state);
    codex_supervisor_tick("/tmp/ws", "t");
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
    codex_supervisor_tick("/tmp/ws", "t");
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn test_supervisor_respawns_dead_daemon_with_live_member() {
    let mut state = super_state();
    state.daemon_alive = false;
    let (_guard, calls) = super_env(state);
    codex_supervisor_tick("/tmp/ws", "t");
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
    codex_supervisor_tick("/tmp/ws", "t");
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
    codex_supervisor_tick("/tmp/ws", "t");
    codex_supervisor_tick("/tmp/ws", "t");
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
    codex_supervisor_tick("/tmp/ws", "t");
    assert!(!calls.lock().unwrap().iter().any(|c| c.starts_with("send ")));
}

#[test]
fn test_supervisor_never_types_into_a_non_shell() {
    let mut state = super_state();
    state.cli_process = HashMap::new();
    state.pane_command = HashMap::from([("%1".to_string(), "vim".to_string())]);
    let (_guard, calls) = super_env(state);
    codex_supervisor_tick("/tmp/ws", "t");
    assert!(!calls.lock().unwrap().iter().any(|c| c.starts_with("send ")));
}

#[test]
fn test_supervisor_skips_member_without_record() {
    let mut state = super_state();
    state.cli_process = HashMap::new();
    state.threads = HashMap::new();
    let (_guard, calls) = super_env(state);
    codex_supervisor_tick("/tmp/ws", "t");
    assert!(!calls.lock().unwrap().iter().any(|c| c.starts_with("send ")));
}

// ---- idle notify -------------------------------------------------------

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

/// One recorded `clear_stale_notify` call.
#[derive(Debug, PartialEq)]
struct Cleanup {
    window: String,
    panes: Vec<String>,
    token: String,
    source: String,
    workspace: String,
}

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
        clear_stale_notify: Some(Arc::new(move |window, panes, token, source, workspace| {
            cleanups_sink.lock().unwrap().push(Cleanup {
                window: window.to_string(),
                panes: panes.to_vec(),
                token: token.to_string(),
                source: source.to_string(),
                workspace: workspace.to_string(),
            })
        })),
        is_plugin_enabled: Some(Arc::new(move |_name| plugin_enabled)),
        // Both busy oracles answered here: an unhooked native_daemon_busy
        // resolves "%1" through the real codex pane record and asks the
        // live daemon, so the verdict would follow whatever member sits on
        // that pane of the developer's tmux.
        native_daemon_busy: Some(Arc::new(|_pane| None)),
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
    idle_notify_tick("team-a", "dev", state, Some(monitor), now, "", None, None);
}

fn idle_tick_dbg(
    state: &mut HashMap<String, IdleRecord>,
    monitor: &IdleBusyMonitor,
    now: f64,
    debug_state: &mut NotifyDebugState,
) {
    idle_notify_tick(
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
        vec![Cleanup {
            window: WINDOW.to_string(),
            panes: vec!["%1".to_string()],
            token: "%1:selected-fire".to_string(),
            source: "hived.active_window".to_string(),
            workspace: String::new(),
        }]
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
        vec![Cleanup {
            window: WINDOW.to_string(),
            panes: vec!["%1".to_string()],
            token: "%1:selected-fire".to_string(),
            source: "hived.active_window".to_string(),
            workspace: String::new(),
        }]
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

    assert_eq!(idle_notify_agent_panes("team-a"), vec!["%1".to_string()]);
}

// ---- socket server / lifecycle -----------------------------------------

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
    let server = Arc::new(open_server_socket(&workspace).unwrap());

    let ws_slow = workspace.clone();
    let slow_client = thread::spawn(move || request_hived(&ws_slow, &action_payload("send"), 10.0));
    let ws_serve = workspace.clone();
    let server_serve = Arc::clone(&server);
    let serve_thread = thread::spawn(move || {
        serve_requests(
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
    let response = request_hived(
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
    cleanup_socket_impl(&workspace);

    assert!(keep_running);
    assert!(!requests_in_flight());
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
    let server = Arc::new(open_server_socket(&workspace).unwrap());

    let ws_serve = workspace.clone();
    let server_serve = Arc::clone(&server);
    let serve_thread = thread::spawn(move || {
        serve_requests(
            server_serve.as_ref(),
            &ws_serve,
            "team-a",
            "dev:3",
            "@99",
            "2026-01-01T00:00:00Z",
            1.0,
        )
    });

    let response = request_hived(&workspace, &action_payload("shutdown"), 2.0);
    let keep_running = serve_thread.join().unwrap();

    assert_eq!(response, Some(json_obj(&[("ok", Value::Bool(true))])));
    assert!(!keep_running);

    SHUTDOWN.store(false, Ordering::SeqCst);
    server.close();
    cleanup_socket_impl(&workspace);
}

#[test]
fn test_socket_alive_requires_matching_api_version() {
    let hook = Hook {
        request_ping: Some(Arc::new(|_ws| Some(json_obj(&[("ok", Value::Bool(true))])))),
        ..Default::default()
    };
    let _guard = testhook::install(hook);
    assert!(!socket_alive("/tmp/ws"));

    testhook::update(|h| {
        h.request_ping = Some(Arc::new(|_ws| {
            Some(json_obj(&[
                ("ok", Value::Bool(true)),
                ("apiVersion", Value::from(HIVED_API_VERSION)),
            ]))
        }));
    });
    assert!(socket_alive("/tmp/ws"));
}

#[test]
fn test_hived_identity_matches_team_and_ignores_window() {
    assert!(!hived_identity_matches(
        Some(&json_obj(&[
            ("ok", Value::Bool(true)),
            ("apiVersion", Value::from(HIVED_API_VERSION)),
        ])),
        "team-a",
    ));
    assert!(!hived_identity_matches(
        Some(&json_obj(&[
            ("ok", Value::Bool(true)),
            ("apiVersion", Value::from(HIVED_API_VERSION)),
            ("team", Value::from("team-b")),
        ])),
        "team-a",
    ));
    assert!(!hived_identity_matches(
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
    assert!(hived_identity_matches(
        Some(&json_obj(&[
            ("ok", Value::Bool(true)),
            ("apiVersion", Value::from(HIVED_API_VERSION)),
            ("buildHash", Value::from(hived_build_hash())),
            ("team", Value::from("team-a")),
            ("tmuxWindowId", Value::from("@9")),
        ])),
        "team-a",
    ));
    assert!(hived_identity_matches(
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
fn test_hived_identity_refuses_another_hive_home_before_reading_the_build() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = EnvGuard::new();
    env.set("HIVE_HOME", tmp.path());
    let home = tmp.path().to_string_lossy().into_owned();
    let identity = |build: &str, home: &str| {
        json_obj(&[
            ("ok", Value::Bool(true)),
            ("apiVersion", Value::from(HIVED_API_VERSION)),
            ("buildHash", Value::from(build)),
            ("team", Value::from("team-a")),
            ("hiveHome", Value::from(home)),
        ])
    };
    assert_eq!(
        hived_identity(Some(&identity(hived_build_hash(), &home)), "team-a"),
        HivedIdentity::Matches
    );
    // A trailing slash is the same home.
    assert_eq!(
        hived_identity(
            Some(&identity(hived_build_hash(), &format!("{home}/"))),
            "team-a"
        ),
        HivedIdentity::Matches
    );
    assert_eq!(
        hived_identity(Some(&identity("stale", &home)), "team-a"),
        HivedIdentity::Restart
    );
    assert_eq!(hived_identity(None, "team-a"), HivedIdentity::Restart);
    // Another home is refused whatever the build says — even this one.
    assert_eq!(
        hived_identity(
            Some(&identity(hived_build_hash(), "/elsewhere/.hive")),
            "team-a"
        ),
        HivedIdentity::ForeignHome("/elsewhere/.hive".to_string())
    );
    assert_eq!(
        hived_identity(Some(&identity("stale", "/elsewhere/.hive")), "team-a"),
        HivedIdentity::ForeignHome("/elsewhere/.hive".to_string())
    );
    // A hived that reports no home (an older build) is restarted as before.
    let mut unhomed = identity("stale", &home);
    unhomed.shift_remove("hiveHome");
    assert_eq!(
        hived_identity(Some(&unhomed), "team-a"),
        HivedIdentity::Restart
    );
}

/// `ensure_hived` against a hooked ping: `identity` is what the socket
/// answers before a start, `after_start` once the popen hook has run.
/// Returns the result and the popen / cleanup_socket call counts.
fn ensure_hived_against(
    identity: Map<String, Value>,
    after_start: Map<String, Value>,
) -> (Result<Option<i32>>, usize, usize) {
    let run_tmp = tempfile::Builder::new()
        .prefix("hens")
        .tempdir_in("/tmp")
        .unwrap();
    let run_dir = run_tmp.path().to_path_buf();
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cleanups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ping_started = Arc::clone(&started);
    let popen_started = Arc::clone(&started);
    let popen_spawns = Arc::clone(&spawns);
    let cleanup_count = Arc::clone(&cleanups);
    let _guard = testhook::install(Hook {
        run_dir: Some(Arc::new(move |_ws| run_dir.clone())),
        request_ping: Some(Arc::new(move |_ws| {
            if ping_started.load(Ordering::SeqCst) {
                Some(after_start.clone())
            } else {
                Some(identity.clone())
            }
        })),
        cleanup_socket: Some(Arc::new(move |_ws| {
            cleanup_count.fetch_add(1, Ordering::SeqCst);
        })),
        popen: Some(Arc::new(move |_command, _stderr| {
            popen_spawns.fetch_add(1, Ordering::SeqCst);
            popen_started.store(true, Ordering::SeqCst);
            4242
        })),
        ..Default::default()
    });
    let result = ensure_hived("/tmp/ws-ensure", "team-a", "dev:3", "@99");
    (
        result,
        spawns.load(Ordering::SeqCst),
        cleanups.load(Ordering::SeqCst),
    )
}

#[test]
fn test_ensure_hived_restarts_a_stale_hived_of_the_same_home() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = EnvGuard::new();
    env.set("HIVE_HOME", tmp.path());
    let home = tmp.path().to_string_lossy().into_owned();
    let identity = |build: &str| {
        json_obj(&[
            ("ok", Value::Bool(true)),
            ("apiVersion", Value::from(HIVED_API_VERSION)),
            ("buildHash", Value::from(build)),
            ("team", Value::from("team-a")),
            ("hiveHome", Value::from(home.clone())),
        ])
    };
    let (result, spawns, cleanups) =
        ensure_hived_against(identity("stale"), identity(hived_build_hash()));
    assert_eq!(result.unwrap(), Some(4242));
    assert_eq!(spawns, 1);
    assert_eq!(cleanups, 1);

    // Already this build: nothing to do.
    let (result, spawns, _) =
        ensure_hived_against(identity(hived_build_hash()), identity(hived_build_hash()));
    assert_eq!(result.unwrap(), None);
    assert_eq!(spawns, 0);
}

#[test]
fn test_ensure_hived_refuses_a_hived_of_another_home_and_starts_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = EnvGuard::new();
    env.set("HIVE_HOME", tmp.path());
    let foreign = json_obj(&[
        ("ok", Value::Bool(true)),
        ("apiVersion", Value::from(HIVED_API_VERSION)),
        ("buildHash", Value::from("stale")),
        ("team", Value::from("team-a")),
        ("hiveHome", Value::from("/elsewhere/.hive")),
    ]);
    let (result, spawns, cleanups) = ensure_hived_against(foreign.clone(), foreign);
    let err = result.unwrap_err().to_string();
    assert!(err.contains("/tmp/ws-ensure"), "{err}");
    assert!(err.contains("/elsewhere/.hive"), "{err}");
    assert!(
        err.contains(&tmp.path().to_string_lossy().into_owned()),
        "{err}"
    );
    assert_eq!(spawns, 0);
    assert_eq!(cleanups, 0);
}

#[test]
fn test_handle_request_ping_returns_hived_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = EnvGuard::new();
    env.set("HIVE_HOME", tmp.path());
    let (response, keep_running) = handle_request(
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
        (
            "hiveHome",
            Value::from(tmp.path().to_string_lossy().into_owned()),
        ),
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

    let (response, keep_running) = handle_request(
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

    let (response, keep_running) = handle_request(
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

    // No pane, no client: the leader is never asked and the CLI is told so.
    let (response, keep_running) = handle_request(
        "/tmp/ws",
        "team-a",
        "dev:3",
        "@99",
        "2026-04-17T00:00:00Z",
        &json_obj(&[("action", Value::from("connect-grok"))]),
    );

    assert!(keep_running);
    assert_eq!(
        response,
        json_obj(&[("ok", Value::Bool(true)), ("connected", Value::Bool(false))])
    );
    assert_eq!(*connected.lock().unwrap(), vec!["%5".to_string()]);
}

#[test]
fn test_handle_request_send_defaults_to_the_hived_team_and_writes_the_bus_event() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    bus::init_workspace(&workspace).unwrap();
    let resolved: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let handed: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let resolved_sink = Arc::clone(&resolved);
    let handed_sink = Arc::clone(&handed);
    let ws_hook = workspace.to_string_lossy().to_string();
    let hook = Hook {
        resolve_live_agent: Some(Arc::new(move |team, _agent| {
            resolved_sink.lock().unwrap().push(team.to_string());
            let team = Team {
                name: team.to_string(),
                workspace: ws_hook.clone(),
                tmux_session: "dev".to_string(),
                tmux_window: "dev:0".to_string(),
                ..Default::default()
            };
            Ok((team, fake_agent("b", "%9", "claude")))
        })),
        check_send_gate: Some(Arc::new(|_target| Ok(()))),
        agent_send: Some(Arc::new(move |_agent, text, sender| {
            handed_sink
                .lock()
                .unwrap()
                .push((text.to_string(), sender.to_string()));
            Ok("udsWriteAccepted".to_string())
        })),
        ..Default::default()
    };
    let _guard = testhook::install(hook);

    // No `team` in the request: the hived's own team is the default.
    let (response, keep_running) = handle_request(
        &workspace.to_string_lossy(),
        "team-a",
        "dev:3",
        "@99",
        "2026-04-17T00:00:00Z",
        &json_obj(&[
            ("action", Value::from("send")),
            ("senderAgent", Value::from("a")),
            ("targetAgent", Value::from("b")),
            ("body", Value::from("  ship it  ")),
        ]),
    );

    assert!(keep_running);
    assert_eq!(response["ok"], Value::Bool(true));
    assert_eq!(response["to"], Value::from("b"));
    let seq = response["seq"].as_i64().unwrap();
    assert!(seq > 0);
    assert_eq!(*resolved.lock().unwrap(), vec!["team-a".to_string()]);
    let events = bus::read_all_events(&workspace).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, seq);
    assert_eq!(events[0].from, "a");
    assert_eq!(events[0].to, "b");
    assert_eq!(events[0].body, "ship it");
    let handed = handed.lock().unwrap();
    assert_eq!(handed.len(), 1);
    let (envelope, sender_label) = &handed[0];
    assert_eq!(sender_label, "team-a.a");
    assert_eq!(envelope, "<HIVE from=a to=b>\nship it\n</HIVE>");
}

#[test]
fn test_handle_request_node_dispatch_carries_no_sender() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    bus::init_workspace(&workspace).unwrap();
    let handed: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let handed_sink = Arc::clone(&handed);
    let mut hook = Hook::default();
    wire_send(&mut hook, &workspace);
    hook.agent_send = Some(Arc::new(move |_agent, text, sender| {
        handed_sink
            .lock()
            .unwrap()
            .push((text.to_string(), sender.to_string()));
        Ok("udsWriteAccepted".to_string())
    }));
    let _guard = testhook::install(hook);

    // The wire shape: action `node-dispatch`, no `senderAgent` key at all.
    let (response, keep_running) = handle_request(
        &workspace.to_string_lossy(),
        "team-a",
        "dev:3",
        "@99",
        "2026-04-17T00:00:00Z",
        &json_obj(&[
            ("action", Value::from("node-dispatch")),
            ("targetAgent", Value::from("b")),
            ("body", Value::from("task nd-0123456789ab\ndo it")),
            (
                "artifact",
                Value::from("/ws/artifacts/tasks/b-nd-0123456789ab.md"),
            ),
        ]),
    );

    assert!(keep_running);
    assert_eq!(response["ok"], Value::Bool(true));
    assert_eq!(response["to"], Value::from("b"));
    let seq = response["seq"].as_i64().unwrap();
    let events = bus::read_all_events(&workspace).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, seq);
    assert_eq!(events[0].from, "");
    assert_eq!(events[0].to, "b");
    let handed = handed.lock().unwrap();
    assert_eq!(handed.len(), 1);
    assert_eq!(handed[0].1, "team-a");
    assert!(
        handed[0].0.starts_with("<HIVE to=b artifact="),
        "{}",
        handed[0].0
    );
}

#[test]
fn test_handle_request_doctor_embeds_hived_identity_and_defaults_the_team() {
    let asked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&asked);
    let hook = Hook {
        team_load: Some(Arc::new(move |name| {
            sink.lock().unwrap().push(name.to_string());
            Ok(fake_team(name, vec![fake_agent("v", "%1", "codex")]))
        })),
        agent_is_alive: Some(Arc::new(|_a| true)),
        member_runtime_payload: Some(Arc::new(|_p, _r| json_obj(&[("alive", Value::Bool(true))]))),
        ..Default::default()
    };
    let _guard = testhook::install(hook);

    // No `team` in the request: the hived's own team is the default.
    let (response, keep_running) = handle_request(
        "/tmp/ws",
        "team-a",
        "dev:3",
        "@99",
        "2026-04-17T00:00:00Z",
        &json_obj(&[
            ("action", Value::from("doctor")),
            ("agent", Value::from("v")),
        ]),
    );

    assert!(keep_running);
    assert_eq!(*asked.lock().unwrap(), vec!["team-a".to_string()]);
    assert_eq!(response["ok"], Value::Bool(true));
    assert_eq!(response["team"], Value::from("team-a"));
    assert_eq!(response["agent"], Value::from("v"));
    assert_eq!(response["alive"], Value::Bool(true));
    // The identity block a doctor reader uses to tell which hived answered.
    assert_eq!(
        response["hived"],
        Value::Object(json_obj(&[
            ("pid", Value::from(getpid())),
            ("started_at", Value::from("2026-04-17T00:00:00Z")),
            ("code_hash", Value::from(hived_build_hash())),
        ]))
    );
}

#[test]
fn test_handle_request_reports_a_failing_handler_without_retiring_the_loop() {
    let hook = Hook {
        team_load: Some(Arc::new(|name| {
            Err(anyhow::anyhow!("no such team '{name}'"))
        })),
        ..Default::default()
    };
    let _guard = testhook::install(hook);

    // A `team` in the request overrides the hived's own team.
    let (response, keep_running) = handle_request(
        "/tmp/ws",
        "team-a",
        "dev:3",
        "@99",
        "2026-04-17T00:00:00Z",
        &json_obj(&[
            ("action", Value::from("team-runtime")),
            ("team", Value::from("ghost")),
        ]),
    );

    assert!(keep_running);
    assert_eq!(
        response,
        json_obj(&[
            ("ok", Value::Bool(false)),
            ("error", Value::from("no such team 'ghost'")),
        ])
    );
}

#[test]
fn test_start_hived_spawns_current_exe_with_hived_argv() {
    let captured: SpawnSink = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    let hook = Hook {
        current_exe: Some(Arc::new(|| "/tmp/fake-hive".to_string())),
        popen: Some(Arc::new(move |command, stderr_path| {
            sink.lock()
                .unwrap()
                .push((command.to_vec(), stderr_path.to_path_buf()));
            4321
        })),
        ..Default::default()
    };
    let _guard = testhook::install(hook);

    let pid = start_hived("/tmp/ws", "team-a", "dev:3", "@99");

    assert_eq!(pid, Some(4321));
    let captured = captured.lock().unwrap();
    assert_eq!(
        captured[0].0,
        vec![
            "/tmp/fake-hive".to_string(),
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
fn test_registry_visible_requires_the_entry_under_this_hive_home() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = EnvGuard::new();
    env.set("HIVE_HOME", tmp.path());
    let entry = crate::registry::entry_path("team-a").unwrap();

    let refused = registry_visible("team-a").unwrap_err();
    assert!(
        refused.contains(&entry.to_string_lossy().into_owned()),
        "{refused}"
    );
    assert!(
        refused.contains(&tmp.path().to_string_lossy().into_owned()),
        "{refused}"
    );
    assert!(registry_visible("../escape").is_err());

    fs::create_dir_all(entry.parent().unwrap()).unwrap();
    fs::write(&entry, "{}").unwrap();
    assert_eq!(registry_visible("team-a"), Ok(()));

    // Another home does not see it.
    let other = tempfile::tempdir().unwrap();
    env.set("HIVE_HOME", other.path());
    assert!(registry_visible("team-a").is_err());
}

#[test]
fn test_run_spawned_hived_refuses_a_team_missing_from_its_registry() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = EnvGuard::new();
    env.set("HIVE_HOME", tmp.path());
    let _guard = testhook::install(Hook {
        ignore_sigint: Some(Arc::new(|| panic!("must not reach the loop"))),
        hived_loop: Some(Arc::new(|_, _, _, _| panic!("must not reach the loop"))),
        ..Default::default()
    });
    let exit_code = run_spawned_hived(&[
        "--hived".to_string(),
        "/tmp/ws".to_string(),
        "team-a".to_string(),
        "dev:3".to_string(),
        "@99".to_string(),
    ]);
    assert_eq!(exit_code, 2);
}

#[test]
fn test_run_spawned_hived_ignores_sigint_and_runs_loop() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = EnvGuard::new();
    env.set("HIVE_HOME", tmp.path());
    let entry = crate::registry::entry_path("team-a").unwrap();
    fs::create_dir_all(entry.parent().unwrap()).unwrap();
    fs::write(&entry, "{}").unwrap();
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

    let exit_code = run_spawned_hived(&[
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
    let mut state = ReexecState {
        last_code_check_at: 5.0,
        ..Default::default()
    };

    assert_eq!(stale_disk_build_hash_for_reexec(&mut state, 10.0), None);
    assert_eq!(state.candidate_hash.as_deref(), Some("new-hash"));
    assert_eq!(stale_disk_build_hash_for_reexec(&mut state, 14.9), None);
    assert_eq!(
        stale_disk_build_hash_for_reexec(&mut state, 15.0),
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

    assert_eq!(stale_disk_build_hash_for_reexec(&mut state, 10.0), None);
    assert!(state.candidate_hash.is_none());
}

#[test]
fn test_try_acquire_reexec_lock_returns_inheritable_lock_fd() {
    let _guard = testhook::install(Hook::default());
    let tmp = tempfile::tempdir().unwrap();
    let lock_fd = try_acquire_reexec_lock(&tmp.path().to_string_lossy());
    let fd = lock_fd.expect("lock fd");
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert_eq!(flags & libc::FD_CLOEXEC, 0); // inheritable
    release_reexec_lock_fd(lock_fd);
}

#[test]
fn test_try_acquire_reexec_lock_returns_none_when_lock_is_busy() {
    let _guard = testhook::install(Hook::default());
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().to_string_lossy().to_string();
    let lock_path = lock_path(&workspace);
    fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let cpath = CString::new(lock_path.as_os_str().as_bytes()).unwrap();
    let held_fd = unsafe { libc::open(cpath.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644) };
    assert!(held_fd >= 0);
    assert_eq!(unsafe { libc::flock(held_fd, libc::LOCK_EX) }, 0);

    assert_eq!(try_acquire_reexec_lock(&workspace), None);

    unsafe {
        libc::flock(held_fd, libc::LOCK_UN);
        libc::close(held_fd);
    }
}

#[test]
fn test_reexec_hived_stops_monitor_closes_socket_and_execs() {
    let _env = EnvGuard::cleared(&[HIVED_REEXEC_LOCK_ENV]);
    let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let lock_sink = Arc::clone(&calls);
    let release_sink = Arc::clone(&calls);
    let cleanup_sink = Arc::clone(&calls);
    let execv_sink = Arc::clone(&calls);
    let hook = Hook {
        current_exe: Some(Arc::new(|| "/tmp/fake-hive".to_string())),
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
                std::env::var(HIVED_REEXEC_LOCK_ENV).unwrap_or_default()
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

    let replacement = reexec_hived(
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
            "execv /tmp/fake-hive --hived /ws team-a dev:3 @99 env=42".to_string(),
            "release Some(42)".to_string(),
        ]
    );
    assert!(std::env::var(HIVED_REEXEC_LOCK_ENV).is_err());
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

    let replacement = reexec_hived(
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
    let _env = EnvGuard::cleared(&[HIVED_REEXEC_LOCK_ENV]);
    let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let release_sink = Arc::clone(&calls);
    let cleanup_sink = Arc::clone(&calls);
    let open_sink = Arc::clone(&calls);
    let open_calls = Arc::clone(&calls);
    let hook = Hook {
        current_exe: Some(Arc::new(|| "/tmp/fake-hive".to_string())),
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

    let replacement = reexec_hived(
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
    let installed = get_output_busy_monitor().expect("monitor restored");
    assert!(Arc::ptr_eq(&installed, &monitor));
    assert!(std::env::var(HIVED_REEXEC_LOCK_ENV).is_err());
    set_output_busy_monitor(None);
}

#[test]
fn test_cleanup_socket_if_owner_skips_foreign_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().to_string_lossy().to_string();
    write_hived_owner_impl(
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

    cleanup_socket_if_owner(&workspace, "mine");

    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn test_hived_loop_retires_orphan_before_idle_tick() {
    let mut env = EnvGuard::cleared(&[HIVED_REEXEC_LOCK_ENV]);
    let tmp = tempfile::tempdir().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    let workspace = tmp.path().to_string_lossy().to_string();
    let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let events: EventSink = Arc::new(Mutex::new(Vec::new()));
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
            write_hived_owner_impl(workspace, pid, started_at, token);
            write_hived_owner_impl(workspace, pid + 1, started_at, "foreign");
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

    hived_loop(&workspace, "team-a", "dev:3", "@99");

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
fn test_open_server_socket_relocates_and_links_for_a_long_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp
        .path()
        .join("w".repeat(crate::devlog::max_socket_path_len()));
    let workspace = workspace.to_string_lossy().to_string();
    let sock = socket_path(&workspace);
    let link = socket_link_path(&workspace);
    assert_ne!(
        sock, link,
        "a workspace this deep cannot host its socket in tree"
    );
    assert!(sock.as_os_str().len() <= crate::devlog::max_socket_path_len());

    let server = open_server_socket(&workspace).unwrap();
    assert!(sock.exists(), "real socket bound at {}", sock.display());
    assert_eq!(
        fs::read_link(&link).unwrap(),
        sock,
        "run/hived.sock points at it"
    );
    let mode = fs::metadata(sock.parent().unwrap())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);

    // a client derives the same path from the workspace alone and gets through
    let ws_client = workspace.clone();
    let client = thread::spawn(move || request_hived(&ws_client, &action_payload("ping"), 5.0));
    let conn = server
        .accept_timeout(5.0)
        .expect("client connected to the relocated socket");
    let mut conn = conn;
    let mut buf = [0u8; 65536];
    loop {
        match conn.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => continue, // drain until the client half-closes
        }
    }
    (&conn).write_all(b"{\"ok\": true}\n").unwrap();
    drop(conn);
    let response = client
        .join()
        .unwrap()
        .expect("ping answered over the relocated socket");
    assert_eq!(response.get("ok"), Some(&Value::Bool(true)));

    server.close();
    cleanup_socket_impl(&workspace);
    assert!(!sock.exists());
    assert!(
        fs::symlink_metadata(&link).is_err(),
        "the symlink is cleaned up too"
    );
    assert!(
        !sock.parent().unwrap().exists(),
        "the relocated directory does not linger under /tmp"
    );
}

#[test]
fn test_hived_loop_reports_a_socket_bind_failure_instead_of_exiting_silently() {
    let mut env = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    env.set(HIVED_REEXEC_LOCK_ENV, "78");
    let workspace = tmp.path().to_string_lossy().to_string();
    let events: DebugEventSink = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let released: Arc<Mutex<Vec<Option<i32>>>> = Arc::new(Mutex::new(Vec::new()));
    let release_sink = Arc::clone(&released);
    let hook = Hook {
        open_server_socket: Some(Arc::new(|_workspace| {
            Err(anyhow::anyhow!("File name too long (os error 63)"))
        })),
        release_reexec_lock_fd: Some(Arc::new(move |fd| release_sink.lock().unwrap().push(fd))),
        cleanup_socket: Some(Arc::new(|_workspace| {})),
        is_tmux_window_alive: Some(Arc::new(|_id| false)),
        make_busy_monitor: Some(Arc::new(|_session| None)),
        notify_debug_emit: Some(Arc::new(move |_ws, event, fields| {
            sink.lock().unwrap().push((
                event.to_string(),
                fields
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect(),
            ));
        })),
        ..Default::default()
    };
    let _guard = testhook::install(hook);

    hived_loop(&workspace, "team-a", "", "");

    let events = events.lock().unwrap();
    let names: Vec<&str> = events.iter().map(|(e, _)| e.as_str()).collect();
    assert_eq!(names, vec!["hived.start", "hived.socket_bind_failed"]);
    let fields = &events[1].1;
    let get = |k: &str| {
        fields
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(get("team"), Some(Value::from("team-a")));
    assert!(get("socket")
        .unwrap()
        .as_str()
        .unwrap()
        .ends_with("run/hived.sock"));
    assert!(get("error").unwrap().as_str().unwrap().contains("too long"));
    // the inherited reexec lock is not leaked on the failure path either
    assert_eq!(*released.lock().unwrap(), vec![Some(78)]);
}

#[test]
fn test_hived_loop_releases_inherited_reexec_lock_after_socket_ready() {
    let mut env = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    env.set(HIVED_REEXEC_LOCK_ENV, "77");
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

    hived_loop(&workspace, "team-a", "", "");

    assert_eq!(
        *calls.lock().unwrap(),
        vec![
            format!("open {workspace}"),
            "release Some(77)".to_string(),
            "server.close".to_string(),
            format!("cleanup {workspace}"),
        ]
    );
    assert!(std::env::var(HIVED_REEXEC_LOCK_ENV).is_err());
}

#[test]
fn test_send_request_budget_covers_native_submission() {
    // The CLI socket budget is strictly longer than the worst-case
    // native transport submission: a valid slow acceptance must never
    // surface as `hived unavailable`.
    let native = crate::adapters::claude_sessions::SUBMIT_TIMEOUT
        .max(crate::adapters::codex_app_server::SUBMIT_TIMEOUT)
        .max(crate::adapters::grok_leader::SUBMIT_TIMEOUT);
    assert!(send_request_timeout() > native);
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
    let (held_tx, held_rx) = std::sync::mpsc::channel();
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
        // Hold the reply past the ping budget: a send that borrowed the
        // ping timeout would have hung up by the time this is written.
        let drained = std::time::Instant::now();
        thread::sleep(Duration::from_secs_f64(SOCKET_RETRY_INTERVAL * 3.0));
        let _ = (&conn).write_all(b"{\"ok\": true, \"seq\": 1, \"delivery\": \"queued\"}\n");
        let _ = held_tx.send(drained.elapsed().as_secs_f64());
    });

    let response = request_send("/tmp/ws-x", "t", "a", "b", "hello", "");

    // The server held the reply past the ping budget (SOCKET_RETRY_INTERVAL)
    // but inside the send budget; the oracle is the reply arriving at all.
    let held_for = held_rx.recv().unwrap();
    assert!(held_for < send_request_timeout());
    let response = response.expect("delayed acceptance must not be dropped");
    assert_eq!(response["delivery"], Value::from("queued"));
}

#[test]
fn test_serve_connection_round_trips_ping_over_a_real_socket() {
    // No handle_request hook: the wire framing meets the real dispatcher.
    let _guard = testhook::install(Hook::default());
    let tmp = short_workspace();
    let workspace = tmp.path().to_string_lossy().to_string();
    let server = Arc::new(open_server_socket(&workspace).unwrap());

    let ws_serve = workspace.clone();
    let server_serve = Arc::clone(&server);
    let serve_thread = thread::spawn(move || {
        serve_requests(
            server_serve.as_ref(),
            &ws_serve,
            "team-a",
            "dev:3",
            "@99",
            "2026-04-17T00:00:00Z",
            5.0,
        )
    });

    let ping = request_hived(&workspace, &action_payload("ping"), SOCKET_READY_TIMEOUT)
        .expect("ping must be answered over the socket");
    assert_eq!(ping["ok"], Value::Bool(true));
    assert_eq!(ping["apiVersion"], Value::from(HIVED_API_VERSION));
    assert_eq!(ping["buildHash"], Value::from(hived_build_hash()));
    assert_eq!(ping["tmuxWindowId"], Value::from("@99"));
    assert_eq!(
        ping["hived"]["started_at"],
        Value::from("2026-04-17T00:00:00Z")
    );
    assert!(
        !SHUTDOWN.load(Ordering::SeqCst),
        "ping must keep the loop running"
    );

    // A truncated frame is answered as an unknown action and the loop
    // stays up for the next client.
    let mut raw = UnixStream::connect(socket_path(&workspace)).unwrap();
    raw.set_read_timeout(Some(Duration::from_secs_f64(SOCKET_READY_TIMEOUT)))
        .unwrap();
    raw.write_all(b"{\"action\": \"ping\"\n").unwrap();
    raw.shutdown(std::net::Shutdown::Write).unwrap();
    let mut reply = String::new();
    raw.read_to_string(&mut reply).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&reply).unwrap(),
        serde_json::json!({"ok": false, "error": "unknown action"})
    );
    assert!(!SHUTDOWN.load(Ordering::SeqCst));

    let again = request_hived(&workspace, &action_payload("ping"), SOCKET_READY_TIMEOUT)
        .expect("the loop must survive a malformed frame");
    assert_eq!(again["tmuxWindowId"], Value::from("@99"));

    let bye = request_hived(
        &workspace,
        &action_payload("shutdown"),
        SOCKET_READY_TIMEOUT,
    );
    assert_eq!(bye, Some(json_obj(&[("ok", Value::Bool(true))])));
    // The loop is parked in accept: one more client wakes it to notice
    // the shutdown flag instead of waiting out the accept timeout.
    let _ = request_hived(&workspace, &action_payload("ping"), SOCKET_READY_TIMEOUT);
    let keep_running = serve_thread.join().unwrap();
    assert!(!keep_running);
    // The wake-up ping's handler thread retires on its own; the loop does
    // not wait for it.
    let settle = std::time::Instant::now();
    while requests_in_flight() && settle.elapsed().as_secs_f64() < SOCKET_READY_TIMEOUT {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !requests_in_flight(),
        "handler thread still counted in flight"
    );
    server.close();
    cleanup_socket_impl(&workspace);
}

// ---- send payload ------------------------------------------------------

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

fn send_payload_for_test(
    workspace: &Path,
    sender: &str,
    target: &str,
    body: &str,
    artifact: &str,
) -> Map<String, Value> {
    send_payload(
        &workspace.to_string_lossy(),
        "team-x",
        SendOrigin::Member(sender),
        target,
        body,
        artifact,
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
    hook.agent_send = Some(Arc::new(|_agent, _text, _sender| {
        Ok("udsWriteAccepted".to_string())
    }));
    let _guard = testhook::install(hook);

    let payload = send_payload_for_test(&workspace, "a", "b", "hi", "");

    assert_eq!(payload["ok"], Value::Bool(true));
    assert!(payload["seq"].as_i64().unwrap() > 0);
    assert!(!payload.contains_key("delivery"));
    // exactly one durable event: the send itself — no observations, no
    // tracking
    assert_eq!(bus::read_all_events(&workspace).unwrap().len(), 1);
}

#[test]
fn test_send_hands_the_transport_the_qualified_author() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    bus::init_workspace(&workspace).unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let mut hook = Hook::default();
    wire_send(&mut hook, &workspace);
    hook.agent_send = Some(Arc::new(move |_agent, _text, sender| {
        sink.lock().unwrap().push(sender.to_string());
        Ok("udsWriteAccepted".to_string())
    }));
    let _guard = testhook::install(hook);

    send_payload_for_test(&workspace, "yoyo", "orch", "hi", "");
    send_payload_for_test(&workspace, "other.guest", "orch", "hi", "");
    send_payload_for_test(&workspace, "ccd.desk", "orch", "hi", "");

    // bare member names get the team prefix; guests and ccd senders are
    // already qualified and travel as-is
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            "team-x.yoyo".to_string(),
            "other.guest".to_string(),
            "ccd.desk".to_string()
        ]
    );
}

#[test]
fn test_refused_send_fails_synchronously() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    bus::init_workspace(&workspace).unwrap();
    let mut hook = Hook::default();
    wire_send(&mut hook, &workspace);
    hook.agent_send = Some(Arc::new(|_agent, _text, _sender| {
        Err(DeliveryError("no channel".to_string()))
    }));
    let _guard = testhook::install(hook);

    let payload = send_payload_for_test(&workspace, "a", "b", "hi", "");

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
    hook.agent_send = Some(Arc::new(move |_agent, text, _sender| {
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
    let ids: HashSet<i64> = results.iter().map(|r| r["seq"].as_i64().unwrap()).collect();
    assert_eq!(ids.len(), 3);
}

#[test]
fn test_node_dispatch_writes_a_senderless_row_and_a_from_less_envelope() {
    // A `hive node run` dispatch rides the normal transport (member
    // resolution, send gate, hand-off) but has no sender: the ledger row's
    // from_agent is empty, the envelope carries no `from`, and the
    // transport's origin label is the team itself.
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    bus::init_workspace(&workspace).unwrap();
    let gated: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let handed: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let gated_sink = Arc::clone(&gated);
    let handed_sink = Arc::clone(&handed);
    let mut hook = Hook::default();
    wire_send(&mut hook, &workspace);
    hook.check_send_gate = Some(Arc::new(move |target| {
        gated_sink.lock().unwrap().push(target.name.clone());
        Ok(())
    }));
    hook.agent_send = Some(Arc::new(move |_agent, text, sender| {
        handed_sink
            .lock()
            .unwrap()
            .push((text.to_string(), sender.to_string()));
        Ok("udsWriteAccepted".to_string())
    }));
    let _guard = testhook::install(hook);

    let payload = send_payload(
        &workspace.to_string_lossy(),
        "team-x",
        SendOrigin::Node,
        "b",
        "task nd-0123456789ab\nreview it",
        "/ws/artifacts/tasks/b-nd-0123456789ab.md",
    )
    .unwrap();

    assert_eq!(payload["ok"], Value::Bool(true));
    assert_eq!(payload["to"], Value::from("b"));
    assert_eq!(*gated.lock().unwrap(), vec!["b".to_string()]);
    let events = bus::read_all_events(&workspace).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, payload["seq"].as_i64().unwrap());
    assert_eq!(events[0].from, "");
    assert_eq!(events[0].to, "b");
    assert_eq!(events[0].body, "task nd-0123456789ab\nreview it");
    assert_eq!(
        events[0].artifact,
        "/ws/artifacts/tasks/b-nd-0123456789ab.md"
    );
    let handed = handed.lock().unwrap();
    assert_eq!(handed.len(), 1);
    assert_eq!(
        handed[0].0,
        "<HIVE to=b artifact=/ws/artifacts/tasks/b-nd-0123456789ab.md>\ntask nd-0123456789ab\nreview it\n</HIVE>"
    );
    assert_eq!(handed[0].1, "team-x");
}

#[test]
fn test_send_with_an_empty_sender_is_not_a_node_dispatch() {
    // Only the explicit node mode drops `from`; a malformed member send
    // with no sender still renders the normal envelope.
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    bus::init_workspace(&workspace).unwrap();
    let handed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let handed_sink = Arc::clone(&handed);
    let mut hook = Hook::default();
    wire_send(&mut hook, &workspace);
    hook.agent_send = Some(Arc::new(move |_agent, text, _sender| {
        handed_sink.lock().unwrap().push(text.to_string());
        Ok("accepted".to_string())
    }));
    let _guard = testhook::install(hook);

    send_payload_for_test(&workspace, "", "b", "hi", "");
    assert_eq!(
        *handed.lock().unwrap(),
        vec!["<HIVE from= to=b>\nhi\n</HIVE>".to_string()]
    );
}

// ---- retained-shell liveness -------------------------------------------

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
    let rt = agent_runtime_payload("%9", None);
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
    let rt = agent_runtime_payload("%9", None);
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
    let rt = agent_runtime_payload("%9", None);
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
    let rt = agent_runtime_payload("%9", None);
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

    let payload = send_payload_for_test(&workspace, "w", "v", "hi", "");

    assert_eq!(payload["ok"], Value::Bool(false));
    let error = payload["error"].as_str().unwrap();
    assert!(error.contains("transport refused"));
    assert!(error.contains("cli_exited"));
    // the send event is durable: recoverable from the bus by seq
    assert_eq!(bus::read_all_events(&workspace).unwrap().len(), 1);
    assert!(payload["seq"].as_i64().unwrap() > 0);
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

        let hook = Hook {
            check_send_gate: Some(Arc::new(|_target| Ok(()))),
            resolve_live_agent: Some(Arc::new({
                let cli = cli_name.to_string();
                move |_team, _agent| Ok((fake_team("team-x", vec![]), fake_agent("v", "%9", &cli)))
            })),
            ..Default::default()
        };
        let _guard = testhook::install(hook);

        let payload = send_payload_for_test(&workspace, "w", "v", "hi", "");
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
    assert_eq!(idle_notify_agent_panes("t"), vec!["%1".to_string()]);
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
    let diag = doctor_payload("/tmp/ws", "t", "v", false, None).unwrap();
    assert_eq!(diag["alive"], Value::Bool(true));
    assert_eq!(diag["cliAlive"], Value::Bool(false));
}

/// A live interactive Claude session as the sessions registry lists it.
fn live_session(session_id: &str, pid: i32) -> crate::adapters::claude_sessions::ClaudeSession {
    crate::adapters::claude_sessions::ClaudeSession {
        name: "desk".to_string(),
        pid,
        cwd: "/repo".to_string(),
        kind: String::new(),
        socket_path: "/tmp/desk.sock".to_string(),
        session_id: session_id.to_string(),
        title: String::new(),
    }
}

/// A joined desktop Claude is drawn as a read-only viewer pane: no CLI on
/// the tty, so the pane probe says cli_exited while the member's
/// engine — its session — is alive and reachable.
fn mirror_pane_hook(member: Agent) -> Hook {
    Hook {
        team_load: Some(Arc::new(move |_name| {
            Ok(fake_team("t", vec![member.clone()]))
        })),
        member_runtime_payload: Some(Arc::new(|_p, _r| {
            json_obj(&[
                ("alive", Value::Bool(true)),
                ("cliAlive", Value::Bool(false)),
                ("busy", Value::Bool(false)),
                ("inputState", Value::from("offline")),
                ("inputReason", Value::from("cli_exited")),
            ])
        })),
        cs_list_sessions: Some(Arc::new(|| vec![live_session("sess-desk", 4242)])),
        cs_session_status: Some(Arc::new(|_pid| None)),
        ..Default::default()
    }
}

#[test]
fn test_team_runtime_reads_a_mirror_pane_member_off_its_live_session() {
    let mut member = fake_agent("orch", "%1", "claude");
    member.session_id = Some("sess-desk".to_string());
    let _guard = testhook::install(mirror_pane_hook(member));

    let payload = team_runtime_payload("t").unwrap();
    let rt = payload["members"]["orch"].as_object().unwrap();

    assert_eq!(rt["cliAlive"], Value::Bool(true));
    assert_eq!(rt["inputState"], Value::from("ready"));
    assert_eq!(rt["inputReason"], Value::from(""));
    assert_eq!(rt["sessionId"], Value::from("sess-desk"));
    assert_eq!(rt["_runtimeSource"], Value::from("claude_session"));
    assert_eq!(rt["alive"], Value::Bool(true)); // still the pane's own fact
}

#[test]
fn test_team_runtime_leaves_a_pane_member_with_no_live_session_dead() {
    let mut member = fake_agent("orch", "%1", "claude");
    member.session_id = Some("sess-gone".to_string());
    let _guard = testhook::install(mirror_pane_hook(member));

    let payload = team_runtime_payload("t").unwrap();
    let rt = payload["members"]["orch"].as_object().unwrap();

    assert_eq!(rt["cliAlive"], Value::Bool(false));
    assert_eq!(rt["inputReason"], Value::from("cli_exited"));
}

// ---- headless member runtime -------------------------------------------

fn headless_member(cli: &str, session_id: Option<&str>) -> Agent {
    Agent {
        team_name: "honey".to_string(),
        session_id: session_id.map(|s| s.to_string()),
        ..fake_agent("rex", "", cli)
    }
}

#[test]
fn test_headless_member_runtime_grok() {
    let hook = Hook {
        gl_runtime_for_key: Some(Arc::new(|key| {
            if key == "m-honey.rex" {
                Some(session_runtime(true, "ready"))
            } else {
                None
            }
        })),
        gl_read_session_key: Some(Arc::new(|_key| {
            Some(SessionRecord {
                session_id: "sid-g".to_string(),
                cwd: "/repo".to_string(),
            })
        })),
        ..Default::default()
    };
    let _guard = testhook::install(hook);

    let payload = headless_member_runtime(&headless_member("grok", Some("sid-1")));

    assert_eq!(payload["headless"], Value::Bool(true));
    assert_eq!(payload["alive"], Value::Bool(true));
    assert_eq!(payload["busy"], Value::Bool(true));
    assert_eq!(payload["sessionId"], Value::from("sid-g"));
}

#[test]
fn test_headless_member_runtime_unknown_engine() {
    let _guard = testhook::install(Hook::default());

    let payload = headless_member_runtime(&headless_member("codex", None));

    assert_eq!(payload["alive"], Value::Bool(false));
    assert_eq!(payload["inputState"], Value::from("unknown"));
}

// ---- the hived writer over the registry --------------------------------

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
    let mut env = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    let mut worker_row = Map::new();
    worker_row.insert("name".to_string(), Value::from("worker"));
    worker_row.insert("cwd".to_string(), Value::from("/old"));
    let mut validator_row = Map::new();
    validator_row.insert("name".to_string(), Value::from("validator"));
    assert_eq!(
        crate::registry::record_team("honey", "/ws", "123.0", &[worker_row, validator_row], "")
            .unwrap(),
        "written"
    );
    {
        // Team::load merges the live pane's #{pane_current_path} into the
        // agent; the worker has since `cd`-ed away from the row's "/old".
        let mut worker = fake_agent("worker", "%1", "claude");
        worker.cwd = "/fresh".to_string();
        let hook = writer_hook(
            writer_team(vec![worker, fake_agent("validator", "%2", "codex")]),
            &[("%1", "sid-w"), ("%2", "sid-v")],
        );
        let _guard = testhook::install(hook);

        write_registry_backfill("/ws", "honey");
    }

    let entry = crate::registry::load("honey").unwrap();
    let by_name = roster_by_name("honey");
    assert_eq!(by_name["worker"]["sessionId"], Value::from("sid-w"));
    assert_eq!(by_name["worker"]["cwd"], Value::from("/fresh"));
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
        write_registry_backfill("/ws", "honey");
    }
    let by_name2 = roster_by_name("honey");
    assert_eq!(by_name2["validator"]["sessionId"], Value::from("sid-v")); // dead member survives
    assert_eq!(by_name2["worker"]["sessionId"], Value::from("sid-w2"));
}

#[test]
fn test_writer_without_registry_entry_writes_nothing() {
    // Observation never creates a roster: membership belongs to the CLI.
    let mut env = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    let hook = writer_hook(
        writer_team(vec![fake_agent("worker", "%1", "claude")]),
        &[("%1", "sid-w")],
    );
    let _guard = testhook::install(hook);

    write_registry_backfill("/ws", "honey");

    assert!(crate::registry::load("honey").is_none());
}

// ---- status tick (the team status bar's pane and window options) -------

/// A workspace with a bus, a listing of `(pane, role)` panes, recorders for
/// the pane and window options the tick writes, and the window every pane
/// reports as its own.
struct StatusEnv {
    _tmp: tempfile::TempDir,
    workspace: PathBuf,
    pane_writes: OptionWrites,
    window_writes: OptionWrites,
    state: StatusTickState,
}

fn status_env(panes: &[(&str, &str)], busy: Option<bool>) -> (StatusEnv, testhook::Guard) {
    status_env_in_windows(panes, busy, &[])
}

/// `status_env` whose panes report the window of their `(pane, window)`
/// row in *windows* (`dev:1` for the rest).
fn status_env_in_windows(
    panes: &[(&str, &str)],
    busy: Option<bool>,
    windows: &[(&str, &str)],
) -> (StatusEnv, testhook::Guard) {
    let windows: HashMap<String, String> = windows
        .iter()
        .map(|(pane, window)| (pane.to_string(), window.to_string()))
        .collect();
    let tmp = tempfile::tempdir().unwrap();
    let workspace = bus::init_workspace(tmp.path().join("ws")).unwrap();
    let listing: Vec<PaneInfo> = panes
        .iter()
        .map(|(pane, role)| PaneInfo {
            pane_id: (*pane).to_string(),
            role: (*role).to_string(),
            ..Default::default()
        })
        .collect();
    let pane_writes: OptionWrites = Default::default();
    let window_writes: OptionWrites = Default::default();
    let pane_sink = Arc::clone(&pane_writes);
    let window_sink = Arc::clone(&window_writes);
    let hook = Hook {
        list_panes_all: Some(Arc::new(move || listing.clone())),
        native_daemon_busy: Some(Arc::new(move |_pane| busy)),
        set_pane_option: Some(Arc::new(move |pane, key, value| {
            pane_sink
                .lock()
                .unwrap()
                .push((pane.to_string(), key.to_string(), value.to_string()));
        })),
        set_window_option: Some(Arc::new(move |target, key, value| {
            window_sink.lock().unwrap().push((
                target.to_string(),
                key.to_string(),
                value.to_string(),
            ));
        })),
        get_pane_window_target: Some(Arc::new(move |pane| {
            Some(
                windows
                    .get(pane)
                    .cloned()
                    .unwrap_or_else(|| "dev:1".to_string()),
            )
        })),
        ..Default::default()
    };
    let guard = testhook::install(hook);
    (
        StatusEnv {
            _tmp: tmp,
            workspace,
            pane_writes,
            window_writes,
            state: StatusTickState::default(),
        },
        guard,
    )
}

fn status_members(rows: &[(&str, &str)]) -> Vec<(String, Map<String, Value>)> {
    rows.iter()
        .map(|(name, pane)| {
            let mut row = Map::new();
            row.insert("name".to_string(), Value::from(*name));
            row.insert("pane".to_string(), Value::from(*pane));
            (name.to_string(), row)
        })
        .collect()
}

fn tick_status(env: &mut StatusEnv, members: &[(String, Map<String, Value>)], now: i64) {
    status_tick(
        &env.workspace.to_string_lossy(),
        members,
        None,
        &mut env.state,
        now,
    );
}

fn drain(sink: &OptionWrites) -> Vec<(String, String, String)> {
    std::mem::take(&mut *sink.lock().unwrap())
}

fn row(pane: &str, key: &str, value: &str) -> (String, String, String) {
    (pane.to_string(), key.to_string(), value.to_string())
}

#[test]
fn test_status_tick_writes_busy_and_unread_only_on_edges() {
    let (mut env, _guard) = status_env(&[("%1", "agent")], Some(true));
    let members = status_members(&[("sage", "%1")]);

    tick_status(&mut env, &members, 1_000);
    assert_eq!(
        drain(&env.pane_writes),
        vec![row("%1", "hive-busy", "1"), row("%1", "hive-unread", "0")]
    );

    tick_status(&mut env, &members, 1_001);
    assert_eq!(drain(&env.pane_writes), Vec::new());

    testhook::update(|h| stub_app_server_busy(h, Some(false)));
    tick_status(&mut env, &members, 1_002);
    assert_eq!(drain(&env.pane_writes), vec![row("%1", "hive-busy", "0")]);
}

#[test]
fn test_status_tick_clears_unread_when_the_member_goes_busy() {
    let (mut env, _guard) = status_env(&[("%1", "agent")], Some(false));
    let members = status_members(&[("sage", "%1")]);
    unread_pending().lock().unwrap().insert("%1".to_string());

    tick_status(&mut env, &members, 1_000);
    assert_eq!(
        drain(&env.pane_writes),
        vec![row("%1", "hive-busy", "0"), row("%1", "hive-unread", "1")]
    );

    // The turn that reads the message: busy consumes the pending mark…
    testhook::update(|h| stub_app_server_busy(h, Some(true)));
    tick_status(&mut env, &members, 1_001);
    assert_eq!(
        drain(&env.pane_writes),
        vec![row("%1", "hive-busy", "1"), row("%1", "hive-unread", "0")]
    );

    // …so idle again is not unread again.
    testhook::update(|h| stub_app_server_busy(h, Some(false)));
    tick_status(&mut env, &members, 1_002);
    assert_eq!(drain(&env.pane_writes), vec![row("%1", "hive-busy", "0")]);
}

#[test]
fn test_status_tick_skips_mirror_and_terminal_panes() {
    let (mut env, _guard) = status_env(&[("%1", "mirror"), ("%2", "terminal")], Some(true));
    let members = status_members(&[("orch", "%1"), ("shell", "%2")]);
    unread_pending().lock().unwrap().insert("%1".to_string());

    tick_status(&mut env, &members, 1_000);

    assert_eq!(drain(&env.pane_writes), Vec::new());
    // No engine pane, no ticker anchor: the parked mirror's window never
    // gets one.
    assert_eq!(drain(&env.window_writes), Vec::new());
    // A message to a pane without a chip is not pending unread.
    assert!(!unread_pending().lock().unwrap().contains("%1"));
}

#[test]
fn test_status_tick_anchors_the_ticker_on_an_engine_pane_not_the_parked_mirror() {
    let (mut env, _guard) = status_env_in_windows(
        &[("%1", "mirror"), ("%2", "agent")],
        Some(false),
        &[("%1", "honey:9"), ("%2", "dev:1")],
    );
    bus::write_send_event(&env.workspace, "orch", "sage", "hi", "").unwrap();
    // The mirror is bound first; the ticker still lands on the engine
    // pane's window.
    let members = status_members(&[("orch", "%1"), ("sage", "%2")]);

    tick_status(&mut env, &members, 1_000);

    let writes = drain(&env.window_writes);
    assert_eq!(writes.len(), 1);
    assert_eq!(
        (writes[0].0.as_str(), writes[0].1.as_str()),
        ("dev:1", "@hive-ticker")
    );
}

#[test]
fn test_status_tick_writes_nothing_on_an_empty_listing() {
    let (mut env, _guard) = status_env(&[], Some(true));
    let members = status_members(&[("sage", "%1")]);
    unread_pending().lock().unwrap().insert("%1".to_string());

    tick_status(&mut env, &members, 1_000);

    assert_eq!(drain(&env.pane_writes), Vec::new());
    assert_eq!(drain(&env.window_writes), Vec::new());
    // A tmux failure, not an empty server: nothing is forgotten either.
    assert!(unread_pending().lock().unwrap().remove("%1"));
}

#[test]
fn test_status_tick_writes_the_ticker_once_per_text() {
    let (mut env, _guard) = status_env(&[("%1", "agent")], Some(false));
    let members = status_members(&[("sage", "%1")]);
    bus::write_send_event(&env.workspace, "orch", "sage", "first #1", "").unwrap();
    bus::write_send_event(&env.workspace, "sage", "orch", "second", "").unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let events = bus::latest_send_events(&env.workspace, TICKER_ROWS).unwrap();

    tick_status(&mut env, &members, now);
    let text = ticker_text(&events, now);
    assert!(
        text.starts_with("sage → orch · now · \"second\"   │   orch → sage · now · \"first ##1\""),
        "{text}"
    );
    assert_eq!(
        drain(&env.window_writes),
        vec![row("dev:1", "@hive-ticker", &text)]
    );

    tick_status(&mut env, &members, now);
    assert_eq!(drain(&env.window_writes), Vec::new());

    // The age bucket moved: one write, with the new text.
    tick_status(&mut env, &members, now + 120);
    assert_eq!(
        drain(&env.window_writes),
        vec![row(
            "dev:1",
            "@hive-ticker",
            &ticker_text(&events, now + 120)
        )]
    );
}

#[test]
fn test_ticker_text_escapes_hashes_clips_the_body_and_orders_newest_first() {
    // 2023-11-14T22:13:20Z, and stamps that many seconds before it.
    let now = 1_700_000_000;
    let stamp = |age: i64| -> String {
        match age {
            10 => "2023-11-14T22:13:10Z",
            120 => "2023-11-14T22:11:20Z",
            7_200 => "2023-11-14T20:13:20Z",
            200_000 => "2023-11-12T14:40:00Z",
            _ => unreachable!(),
        }
        .to_string()
    };
    let event = |from: &str, to: &str, body: &str, created_at: String| bus::Event {
        seq: 0,
        from: from.to_string(),
        to: to.to_string(),
        created_at,
        body: body.to_string(),
        artifact: String::new(),
    };

    assert_eq!(
        ticker_head(&"x".repeat(100)),
        format!("{}…", "x".repeat(80))
    );
    assert_eq!(ticker_head("a #tag\n\n  b\tc"), "a ##tag b c");
    assert_eq!(ticker_age(&stamp(10), now), "now");
    assert_eq!(ticker_age(&stamp(120), now), "2m");
    assert_eq!(ticker_age(&stamp(7_200), now), "2h");
    assert_eq!(ticker_age(&stamp(200_000), now), "2d");
    assert_eq!(ticker_age("yesterday", now), "?");
    assert_eq!(
        ticker_text(
            &[
                event("b", "a", "hi", stamp(10)),
                event("a", "b", "yo #1", stamp(120)),
            ],
            now
        ),
        "b → a · now · \"hi\"   │   a → b · 2m · \"yo ##1\""
    );
}

#[test]
fn test_send_marks_the_target_pane_unread() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    bus::init_workspace(&workspace).unwrap();
    let mut hook = Hook::default();
    wire_send(&mut hook, &workspace);
    hook.resolve_live_agent = Some(Arc::new(|_team, _agent| {
        Ok((fake_team("team-x", vec![]), fake_agent("b", "%4", "claude")))
    }));
    hook.agent_send = Some(Arc::new(
        |_agent, _text, _sender| Ok("accepted".to_string()),
    ));
    let _guard = testhook::install(hook);
    let pending = || -> Vec<String> { unread_pending().lock().unwrap().iter().cloned().collect() };

    send_payload_for_test(&workspace, "a", "b", "hi", "");
    assert_eq!(pending(), vec!["%4".to_string()]);

    unread_pending().lock().unwrap().clear();
    testhook::update(|h| {
        h.agent_send = Some(Arc::new(|_agent, _text, _sender| {
            Err(DeliveryError("no channel".to_string()))
        }));
    });
    let refused = send_payload_for_test(&workspace, "a", "b", "hi", "");
    assert_eq!(refused["ok"], Value::Bool(false));
    assert_eq!(pending(), Vec::<String>::new());
}

// ---- codex record reaping is scoped to the hived's own tmux server -------

fn reap_calls(state: SuperState) -> Vec<String> {
    let (_guard, calls) = super_env(state);
    codex_supervisor_tick("/tmp/ws", "t");
    let calls = calls.lock().unwrap();
    calls
        .iter()
        .filter(|call| call.starts_with("clear "))
        .cloned()
        .collect()
}

#[test]
fn test_supervisor_reaps_only_records_of_its_own_server() {
    let mut state = super_state();
    state.own_socket = Some("/x/tmux-501/e2e".to_string());
    state.recorded = vec!["%1".to_string(), "%dead".to_string(), "%3".to_string()];
    state.record_sockets = HashMap::from([
        ("%dead".to_string(), "/x/tmux-501/e2e".to_string()),
        ("%3".to_string(), "/tmp/tmux-501/default".to_string()),
    ]);
    // %3 is absent from this (private) server but lives on the default one
    assert_eq!(reap_calls(state), vec!["clear %dead".to_string()]);
}

#[test]
fn test_supervisor_on_private_server_leaves_legacy_records_alone() {
    let mut state = super_state();
    state.own_socket = Some("/x/tmux-501/e2e".to_string());
    state.recorded = vec!["%1".to_string(), "%dead".to_string()];
    // no tmuxSocket on %dead: written by the pre-field binary
    assert_eq!(reap_calls(state), Vec::<String>::new());
}

#[test]
fn test_supervisor_on_default_server_reaps_legacy_records() {
    let mut state = super_state();
    state.recorded = vec!["%1".to_string(), "%dead".to_string()];
    assert_eq!(reap_calls(state), vec!["clear %dead".to_string()]);
}

#[test]
fn test_supervisor_reaps_nothing_when_its_own_server_is_unknown() {
    let mut state = super_state();
    state.own_socket = None;
    state.recorded = vec!["%1".to_string(), "%dead".to_string(), "%3".to_string()];
    state.record_sockets = HashMap::from([("%3".to_string(), "/tmp/tmux-501/default".to_string())]);
    assert_eq!(reap_calls(state), Vec::<String>::new());
}

#[test]
fn test_supervisor_reaps_own_record_spelled_through_private_tmp() {
    let mut state = super_state();
    state.own_socket = Some("/private/tmp/tmux-501/default".to_string());
    state.recorded = vec!["%dead".to_string()];
    state.record_sockets =
        HashMap::from([("%dead".to_string(), "/tmp/tmux-501/default".to_string())]);
    assert_eq!(reap_calls(state), vec!["clear %dead".to_string()]);
}
