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
use serde_json::{json, Map, Value};

use crate::adapters::claude_bg::PaneJob;
use crate::adapters::grok_leader::PromptId;
use crate::adapters::grok_leader::SessionRecord;
use crate::agent::{Agent, DeliveryError, TurnHandle};
use crate::runtime_snapshot::RuntimeSnapshot;
use crate::team::Team;
use crate::testenv::EnvGuard;
use crate::{bus, devlog};

use super::testhook::{self, FakeAdapter, Hook};
use super::*;
use crate::adapters::claude_bg::EngineSession;
use crate::adapters::claude_view::PaneView;
use crate::adapters::codex_app_server::{AuthVerdict, DaemonOutcome, ThreadRuntime, TurnResult};
use crate::adapters::grok_leader::{PromptResult, SessionRuntime};

/// Collectors the hook closures push into: `(target, option, value)` tmux
/// writes, `(event, payload)` notify emits, `(argv, stderr path)` spawns and
/// `(event, fields)` debug emits.
type OptionWrites = Arc<Mutex<Vec<(String, String, String)>>>;
type EventSink = Arc<Mutex<Vec<(String, Map<String, Value>)>>>;
type SpawnSink = Arc<Mutex<Vec<(Vec<String>, PathBuf)>>>;
type DebugEventSink = Arc<Mutex<Vec<(String, Vec<(String, Value)>)>>>;
use crate::tmux::{PaneInfo, WindowExtra};
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

    // A claude bg job reports no turn end over any RPC, and a claude
    // member is no workflow node: no turn evidence, like any other engine
    // hive cannot ask.
    assert!(null_reason("k", "claude").contains("no turn evidence"));

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
        6
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
    let panes = hooked_list_panes_all();
    claude_view_tick("/tmp/ws", "probe", &members, &mut env.state, &panes);
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

#[test]
fn test_cleanup_reads_a_bound_launchs_grace_from_its_alias_not_the_leaders_pidfile() {
    // A terminal handoff binds a launch leader to the member by alias
    // before the roster row lands. The leader may have run for hours: its
    // pidfile is no newborn clock, the alias is.
    let mut env = reap_env(true);
    env._env.set("GROK_HOME", env.tmp.path());
    let hive_dir = env.tmp.path().join("hive");
    fs::create_dir_all(&hive_dir).unwrap();
    *env.keys.lock().unwrap() = vec!["m-honey.rex".to_string()];
    write_pidfile(env.tmp.path(), "m-honey.rex", 999.0);
    write_pidfile(env.tmp.path(), "l-ab12", 999.0);
    let alias = hive_dir.join("m-honey.rex.alias");
    fs::write(&alias, "l-ab12").unwrap();

    // the bind is fresh and the roster write is in flight: kept
    cleanup_dead_daemons("/tmp/ws", "honey");
    assert!(env.calls.lock().unwrap().is_empty());

    // the bind is old and the roster never listed the member: orphan
    backdate(&alias, 999.0);
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
    record_sockets: HashMap<String, String>, // pane -> tmuxSocket; absent = names no server
    own_socket: Option<String>,
    threads: HashMap<String, String>,
    cwds: HashMap<String, String>,
    roster_cwd: String,
    daemon_alive: bool,
    auth: AuthVerdict,
    spawn: DaemonOutcome,
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
        cwds: HashMap::new(),
        roster_cwd: String::new(),
        daemon_alive: true,
        auth: AuthVerdict::Fresh,
        spawn: DaemonOutcome::Started,
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
            .map(|(pane, agent, cli)| Agent {
                cwd: s.roster_cwd.clone(),
                ..fake_agent(agent, pane, cli)
            })
            .collect();
        Ok(fake_team("t", agents))
    };
    let clear_sink = Arc::clone(&calls);
    let unsubscribe_sink = Arc::clone(&calls);
    let drop_sink = Arc::clone(&calls);
    let spawn_sink = Arc::clone(&calls);
    let send_sink = Arc::clone(&calls);
    let emit_sink = Arc::clone(&calls);
    let s_recorded = Arc::clone(&state);
    let s_sockets = Arc::clone(&state);
    let s_own = Arc::clone(&state);
    let s_threads = Arc::clone(&state);
    let s_cwds = Arc::clone(&state);
    let s_alive = Arc::clone(&state);
    let s_stale = Arc::clone(&state);
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
        cas_unsubscribe_thread: Some(Arc::new(move |thread_id| {
            unsubscribe_sink
                .lock()
                .unwrap()
                .push(format!("unsubscribe {thread_id}"))
        })),
        cas_thread_id_for_pane: Some(Arc::new(move |pane| s_threads.threads.get(pane).cloned())),
        cas_pane_cwd: Some(Arc::new(move |pane| s_cwds.cwds.get(pane).cloned())),
        cas_daemon_alive: Some(Arc::new(move || s_alive.daemon_alive)),
        cas_daemon_auth_verdict: Some(Arc::new(move || s_stale.auth)),
        cas_drop_client: Some(Arc::new(move || {
            drop_sink.lock().unwrap().push("drop_client".to_string())
        })),
        cas_ensure_daemon: Some(Arc::new(move || {
            spawn_sink.lock().unwrap().push("spawn".to_string());
            s_spawn.spawn
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
fn test_supervisor_prunes_records_of_dead_panes_and_releases_their_threads() {
    let mut state = super_state();
    state.recorded = vec!["%1".to_string(), "%dead".to_string()];
    let own = state
        .own_socket
        .clone()
        .expect("the fixture names its server");
    state.record_sockets.insert("%dead".to_string(), own);
    state
        .threads
        .insert("%dead".to_string(), "tid-dead".to_string());
    let (_guard, calls) = super_env(state);
    codex_supervisor_tick("/tmp/ws", "t");
    let calls = calls.lock().unwrap();
    // The dead pane's thread is unsubscribed before its record goes, so the
    // daemon's idle unload can take the thread; the live member's stays.
    let dead = calls.iter().position(|c| c == "unsubscribe tid-dead");
    let cleared = calls.iter().position(|c| c == "clear %dead");
    assert!(dead.is_some() && cleared.is_some(), "{calls:?}");
    assert!(dead < cleared, "{calls:?}");
    assert!(!calls.contains(&"clear %1".to_string()));
    assert!(!calls.contains(&"unsubscribe tid-1".to_string()));
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
fn test_supervisor_replaces_live_daemon_with_stale_auth() {
    let mut state = super_state();
    state.auth = AuthVerdict::Stale;
    let (_guard, calls) = super_env(state);
    codex_supervisor_tick("/tmp/ws", "t");
    let calls = calls.lock().unwrap();
    assert_eq!(
        *calls,
        vec![
            "emit codex.daemon.auth_stale {}".to_string(),
            "spawn".to_string(),
            "drop_client".to_string(),
            "emit codex.daemon.respawn {\"ok\":true}".to_string(),
        ]
    );
}

#[test]
fn test_supervisor_hands_an_unknown_baseline_to_the_locked_spawn() {
    // No baseline: the tick itself writes nothing and asks nothing; the
    // locked ensure_daemon settles it and keeps the daemon. The client
    // stays: it holds the tracked turns a workflow runner still reads.
    let mut state = super_state();
    state.auth = AuthVerdict::Unknown;
    state.spawn = DaemonOutcome::Reused;
    let (_guard, calls) = super_env(state);
    codex_supervisor_tick("/tmp/ws", "t");
    let calls = calls.lock().unwrap();
    assert_eq!(
        *calls,
        vec![
            "spawn".to_string(),
            "emit codex.daemon.auth_settle {\"ok\":true}".to_string(),
        ]
    );
}

#[test]
fn test_supervisor_drops_its_client_only_when_settling_started_a_daemon() {
    // The daemon's own answer under the lock said another account: it
    // was replaced, so the client of the old one is dropped.
    let mut state = super_state();
    state.auth = AuthVerdict::Unknown;
    state.spawn = DaemonOutcome::Started;
    let (_guard, calls) = super_env(state);
    codex_supervisor_tick("/tmp/ws", "t");
    let calls = calls.lock().unwrap();
    assert_eq!(
        *calls,
        vec![
            "spawn".to_string(),
            "drop_client".to_string(),
            "emit codex.daemon.auth_settle {\"ok\":true}".to_string(),
        ]
    );
}

#[test]
fn test_supervisor_keeps_its_client_when_a_stale_verdict_finds_nothing_to_replace() {
    // The lock-free verdict said stale, but under the lock the daemon had
    // already been replaced by another process and is reused: no drop.
    let mut state = super_state();
    state.auth = AuthVerdict::Stale;
    state.spawn = DaemonOutcome::Reused;
    let (_guard, calls) = super_env(state);
    codex_supervisor_tick("/tmp/ws", "t");
    let calls = calls.lock().unwrap();
    assert!(!calls.contains(&"drop_client".to_string()));
    assert!(calls.contains(&"emit codex.daemon.respawn {\"ok\":true}".to_string()));
}

#[test]
fn test_supervisor_ignores_auth_baseline_of_a_dead_daemon() {
    // Dead is dead: the respawn path runs once, without an auth_stale
    // verdict on a daemon that is not there to be stale.
    let mut state = super_state();
    state.daemon_alive = false;
    state.auth = AuthVerdict::Stale;
    let (_guard, calls) = super_env(state);
    codex_supervisor_tick("/tmp/ws", "t");
    let calls = calls.lock().unwrap();
    assert!(!calls.iter().any(|c| c.contains("auth_stale")));
    assert_eq!(calls.iter().filter(|c| *c == "spawn").count(), 1);
}

#[test]
fn test_supervisor_reattaches_retained_shell() {
    let mut state = super_state();
    state.cli_process = HashMap::new(); // CLI exited; pane keeps its shell
    let (_guard, calls) = super_env(state);
    codex_supervisor_tick("/tmp/ws", "t");
    let calls = calls.lock().unwrap();
    assert!(calls.contains(&"send %1 hive codex resume 'tid-1'".to_string()));
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
    idle_notify_enabled: bool,
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
        idle_notify_enabled: Some(Arc::new(move || idle_notify_enabled)),
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
    let snap = TickSnapshot::collect();
    idle_notify_tick(
        "team-a",
        "dev",
        state,
        Some(monitor),
        now,
        "",
        None,
        None,
        &snap,
    );
}

fn idle_tick_dbg(
    state: &mut HashMap<String, IdleRecord>,
    monitor: &IdleBusyMonitor,
    now: f64,
    debug_state: &mut NotifyDebugState,
) {
    let snap = TickSnapshot::collect();
    idle_notify_tick(
        "team-a",
        "dev",
        state,
        Some(monitor),
        now,
        "",
        Some(debug_state),
        None,
        &snap,
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
fn test_idle_notify_reconciles_selected_notify_even_when_setting_off() {
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
fn test_idle_notify_enabled_follows_the_notify_idle_setting_and_defaults_on() {
    // No hook installed: the seam reads `$HIVE_HOME/settings.json`.
    let tmp = tempfile::TempDir::new().unwrap();
    let mut env = crate::testenv::EnvGuard::new();
    env.set("HIVE_HOME", tmp.path().join(".hive"));

    assert!(super::seams::hooked_idle_notify_enabled());
    crate::settings::set_setting("notify.idle", serde_json::Value::Bool(false)).unwrap();
    assert!(!super::seams::hooked_idle_notify_enabled());
    crate::settings::set_setting("notify.idle", serde_json::Value::Bool(true)).unwrap();
    assert!(super::seams::hooked_idle_notify_enabled());
    // Only a literal false turns it off; a stray value is not a switch.
    crate::settings::set_setting("notify.idle", serde_json::Value::from("off")).unwrap();
    assert!(super::seams::hooked_idle_notify_enabled());
}

#[test]
fn test_idle_notify_skips_and_clears_state_when_setting_off() {
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

    let snap = TickSnapshot::collect();
    assert_eq!(
        idle_notify_agent_panes("team-a", &snap),
        vec!["%1".to_string()]
    );
}

// ---- socket server / lifecycle -----------------------------------------

/// A fake hived's side of the admission preflight: read the client's
/// `admit` line and answer it admitted at this api version.
fn admit_preflight(conn: &mut UnixStream) {
    use std::io::BufRead;
    let mut line = String::new();
    std::io::BufReader::new(&*conn)
        .read_line(&mut line)
        .unwrap();
    let preflight: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(preflight["action"], ADMIT_ACTION);
    (&*conn)
        .write_all(
            format!("{{\"ok\":true,\"admitted\":true,\"apiVersion\":{HIVED_API_VERSION}}}\n")
                .as_bytes(),
        )
        .unwrap();
}

/// A handler thread drops its lease after the client has its reply; wait
/// for that before asserting on the admission counts.
fn settle_leases() {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while requests_in_flight() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(2));
    }
    assert!(!requests_in_flight(), "a handler lease never settled");
}

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
    fn wait_readable(&self, timeout: f64) -> bool {
        thread::sleep(Duration::from_secs_f64(timeout));
        false
    }
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
    let slow_client =
        thread::spawn(move || request_admitted(&ws_slow, &action_payload("send"), 10.0).ok());
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
        request_ping: Some(Arc::new(|_ws, _timeout| {
            Some(json_obj(&[("ok", Value::Bool(true))]))
        })),
        ..Default::default()
    };
    let _guard = testhook::install(hook);
    assert!(!socket_alive("/tmp/ws"));

    testhook::update(|h| {
        h.request_ping = Some(Arc::new(|_ws, _timeout| {
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
    let ping_count = std::sync::atomic::AtomicUsize::new(0);
    let _guard = testhook::install(Hook {
        run_dir: Some(Arc::new(move |_ws| run_dir.clone())),
        request_ping: Some(Arc::new(move |_ws, timeout| {
            if ping_started.load(Ordering::SeqCst) {
                assert_eq!(timeout, SOCKET_RETRY_INTERVAL);
                Some(after_start.clone())
            } else {
                assert_eq!(timeout, IDENTITY_PING_TIMEOUT);
                (ping_count.fetch_add(1, Ordering::SeqCst) == 0).then(|| identity.clone())
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

// Exercise the actual socket read budget while keeping restart side effects hooked.
fn ensure_hived_with_delayed_ping(delay: Option<Duration>) -> (Option<i32>, usize, usize) {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    let tmp = tempfile::Builder::new()
        .prefix("hping")
        .tempdir_in("/tmp")
        .unwrap();
    let run_dir = tmp.path().to_path_buf();
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cleanups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let identity = json_obj(&[
        ("ok", Value::Bool(true)),
        ("apiVersion", Value::from(HIVED_API_VERSION)),
        ("buildHash", Value::from(hived_build_hash())),
        ("team", Value::from("team-a")),
    ]);
    let ping_started = Arc::clone(&started);
    let popen_spawns = Arc::clone(&spawns);
    let cleanup_count = Arc::clone(&cleanups);
    let ready_identity = identity.clone();
    let _guard = testhook::install(Hook {
        run_dir: Some(Arc::new(move |_ws| run_dir.clone())),
        request_ping: Some(Arc::new(move |ws, timeout| {
            if ping_started.load(Ordering::SeqCst) {
                assert_eq!(timeout, SOCKET_RETRY_INTERVAL);
                Some(ready_identity.clone())
            } else {
                assert_eq!(timeout, IDENTITY_PING_TIMEOUT);
                request_ping_impl(ws, timeout)
            }
        })),
        cleanup_socket: Some(Arc::new(move |_ws| {
            cleanup_count.fetch_add(1, Ordering::SeqCst);
        })),
        popen: Some(Arc::new(move |_command, _stderr| {
            started.store(true, Ordering::SeqCst);
            popen_spawns.fetch_add(1, Ordering::SeqCst);
            4242
        })),
        ..Default::default()
    });
    let workspace = tmp.path().to_str().unwrap();
    let server = delay.map(|delay| {
        assert!(delay.as_secs_f64() > SOCKET_RETRY_INTERVAL);
        assert!(delay.as_secs_f64() < IDENTITY_PING_TIMEOUT);
        let listener = UnixListener::bind(socket_path(workspace)).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            stream.read_to_string(&mut request).unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&request).unwrap()["action"],
                "ping"
            );
            std::thread::sleep(delay);
            stream
                .write_all(serde_json::to_string(&identity).unwrap().as_bytes())
                .unwrap();
        })
    });
    if delay.is_none() {
        assert!(matches!(
            request_hived_answer(workspace, &action_payload("ping"), IDENTITY_PING_TIMEOUT),
            Err(RequestFailure::NoListener)
        ));
    }
    let result = ensure_hived(workspace, "team-a", "dev:3", "@99").unwrap();
    if let Some(server) = server {
        server.join().unwrap();
    }
    (
        result,
        spawns.load(Ordering::SeqCst),
        cleanups.load(Ordering::SeqCst),
    )
}

/// A hived identity this binary accepts as its own.
fn matching_identity() -> Map<String, Value> {
    json_obj(&[
        ("ok", Value::Bool(true)),
        ("apiVersion", Value::from(HIVED_API_VERSION)),
        ("buildHash", Value::from(hived_build_hash())),
        ("team", Value::from("team-a")),
    ])
}

/// A clock that jumps `step` seconds on every read, so a budget expires
/// without the test waiting for it.
fn stepping_clock(step: f64) -> Arc<dyn Fn() -> f64 + Send + Sync> {
    let now = Arc::new(Mutex::new(0.0f64));
    Arc::new(move || {
        let mut now = now.lock().unwrap();
        *now += step;
        *now
    })
}

#[test]
fn test_startup_lock_is_cloexec_and_reexec_lock_is_inheritable() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().to_string_lossy().to_string();

    // `start_hived` spawns the hived while this lock is held: a descriptor
    // that rode into the child would hold the lock for the child's life.
    let lock = StartupLock::acquire(&workspace).unwrap();
    let fd = lock.raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(flags >= 0, "F_GETFD on the startup lock fd failed");
    assert_eq!(
        flags & libc::FD_CLOEXEC,
        libc::FD_CLOEXEC,
        "the startup lock fd must be close-on-exec"
    );
    drop(lock);
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_GETFD) },
        -1,
        "dropping the startup lock must close its fd"
    );

    // The reexec handoff fd keeps the opposite contract: it rides through
    // execv into the generation that releases it.
    let reexec_fd = try_acquire_reexec_lock_impl(&workspace).unwrap();
    let flags = unsafe { libc::fcntl(reexec_fd, libc::F_GETFD) };
    assert!(flags >= 0, "F_GETFD on the reexec handoff fd failed");
    assert_eq!(
        flags & libc::FD_CLOEXEC,
        0,
        "the reexec handoff fd must stay inheritable"
    );
    release_reexec_lock_fd_impl(Some(reexec_fd));

    // A flock that comes back non-zero never reaches the spawn, and the
    // fd is closed on that path too.
    let spawns = Arc::new(Mutex::new(0usize));
    let counted = Arc::clone(&spawns);
    let _guard = testhook::install(Hook {
        flock_nb: Some(Arc::new(|_fd| Err(libc::ENOLCK))),
        popen: Some(Arc::new(move |_command, _stderr| {
            *counted.lock().unwrap() += 1;
            4242
        })),
        ..Default::default()
    });
    let err = ensure_hived(&workspace, "team-a", "dev:3", "@99")
        .unwrap_err()
        .to_string();
    assert!(err.contains("hived.lock"), "{err}");
    assert_eq!(*spawns.lock().unwrap(), 0);
}

#[test]
fn test_startup_lock_timeout_and_eintr_do_not_spawn() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().to_string_lossy().to_string();
    let lock_path = lock_path(&workspace);
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();

    // A budget for a lock a live holder keeps: a real second open file
    // description on the same path, exactly what a leaked fd looks like.
    let holder = try_acquire_reexec_lock_impl(&workspace).expect("holder takes the lock");
    let spawns = Arc::new(Mutex::new(0usize));
    let cleanups = Arc::new(Mutex::new(0usize));
    let count_spawn = |spawns: &Arc<Mutex<usize>>| {
        let counted = Arc::clone(spawns);
        Arc::new(move |_command: &[String], _stderr: &std::path::Path| {
            *counted.lock().unwrap() += 1;
            4242
        }) as testhook::Popen
    };
    let count_cleanup = |cleanups: &Arc<Mutex<usize>>| {
        let counted = Arc::clone(cleanups);
        Arc::new(move |_ws: &str| {
            *counted.lock().unwrap() += 1;
        }) as testhook::S1<()>
    };
    {
        let _guard = testhook::install(Hook {
            monotonic: Some(stepping_clock(10.0)),
            popen: Some(count_spawn(&spawns)),
            cleanup_socket: Some(count_cleanup(&cleanups)),
            ..Default::default()
        });
        let err = ensure_hived(&workspace, "team-a", "dev:3", "@99")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&lock_path.display().to_string()),
            "the error names the lock path: {err}"
        );
        assert!(
            err.contains(&format!("{STARTUP_LOCK_TIMEOUT}s")),
            "the error names the budget: {err}"
        );
        // flock never says who holds the lock; owner.json's pid is not it.
        assert!(!err.contains("pid"), "{err}");
    }
    release_reexec_lock_fd_impl(Some(holder));
    assert_eq!(*spawns.lock().unwrap(), 0);
    assert_eq!(*cleanups.lock().unwrap(), 0);

    // EINTR retries on the same deadline and then succeeds: one spawn.
    let flocks = Arc::new(Mutex::new(0usize));
    {
        let counted = Arc::clone(&flocks);
        let pings = Arc::new(Mutex::new(0usize));
        let _guard = testhook::install(Hook {
            flock_nb: Some(Arc::new(move |fd| {
                let mut n = counted.lock().unwrap();
                *n += 1;
                if *n <= 2 {
                    return Err(libc::EINTR);
                }
                flock_nb_impl(fd)
            })),
            request_ping: Some(Arc::new(move |_ws, _timeout| {
                let mut n = pings.lock().unwrap();
                *n += 1;
                (*n > 1).then(matching_identity)
            })),
            popen: Some(count_spawn(&spawns)),
            cleanup_socket: Some(count_cleanup(&cleanups)),
            ..Default::default()
        });
        assert_eq!(
            ensure_hived(&workspace, "team-a", "dev:3", "@99").unwrap(),
            Some(4242)
        );
    }
    assert_eq!(*flocks.lock().unwrap(), 3);
    assert_eq!(*spawns.lock().unwrap(), 1);

    // EINTR all the way to the deadline: an error, still no spawn.
    let spawns = Arc::new(Mutex::new(0usize));
    let cleanups = Arc::new(Mutex::new(0usize));
    {
        let _guard = testhook::install(Hook {
            monotonic: Some(stepping_clock(10.0)),
            flock_nb: Some(Arc::new(|_fd| Err(libc::EINTR))),
            popen: Some(count_spawn(&spawns)),
            cleanup_socket: Some(count_cleanup(&cleanups)),
            ..Default::default()
        });
        let err = ensure_hived(&workspace, "team-a", "dev:3", "@99")
            .unwrap_err()
            .to_string();
        assert!(err.contains(&format!("{STARTUP_LOCK_TIMEOUT}s")), "{err}");
    }
    assert_eq!(*spawns.lock().unwrap(), 0);
    assert_eq!(*cleanups.lock().unwrap(), 0);

    // A non-retryable errno fails at once, with the OS error in the text.
    {
        let _guard = testhook::install(Hook {
            flock_nb: Some(Arc::new(|_fd| Err(libc::ENOLCK))),
            popen: Some(count_spawn(&spawns)),
            cleanup_socket: Some(count_cleanup(&cleanups)),
            ..Default::default()
        });
        let err = ensure_hived(&workspace, "team-a", "dev:3", "@99")
            .unwrap_err()
            .to_string();
        assert!(err.contains(&lock_path.display().to_string()), "{err}");
        assert!(
            err.contains(&std::io::Error::from_raw_os_error(libc::ENOLCK).to_string()),
            "{err}"
        );
    }
    assert_eq!(*spawns.lock().unwrap(), 0);
    assert_eq!(*cleanups.lock().unwrap(), 0);

    // The lock taken again after the stale generation stopped goes through
    // the same budget: its failure is the same loud error, and nothing spawns.
    let flocks = Arc::new(Mutex::new(0usize));
    {
        let counted = Arc::clone(&flocks);
        let stale = json_obj(&[
            ("ok", Value::Bool(true)),
            ("apiVersion", Value::from(HIVED_API_VERSION)),
            ("buildHash", Value::from("stale")),
            ("team", Value::from("team-a")),
        ]);
        let _guard = testhook::install(Hook {
            flock_nb: Some(Arc::new(move |fd| {
                let mut n = counted.lock().unwrap();
                *n += 1;
                if *n == 1 {
                    return flock_nb_impl(fd);
                }
                Err(libc::ENOLCK)
            })),
            request_ping: Some(Arc::new(move |_ws, _timeout| Some(stale.clone()))),
            popen: Some(count_spawn(&spawns)),
            cleanup_socket: Some(count_cleanup(&cleanups)),
            ..Default::default()
        });
        let err = ensure_hived(&workspace, "team-a", "dev:3", "@99")
            .unwrap_err()
            .to_string();
        assert!(err.contains(&lock_path.display().to_string()), "{err}");
    }
    assert_eq!(
        *flocks.lock().unwrap(),
        2,
        "the retake uses the same helper"
    );
    assert_eq!(*spawns.lock().unwrap(), 0);
    assert_eq!(*cleanups.lock().unwrap(), 0);
}

#[test]
fn test_startup_timeout_is_error_and_ready_clears_marker() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().to_string_lossy().to_string();

    // A hived that never answers a matching ping is an error, not Ok(pid):
    // the caller's request would otherwise be sent into a dead socket.
    let spawns = Arc::new(Mutex::new(0usize));
    {
        let counted = Arc::clone(&spawns);
        let _guard = testhook::install(Hook {
            monotonic: Some(stepping_clock(10.0)),
            request_ping: Some(Arc::new(|_ws, _timeout| None)),
            cleanup_socket: Some(Arc::new(|_ws| {})),
            popen: Some(Arc::new(move |_command, _stderr| {
                *counted.lock().unwrap() += 1;
                4242
            })),
            ..Default::default()
        });
        let err = ensure_hived(&workspace, "team-a", "dev:3", "@99")
            .unwrap_err()
            .to_string();
        assert!(err.contains("team-a"), "{err}");
        assert!(err.contains("matching ping"), "{err}");
    }
    assert_eq!(*spawns.lock().unwrap(), 1);

    // A ping that misses once and then matches is a success, on one spawn.
    let spawns = Arc::new(Mutex::new(0usize));
    {
        let counted = Arc::clone(&spawns);
        let pings = Arc::new(Mutex::new(0usize));
        let _guard = testhook::install(Hook {
            request_ping: Some(Arc::new(move |_ws, _timeout| {
                let mut n = pings.lock().unwrap();
                *n += 1;
                (*n > 2).then(matching_identity)
            })),
            cleanup_socket: Some(Arc::new(|_ws| {})),
            popen: Some(Arc::new(move |_command, _stderr| {
                *counted.lock().unwrap() += 1;
                4242
            })),
            ..Default::default()
        });
        assert_eq!(
            ensure_hived(&workspace, "team-a", "dev:3", "@99").unwrap(),
            Some(4242)
        );
    }
    assert_eq!(*spawns.lock().unwrap(), 1);

    // In the hived, the marker survives every startup barrier and goes
    // only once the owner file names this generation.
    let env = loop_probe_env("ok");
    let marker = asleep_marker_path(&env.workspace);
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, "{\"reason\":\"unwatched\"}\n").unwrap();
    let barriers: Arc<Mutex<Vec<(&str, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let at_open = Arc::clone(&barriers);
    let at_worker = Arc::clone(&barriers);
    let at_owner = Arc::clone(&barriers);
    let open_marker = marker.clone();
    let worker_marker = marker.clone();
    let owner_marker = marker.clone();
    testhook::update(|h| {
        h.open_server_socket = Some(Arc::new(move |_workspace| {
            at_open.lock().unwrap().push(("bind", open_marker.exists()));
            Ok(Box::new(RecServer {
                calls: Arc::new(Mutex::new(Vec::new())),
            }) as Box<dyn HivedServerApi>)
        }));
        h.start_request_server = Some(Arc::new(move |server| {
            at_worker
                .lock()
                .unwrap()
                .push(("worker", worker_marker.exists()));
            Ok(server)
        }));
        h.write_hived_owner = Some(Arc::new(move |ws, pid, started_at, token| {
            at_owner
                .lock()
                .unwrap()
                .push(("owner", owner_marker.exists()));
            write_hived_owner_impl(ws, pid, started_at, token);
        }));
        h.try_acquire_reexec_lock = Some(Arc::new(|_ws| Some(9)));
    });
    hived_loop(&env.workspace, "probe", "probe:1", "@1");
    assert_eq!(
        *barriers.lock().unwrap(),
        vec![("bind", true), ("worker", true), ("owner", true)]
    );
    assert!(!marker.exists(), "a ready hived clears the marker");
}

#[test]
fn test_failed_bind_preserves_unwatched_marker() {
    for failure in ["bind", "worker"] {
        let env = loop_probe_env("ok");
        let marker = asleep_marker_path(&env.workspace);
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        let bytes = "{\"reason\":\"unwatched\",\"at\":1730000000}\n";
        std::fs::write(&marker, bytes).unwrap();
        let owners = Arc::new(Mutex::new(0usize));
        let counted = Arc::clone(&owners);
        testhook::update(|h| {
            h.write_hived_owner = Some(Arc::new(move |ws, pid, started_at, token| {
                *counted.lock().unwrap() += 1;
                write_hived_owner_impl(ws, pid, started_at, token);
            }));
            h.try_acquire_reexec_lock = Some(Arc::new(|_ws| Some(9)));
            if failure == "bind" {
                h.open_server_socket = Some(Arc::new(|_workspace| {
                    Err(anyhow::anyhow!("File name too long (os error 63)"))
                }));
            } else {
                h.start_request_server = Some(Arc::new(|_server| {
                    Err(anyhow::anyhow!("cannot spawn the accept worker"))
                }));
            }
        });

        hived_loop(&env.workspace, "probe", "probe:1", "@1");

        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            bytes,
            "{failure}: the marker bytes are left as they were found"
        );
        assert_eq!(
            *owners.lock().unwrap(),
            0,
            "{failure}: nothing published an owner"
        );
        assert_eq!(
            display_events(&env, "hived.socket_bind_failed").len(),
            1,
            "{failure}"
        );

        // Fault cleared: the next start reaches ready and the marker goes.
        testhook::update(|h| {
            h.open_server_socket = Some(Arc::new(|_workspace| {
                Ok(Box::new(RecServer {
                    calls: Arc::new(Mutex::new(Vec::new())),
                }) as Box<dyn HivedServerApi>)
            }));
            h.start_request_server = Some(Arc::new(Ok));
        });
        hived_loop(&env.workspace, "probe", "probe:1", "@1");
        assert!(!marker.exists(), "{failure}");
        assert_eq!(*owners.lock().unwrap(), 1, "{failure}");
    }
}

#[test]
fn test_ensure_hived_does_not_restart_when_ping_takes_longer_than_retry_interval() {
    let (pid, spawns, cleanups) = ensure_hived_with_delayed_ping(Some(Duration::from_millis(250)));
    assert_eq!(pid, None);
    assert_eq!(spawns, 0);
    assert_eq!(cleanups, 0);
}

#[test]
fn test_ensure_hived_starts_when_identity_ping_is_not_sent() {
    let (pid, spawns, cleanups) = ensure_hived_with_delayed_ping(None);
    assert_eq!(pid, Some(4242));
    assert_eq!(spawns, 1);
    assert_eq!(cleanups, 1);
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
    let mut env = EnvGuard::new();
    env.set("HIVE_HOME", tmp.path().join("home"));
    crate::registry::record_team(
        "team-a",
        tmp.path().to_str().unwrap(),
        "123",
        &[json_obj(&[("name", Value::from("b"))])],
        "",
    )
    .unwrap();
    let workspace = tmp.path().join("ws");
    bus::init_workspace(&workspace).unwrap();
    let handed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let handed_sink = Arc::clone(&handed);
    let mut hook = Hook::default();
    wire_send(&mut hook, &workspace);
    hook.agent_send = Some(Arc::new(|_agent, _text, _sender| {
        panic!("a node dispatch is a tracked turn, never a plain send")
    }));
    hook.agent_dispatch_turn = Some(Arc::new(move |_agent, text| {
        handed_sink.lock().unwrap().push(text.to_string());
        Ok(TurnHandle::Codex {
            thread_id: "thr-b".to_string(),
            turn_id: "turn-1".to_string(),
        })
    }));
    let _guard = testhook::install(hook);

    // No dispatch id is no dispatch.
    let (response, keep_running) = handle_request(
        &workspace.to_string_lossy(),
        "team-a",
        "dev:3",
        "@99",
        "2026-04-17T00:00:00Z",
        &json_obj(&[
            ("action", Value::from("node-dispatch")),
            ("targetAgent", Value::from("b")),
            ("body", Value::from("task")),
        ]),
    );
    assert!(keep_running);
    assert_eq!(response["ok"], Value::Bool(false));
    assert!(bus::read_all_events(&workspace).unwrap().is_empty());

    // The wire shape: action `node-dispatch`, a `dispatchId`, no
    // `senderAgent` key at all.
    let (response, keep_running) = handle_request(
        &workspace.to_string_lossy(),
        "team-a",
        "dev:3",
        "@99",
        "2026-04-17T00:00:00Z",
        &json_obj(&[
            ("action", Value::from("node-dispatch")),
            ("dispatchId", Value::from("nd-0123456789ab")),
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
    assert!(
        handed[0].starts_with("<HIVE to=b artifact="),
        "{}",
        handed[0]
    );

    // The turn is held under the dispatch id from here: `node-result`
    // reads it running, then ended with the engine's word and text.
    let ask = |dispatch_id: &str| {
        handle_request(
            &workspace.to_string_lossy(),
            "team-a",
            "dev:3",
            "@99",
            "2026-04-17T00:00:00Z",
            &json_obj(&[
                ("action", Value::from("node-result")),
                ("dispatchId", Value::from(dispatch_id)),
            ]),
        )
        .0
    };
    let running = Arc::new(Mutex::new(true));
    let running_hook = Arc::clone(&running);
    testhook::update(|h| {
        h.cas_turn_result = Some(Arc::new(move |turn_id| {
            assert_eq!(turn_id, "turn-1");
            Some(TurnResult {
                thread_id: "thr-b".to_string(),
                status: (!*running_hook.lock().unwrap()).then(|| "completed".to_string()),
                error: None,
                messages: vec!["on it".to_string(), "done: see /tmp/out.md".to_string()],
            })
        }))
    });
    assert_eq!(ask("nd-0123456789ab")["state"], Value::from("running"));
    *running.lock().unwrap() = false;
    let ended = ask("nd-0123456789ab");
    assert_eq!(ended["state"], Value::from("ended"));
    assert_eq!(ended["status"], Value::from("completed"));
    assert_eq!(ended["text"], Value::from("done: see /tmp/out.md"));
    assert_eq!(ended["error"], Value::Null);
    // A dispatch this hived never made is unknown, with the reason.
    let unknown = ask("nd-ffffffffffff");
    assert_eq!(unknown["state"], Value::from("unknown"));
    assert!(unknown["reason"]
        .as_str()
        .unwrap()
        .contains("holds no turn for dispatch nd-ffffffffffff"));
    let (missing, _) = handle_request(
        &workspace.to_string_lossy(),
        "team-a",
        "dev:3",
        "@99",
        "2026-04-17T00:00:00Z",
        &json_obj(&[("action", Value::from("node-result"))]),
    );
    assert_eq!(missing["ok"], Value::Bool(false));
}

#[test]
fn test_node_result_payload_reads_each_engines_own_word() {
    let _guard = testhook::install(Hook {
        cas_turn_result: Some(Arc::new(|turn_id| match turn_id {
            "t-run" => Some(TurnResult {
                thread_id: "thr".to_string(),
                status: None,
                error: None,
                messages: vec!["partial".to_string()],
            }),
            "t-failed" => Some(TurnResult {
                thread_id: "thr".to_string(),
                status: Some("failed".to_string()),
                error: Some("{\"code\":1}".to_string()),
                messages: vec![],
            }),
            "t-gone" => None,
            _ => panic!("unexpected turn {turn_id}"),
        })),
        gl_prompt_result: Some(Arc::new(|key, rid| {
            assert_eq!(key, "m-honey.g");
            match rid.rid {
                1 => Some(PromptResult::Running),
                2 => Some(PromptResult::Ended {
                    stop_reason: "end_turn".to_string(),
                    text: "SAGE_FINAL".to_string(),
                    error: None,
                }),
                3 => Some(PromptResult::Ended {
                    stop_reason: "error".to_string(),
                    text: String::new(),
                    error: Some("closed".to_string()),
                }),
                _ => None,
            }
        })),
        ..Default::default()
    });
    let mut handles = HashMap::new();
    let mut hold = |id: &str, handle: TurnHandle| {
        handles.insert(id.to_string(), handle);
    };
    let codex = |turn_id: &str| TurnHandle::Codex {
        thread_id: "thr".to_string(),
        turn_id: turn_id.to_string(),
    };
    let grok = |rid: u64| TurnHandle::Grok {
        key: "m-honey.g".to_string(),
        prompt_id: PromptId { generation: 1, rid },
    };
    hold("c-run", codex("t-run"));
    hold("c-failed", codex("t-failed"));
    hold("c-gone", codex("t-gone"));
    hold("g-run", grok(1));
    hold("g-done", grok(2));
    hold("g-err", grok(3));
    hold("g-gone", grok(4));
    hold("untracked", TurnHandle::Untracked("no turn id".to_string()));

    let node_result_payload = |id: &str| turn_result_payload(id, handles.get(id).cloned());
    let state = |id: &str| node_result_payload(id)["state"].clone();
    assert_eq!(state("c-run"), Value::from("running"));
    let failed = node_result_payload("c-failed");
    assert_eq!(failed["state"], Value::from("ended"));
    assert_eq!(failed["status"], Value::from("failed"));
    assert_eq!(failed["text"], Value::from(""));
    assert_eq!(failed["error"], Value::from("{\"code\":1}"));
    let gone = node_result_payload("c-gone");
    assert_eq!(gone["state"], Value::from("unknown"));
    assert!(gone["reason"].as_str().unwrap().contains("t-gone"));

    assert_eq!(state("g-run"), Value::from("running"));
    let done = node_result_payload("g-done");
    assert_eq!(done["state"], Value::from("ended"));
    assert_eq!(done["status"], Value::from("end_turn"));
    assert_eq!(done["text"], Value::from("SAGE_FINAL"));
    let err = node_result_payload("g-err");
    assert_eq!(err["status"], Value::from("error"));
    assert_eq!(err["error"], Value::from("closed"));
    assert!(node_result_payload("g-gone")["reason"]
        .as_str()
        .unwrap()
        .contains("prompt 4"));
    let untracked = node_result_payload("untracked");
    assert_eq!(untracked["state"], Value::from("unknown"));
    assert!(untracked["reason"].as_str().unwrap().contains("no turn id"));
    assert_eq!(node_result_payload("nope")["state"], Value::from("unknown"));
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
fn test_disk_build_hash_rehashes_only_when_the_exe_fingerprint_moves() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("hive");
    fs::write(&exe, b"build one").unwrap();
    let mut state = ReexecState::default();

    let first = disk_build_hash_at(&exe, &mut state);
    assert_ne!(first, "unknown");
    assert!(state.disk.is_some());

    // Same inode, length and mtime: the cached digest answers without a
    // read, even though the bytes underneath differ.
    let stamp = fs::metadata(&exe).unwrap().modified().unwrap();
    fs::write(&exe, b"build two").unwrap();
    fs::File::options()
        .write(true)
        .open(&exe)
        .unwrap()
        .set_modified(stamp)
        .unwrap();
    assert_eq!(disk_build_hash_at(&exe, &mut state), first);

    // A newer mtime is a new fingerprint: the file is hashed again.
    let later = stamp + Duration::from_secs(2);
    fs::File::options()
        .write(true)
        .open(&exe)
        .unwrap()
        .set_modified(later)
        .unwrap();
    let second = disk_build_hash_at(&exe, &mut state);
    assert_ne!(second, first);
    assert_eq!(second, compute_build_hash_at(&exe));

    // An install that renames a new file into place is a new inode.
    let staged = dir.path().join("hive.new");
    fs::write(&staged, b"build three").unwrap();
    fs::rename(&staged, &exe).unwrap();
    let third = disk_build_hash_at(&exe, &mut state);
    assert_ne!(third, second);

    // A vanished file is unknown and forgets the cache.
    fs::remove_file(&exe).unwrap();
    assert_eq!(disk_build_hash_at(&exe, &mut state), "unknown");
    assert!(state.disk.is_none());
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
        ..Default::default()
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
            "server.close".to_string(),
            "cleanup /ws".to_string(),
            "monitor.stop".to_string(),
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
        let released = calls.iter().position(|c| c == "release Some(42)").unwrap();
        let rebound = calls.iter().position(|c| c == "open /ws").unwrap();
        let ready = calls.iter().position(|c| c == "monitor.start").unwrap();
        assert!(released > rebound && released > ready);
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
        stale_disk_build_hash: Some(Arc::new(|| None)),
        wait_tick: Some(Arc::new(move || {
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
    crate::registry::record_team("team-a", &workspace, "123", &[], "").unwrap();

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
    // The loop's supervisors run for real here: their record store and
    // their pane listing must be this test's, not the developer's.
    env.set("CODEX_HOME", tmp.path().join(".codex"));
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
        try_acquire_reexec_lock: Some(Arc::new(|_| Some(88))),
        release_reexec_lock_fd: Some(Arc::new(move |fd| {
            release_sink.lock().unwrap().push(format!("release {fd:?}"))
        })),
        cleanup_socket: Some(Arc::new(move |workspace| {
            cleanup_sink
                .lock()
                .unwrap()
                .push(format!("cleanup {workspace}"))
        })),
        make_busy_monitor: Some(Arc::new(|_session| None)),
        notify_debug_emit: Some(Arc::new(|_ws, _event, _fields| {})),
        list_panes_all: Some(Arc::new(Vec::new)),
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
            "release Some(88)".to_string(),
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
        admit_preflight(&mut conn);
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

fn saved_operation(workspace: &Path, id: &str) -> Value {
    serde_json::from_slice(
        &fs::read(
            hooked_run_dir(workspace.to_str().unwrap())
                .join("operations/123")
                .join(format!("{id}.json")),
        )
        .unwrap(),
    )
    .unwrap()
}

fn wire_send(hook: &mut Hook, workspace: &Path) {
    let team = Team {
        name: "team-x".to_string(),
        workspace: workspace.to_string_lossy().to_string(),
        created_at: 123.0,
        tmux_session: "dev".to_string(),
        tmux_window: "dev:0".to_string(),
        ..Default::default()
    };
    let loaded = team.clone();
    hook.team_load = Some(Arc::new(move |_| Ok(loaded.clone())));
    hook.resolve_live_agent = Some(Arc::new(move |_team, _agent| {
        Ok((team.clone(), fake_agent("b", "%9", "claude")))
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
    // A `hive workflow run` dispatch rides the normal transport (member
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
    hook.agent_dispatch_turn = Some(Arc::new(move |agent, text| {
        handed_sink
            .lock()
            .unwrap()
            .push((text.to_string(), agent.name.clone()));
        Ok(TurnHandle::Grok {
            key: "m-team-x.b".to_string(),
            prompt_id: PromptId {
                generation: 1,
                rid: 7,
            },
        })
    }));
    let _guard = testhook::install(hook);

    let payload = send_payload(
        &workspace.to_string_lossy(),
        "team-x",
        SendOrigin::Node {
            dispatch_id: "nd-0123456789ab",
        },
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
    assert_eq!(handed[0].1, "b");
    assert!(payload.get("untracked").is_none());
    let record = saved_operation(&workspace, "nd-0123456789ab");
    assert_eq!(record["handle"]["cli"], "grok");
    assert_eq!(record["handle"]["key"], "m-team-x.b");
    assert_eq!(record["handle"]["generation"], 1);
    assert_eq!(record["handle"]["promptId"], 7);

    // A refused turn is a refused dispatch: the row is on the ledger (the
    // dispatch was attempted), nothing is held for it.
    testhook::update(|h| {
        h.agent_dispatch_turn = Some(Arc::new(|_agent, _text| {
            Err(DeliveryError(
                "codex pane %1 did not accept the turn".to_string(),
            ))
        }))
    });
    let refused = send_payload(
        &workspace.to_string_lossy(),
        "team-x",
        SendOrigin::Node {
            dispatch_id: "nd-refused000000",
        },
        "b",
        "task nd-refused000000\nagain",
        "",
    )
    .unwrap();
    assert_eq!(refused["ok"], Value::Bool(false));
    assert!(refused["error"]
        .as_str()
        .unwrap()
        .contains("transport refused b: codex pane %1 did not accept the turn"));
    let record = saved_operation(&workspace, "nd-refused000000");
    assert_eq!(record["state"], "terminal");
    assert_eq!(record["result"]["status"], "refused");
    assert!(record.get("handle").is_none());

    // A turn the engine took without a trackable id is dispatched, flagged,
    // and held as untracked.
    testhook::update(|h| {
        h.agent_dispatch_turn = Some(Arc::new(|_agent, _text| {
            Ok(TurnHandle::Untracked("result without turn.id".to_string()))
        }))
    });
    let untracked = send_payload(
        &workspace.to_string_lossy(),
        "team-x",
        SendOrigin::Node {
            dispatch_id: "nd-untracked0000",
        },
        "b",
        "task nd-untracked0000\nagain",
        "",
    )
    .unwrap();
    assert_eq!(untracked["ok"], Value::Bool(true));
    assert_eq!(
        untracked["untracked"],
        Value::from("result without turn.id")
    );
    assert_eq!(
        saved_operation(&workspace, "nd-untracked0000")["handle"]["reason"],
        "result without turn.id"
    );
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
            "codex" => agent_hook.codex_dispatch = Some(Ok("turn-1".into())),
            "grok" => {
                agent_hook.grok_dispatch = Some(Ok(PromptId {
                    generation: 1,
                    rid: 7,
                }))
            }
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
    let snap = TickSnapshot::collect();
    assert_eq!(idle_notify_agent_panes("t", &snap), vec!["%1".to_string()]);
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
        entrypoint: String::new(),
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
                binding: None,
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
    let snap = TickSnapshot::collect();
    status_tick(
        &env.workspace.to_string_lossy(),
        members,
        None,
        &mut env.state,
        now,
        &snap,
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
fn test_supervisor_leaves_a_record_that_names_no_server_alone() {
    // %dead has no tmuxSocket: nobody's to reap, on a private server and
    // on the default one alike.
    let mut state = super_state();
    state.own_socket = Some("/x/tmux-501/e2e".to_string());
    state.recorded = vec!["%1".to_string(), "%dead".to_string()];
    assert_eq!(reap_calls(state), Vec::<String>::new());
    let mut state = super_state();
    state.recorded = vec!["%1".to_string(), "%dead".to_string()];
    assert_eq!(reap_calls(state), Vec::<String>::new());
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

#[test]
fn test_supervisor_reattach_prefers_recorded_cwd_and_escapes_both_values() {
    let mut state = super_state();
    state.cli_process.clear();
    state.cwds.insert("%1".into(), "/work/a'b $HOME".into());
    state.roster_cwd = "/fallback".into();
    state.threads.insert("%1".into(), "tid'quoted".into());
    let (_guard, calls) = super_env(state);
    codex_supervisor_tick("/tmp/ws", "t");
    assert!(calls.lock().unwrap().contains(
        &r#"send %1 cd '/work/a'\''b $HOME' && hive codex resume 'tid'\''quoted'"#.to_string()
    ));
}

#[test]
fn test_supervisor_reattach_uses_roster_cwd_for_empty_record() {
    let mut state = super_state();
    state.cli_process.clear();
    state.cwds.insert("%1".into(), String::new());
    state.roster_cwd = "/fallback dir".into();
    let (_guard, calls) = super_env(state);
    codex_supervisor_tick("/tmp/ws", "t");
    assert!(calls
        .lock()
        .unwrap()
        .contains(&"send %1 cd '/fallback dir' && hive codex resume 'tid-1'".to_string()));
}

// --------------------------------------------------------------------------
// display probe: the tick's tmux gate and its backoff
// --------------------------------------------------------------------------

#[test]
fn test_display_probe_backs_off_doubling_to_the_cap_and_resets_on_recovery() {
    let mut probe = DisplayProbe::new();
    assert!(probe.due(0.0));
    assert_eq!(
        probe.record("no-server", 0.0),
        Some(DisplayTransition::Unreachable)
    );
    assert!(!probe.due(0.5));
    assert!(probe.due(1.0));
    assert_eq!(probe.next_in(0.0), 1.0);
    assert_eq!(probe.record("no-server", 1.0), None);
    assert!(!probe.due(2.9));
    assert!(probe.due(3.0));
    // `unknown` backs off the same way: the display cannot be read either way.
    assert_eq!(probe.record("unknown", 3.0), None);
    assert!(probe.due(7.0));
    for now in [7.0, 15.0, 31.0, 61.0] {
        assert_eq!(probe.record("no-server", now), None);
    }
    assert_eq!(probe.next_in(61.0), DISPLAY_PROBE_MAX_BACKOFF_SECONDS);
    assert_eq!(probe.record("ok", 91.0), Some(DisplayTransition::Recovered));
    assert!(probe.due(91.0));
    assert_eq!(probe.next_in(91.0), 0.0);
    assert_eq!(probe.record("ok", 92.0), None);
    assert_eq!(
        probe.record("no-server", 93.0),
        Some(DisplayTransition::Unreachable)
    );
    assert_eq!(probe.next_in(93.0), IDLE_NOTIFY_TICK_SECONDS);
}

struct LoopProbeEnv {
    _env: EnvGuard,
    _tmp: tempfile::TempDir,
    workspace: String,
    probes: Arc<Mutex<usize>>,
    bindings: Arc<Mutex<usize>>,
    serves: Arc<Mutex<usize>>,
    events: EventSink,
    _guard: testhook::Guard,
}

/// A window of team `probe`'s instance (`createdAt` 123 on *workspace*)
/// as the tick's window listing reports it.
fn probe_window(window: &str, window_id: &str, session_id: &str, workspace: &str) -> WindowExtra {
    WindowExtra {
        window: window.to_string(),
        window_id: window_id.to_string(),
        session_id: session_id.to_string(),
        session_name: window.split(':').next().unwrap_or_default().to_string(),
        team: "probe".to_string(),
        workspace: workspace.to_string(),
        created: "123".to_string(),
        token: String::new(),
    }
}

/// A hived loop that serves four ticks then retires, against a display
/// whose probe answers *status* (`(None, status)` unless `ok`).
fn loop_probe_env(status: &'static str) -> LoopProbeEnv {
    let mut env = EnvGuard::cleared(&[HIVED_REEXEC_LOCK_ENV]);
    let tmp = tempfile::tempdir().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    // The sleep path asks the real Grok pool which keys it holds, and the
    // pool reads and writes session records under this root.
    env.set("GROK_HOME", tmp.path().join(".grok"));
    let workspace = tmp.path().to_string_lossy().to_string();
    // The instance the loop serves: its entry names the createdAt the
    // window tags must match, and the members a node may be dispatched to.
    crate::registry::record_team(
        "probe",
        &workspace,
        "123",
        &[
            crate::testkit::member_row("worker", "codex", "t-worker"),
            crate::testkit::member_row("b", "codex", "t-b"),
        ],
        "",
    )
    .unwrap();
    let window_ws = workspace.clone();
    let probes = Arc::new(Mutex::new(0usize));
    let bindings = Arc::new(Mutex::new(0usize));
    let serves = Arc::new(Mutex::new(0usize));
    let events: EventSink = Arc::new(Mutex::new(Vec::new()));
    let probes_sink = Arc::clone(&probes);
    let bindings_sink = Arc::clone(&bindings);
    let serves_sink = Arc::clone(&serves);
    let events_sink = Arc::clone(&events);
    let hook = Hook {
        open_server_socket: Some(Arc::new(|_workspace| {
            Ok(Box::new(RecServer {
                calls: Arc::new(Mutex::new(Vec::new())),
            }) as Box<dyn HivedServerApi>)
        })),
        write_hived_owner: Some(Arc::new(|workspace, pid, started_at, token| {
            write_hived_owner_impl(workspace, pid, started_at, token);
        })),
        release_reexec_lock_fd: Some(Arc::new(|_fd| {})),
        stale_disk_build_hash: Some(Arc::new(|| None)),
        wait_tick: Some(Arc::new(move || {
            let mut served = serves_sink.lock().unwrap();
            *served += 1;
            *served < 4
        })),
        cleanup_socket: Some(Arc::new(|_workspace| {})),
        make_busy_monitor: Some(Arc::new(|_session| None)),
        get_most_recent_client_window: Some(Arc::new(|_session| None)),
        team_load: Some(Arc::new(|_name| anyhow::bail!("no team"))),
        team_member_bindings: Some(Arc::new(move |_team| {
            *bindings_sink.lock().unwrap() += 1;
            Ok(Vec::new())
        })),
        list_panes_all: Some(Arc::new(Vec::new)),
        list_panes_all_status: Some(Arc::new(move || {
            *probes_sink.lock().unwrap() += 1;
            if status == "ok" {
                (Some(Vec::new()), "ok")
            } else {
                (None, status)
            }
        })),
        // The team window `probe:1` (`@1`, in session `$1`) carries this
        // instance's tags whenever the display answers.
        list_windows_snapshot: Some(Arc::new(move || {
            if status == "ok" {
                (
                    Some(vec![probe_window("probe:1", "@1", "$1", &window_ws)]),
                    "ok",
                )
            } else {
                (None, status)
            }
        })),
        gl_list_daemon_keys: Some(Arc::new(Vec::new)),
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
    LoopProbeEnv {
        _env: env,
        _tmp: tmp,
        workspace,
        probes,
        bindings,
        serves,
        events,
        _guard: testhook::install(hook),
    }
}

fn display_events(env: &LoopProbeEnv, name: &str) -> Vec<Map<String, Value>> {
    env.events
        .lock()
        .unwrap()
        .iter()
        .filter(|(event, _)| event == name)
        .map(|(_, fields)| fields.clone())
        .collect()
}

#[test]
fn test_hived_loop_skips_display_ticks_and_backs_off_while_tmux_is_unreachable() {
    let env = loop_probe_env("no-server");
    hived_loop(&env.workspace, "probe", "probe:1", "@1");
    assert_eq!(
        *env.serves.lock().unwrap(),
        4,
        "the coordinator advances every tick"
    );
    assert_eq!(
        *env.bindings.lock().unwrap(),
        0,
        "no display tick may run while tmux is unreachable"
    );
    assert_eq!(
        *env.probes.lock().unwrap(),
        1,
        "the probe backs off: one probe for four ticks, not one per tick"
    );
    let unreachable = display_events(&env, "display.unreachable");
    assert_eq!(unreachable.len(), 1, "the flip is logged once");
    assert_eq!(
        unreachable[0].get("status"),
        Some(&Value::from("no-server"))
    );
    let next = unreachable[0]
        .get("nextProbeSeconds")
        .and_then(Value::as_f64)
        .unwrap();
    assert!(
        (next - IDLE_NOTIFY_TICK_SECONDS).abs() < 1e-6,
        "next probe in {next}s"
    );
    assert!(display_events(&env, "display.recovered").is_empty());
}

#[test]
fn test_hived_loop_runs_display_ticks_every_tick_while_tmux_answers() {
    let env = loop_probe_env("ok");
    hived_loop(&env.workspace, "probe", "probe:1", "@1");
    assert_eq!(*env.serves.lock().unwrap(), 4);
    assert_eq!(
        *env.probes.lock().unwrap(),
        4,
        "a reachable display is probed every tick"
    );
    assert_eq!(
        *env.bindings.lock().unwrap(),
        4,
        "display ticks run every tick"
    );
    assert!(display_events(&env, "display.unreachable").is_empty());
    assert!(display_events(&env, "display.recovered").is_empty());
}

#[test]
fn test_accepted_connection_holds_drain_before_handler_starts() {
    use std::sync::Barrier;
    let accepted = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let accepted_hook = Arc::clone(&accepted);
    let release_hook = Arc::clone(&release);
    let handled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handled_hook = Arc::clone(&handled);
    let hook = Hook {
        after_accept: Some(Arc::new(move || {
            accepted_hook.wait();
            release_hook.wait();
        })),
        handle_request: Some(Arc::new(move |_| {
            handled_hook.fetch_add(1, Ordering::SeqCst);
            (json_obj(&[("ok", Value::Bool(true))]), false)
        })),
        execv: Some(Arc::new(|_| panic!("accepted request must block exec"))),
        ..Default::default()
    };
    let _guard = testhook::install(hook);
    let tmp = short_workspace();
    let workspace = tmp.path().to_str().unwrap().to_string();
    let server = Arc::new(open_server_socket(&workspace).unwrap());
    let client_ws = workspace.clone();
    let client = thread::spawn(move || request_hived(&client_ws, &action_payload("shutdown"), 5.0));
    let serve_ws = workspace.clone();
    let serve_server = Arc::clone(&server);
    let serving = thread::spawn(move || {
        serve_requests(serve_server.as_ref(), &serve_ws, "t", "", "", "start", 2.0)
    });
    accepted.wait();
    assert!(requests_in_flight());
    assert!(!drain_ready(&workspace));
    assert!(reexec_hived(&workspace, "t", "", "", server.as_ref(), None, None).is_none());
    assert_eq!(handled.load(Ordering::SeqCst), 0);
    close_admission();
    release.wait();
    assert_eq!(client.join().unwrap().unwrap()["ok"], true);
    serving.join().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while requests_in_flight() && std::time::Instant::now() < deadline {
        thread::yield_now();
    }
    assert!(drain_ready(&workspace));
    assert_eq!(handled.load(Ordering::SeqCst), 1);
    server.close();
}

#[test]
fn test_concurrent_ensure_processes_start_one_generation() {
    const CHILD: &str = "HIVE_ENSURE_CONCURRENCY_TEST";
    if let Ok(workspace) = std::env::var(CHILD) {
        let ready = Path::new(&workspace).join("ready");
        let started = ready.clone();
        let count = Path::new(&workspace).join("generations");
        let _guard = testhook::install(Hook {
            request_ping: Some(Arc::new(move |_, _| {
                ready.exists().then(|| {
                    json_obj(&[
                        ("ok", Value::Bool(true)),
                        ("apiVersion", Value::from(HIVED_API_VERSION)),
                        ("buildHash", Value::from(hived_build_hash())),
                        ("team", Value::from("t")),
                        (
                            "hiveHome",
                            Value::from(crate::paths::hive_home().to_string_lossy().to_string()),
                        ),
                    ])
                })
            })),
            popen: Some(Arc::new(move |_, _| {
                use std::io::Write;
                let mut file = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&count)
                    .unwrap();
                writeln!(file, "generation").unwrap();
                fs::write(&started, "ready").unwrap();
                4242
            })),
            ..Default::default()
        });
        ensure_hived(&workspace, "t", "", "").unwrap();
        return;
    }
    let tmp = short_workspace();
    let mut env = EnvGuard::new();
    env.set("HIVE_HOME", tmp.path().join("home"));
    let child = || {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "hived::tests::test_concurrent_ensure_processes_start_one_generation",
            ])
            .env(CHILD, tmp.path())
            .spawn()
            .unwrap()
    };
    let mut first = child();
    let mut second = child();
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());
    assert_eq!(
        fs::read_to_string(tmp.path().join("generations"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn test_shutdown_for_old_generation_does_not_retire_replacement() {
    let _guard = testhook::install(Hook::default());
    let mut request = action_payload("shutdown");
    request.insert(
        "expectedHived".into(),
        Value::Object(hived_metadata("old-generation")),
    );
    let (answer, keep_running) = handle_request("/unused", "t", "", "", "new-generation", &request);
    assert!(keep_running);
    assert_eq!(answer["generationChanged"], true);
    request.insert(
        "expectedHived".into(),
        Value::Object(hived_metadata("new-generation")),
    );
    let (answer, keep_running) = handle_request("/unused", "t", "", "", "new-generation", &request);
    assert!(!keep_running);
    assert_eq!(answer["ok"], true);
}

#[test]
fn test_shutdown_wins_over_reexec_after_last_lease_drops() {
    let _guard = testhook::install(Hook {
        try_acquire_reexec_lock: Some(Arc::new(|_| panic!("shutdown must not become an upgrade"))),
        ..Default::default()
    });
    SHUTDOWN.store(true, Ordering::SeqCst);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let server = RecServer {
        calls: Arc::clone(&calls),
    };
    assert!(reexec_hived("/unused", "t", "", "", &server, None, None).is_none());
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn test_draining_rejects_new_requests_before_any_handler_side_effect() {
    let _guard = testhook::install(Hook {
        handle_request: Some(Arc::new(|_| panic!("draining request was admitted"))),
        ..Default::default()
    });
    let tmp = short_workspace();
    let workspace = tmp.path().to_str().unwrap().to_string();
    let raw = open_server_socket(&workspace).unwrap();
    let server = RequestServer::start(Box::new(raw), &workspace, "t", "", "", "start").unwrap();
    close_admission();
    // The accept worker refuses on its own: an old-format request hears
    // notAdmitted, a preflight is refused before any payload is written.
    let reply = request_hived(&workspace, &action_payload("node-dispatch"), 2.0).unwrap();
    assert_eq!(reply["ok"], false);
    assert_eq!(reply["notAdmitted"], true);
    let err = request_admitted(&workspace, &action_payload("node-dispatch"), 2.0).unwrap_err();
    assert!(matches!(err, RequestFailure::NotAdmitted(_)), "{err:?}");
    assert!(!requests_in_flight());
    assert_eq!(admission().lock().unwrap().usage, 0);
    assert_eq!(admission().lock().unwrap().arrivals, 2);
    assert!(!hooked_run_dir(&workspace).join("operations").exists());
    server.close();
}

// --------------------------------------------------------------------------
// tick snapshot: every per-pane answer is a lookup
// --------------------------------------------------------------------------

fn snapshot_fixture() -> TickSnapshot {
    let listing = "%1\tclaude\tclaude\tagent\trex\tteam-a\tclaude\t\tdev:1\t0\t/dev/ttys003\t501\t/tmp/a\n\
                   %2\tshell\tzsh\tagent\tdodo\tteam-a\tcodex\t\tdev:1\t1\t/dev/ttys004\t502\t/tmp/b\n";
    let (panes, extras) = crate::tmux::parse_panes_snapshot(listing);
    let processes = crate::tmux::parse_all_tty_processes(
        "  501 ttys003 /Users/x/.local/bin/claude claude --resume abc\n  600 ttys004 -zsh -zsh\n  700 ??   /sbin/launchd /sbin/launchd\n",
    );
    let windows =
        crate::tmux::parse_windows_snapshot("dev:1\t@1\t$0\tdev\tteam-a\t/tmp\t1\ttok-1\n");
    TickSnapshot::with_extras("ok", panes, extras, Some(windows), Some(processes))
}

#[test]
fn test_tick_snapshot_answers_liveness_window_cli_and_token_without_probing() {
    let mut env = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    env.set("CLAUDE_CONFIG_DIR", tmp.path().join(".claude"));
    let _guard = testhook::install(Hook {
        is_pane_alive: Some(Arc::new(|_p| panic!("probed is_pane_alive"))),
        detect_cli_process_for_pane: Some(Arc::new(|_p| panic!("probed detect_cli"))),
        get_pane_window_target: Some(Arc::new(|_p| panic!("probed window target"))),
        get_window_option: Some(Arc::new(|_w, _k| panic!("probed window option"))),
        ..Default::default()
    });
    let snap = snapshot_fixture();
    assert!(snap.reachable());
    assert!(snap.is_alive("%1"));
    assert!(!snap.is_alive("%2"), "pane_dead=1");
    assert!(
        !snap.is_alive("%9"),
        "a pane the listing does not hold is gone"
    );
    assert_eq!(snap.window_of("%1").as_deref(), Some("dev:1"));
    assert_eq!(snap.window_of("%9"), None);
    assert_eq!(snap.cli_profile("%1").map(|p| p.name), Some("claude"));
    assert_eq!(
        snap.cli_profile("%2"),
        None,
        "a shell on the tty is not a CLI"
    );
    assert_eq!(snap.cli_profile("%9"), None);
    assert_eq!(snap.window_token("dev:1").as_deref(), Some("tok-1"));
    assert_eq!(snap.window_token("dev:2"), None);
}

#[test]
fn test_tick_snapshot_without_columns_answers_through_the_hooked_seams() {
    let _guard = testhook::install(Hook {
        list_panes_all: Some(Arc::new(|| {
            vec![crate::tmux::PaneInfo {
                pane_id: "%1".to_string(),
                ..Default::default()
            }]
        })),
        is_pane_alive: Some(Arc::new(|pane| pane == "%1")),
        detect_cli_process_for_pane: Some(Arc::new(|_p| claude_profile())),
        get_pane_window_target: Some(Arc::new(|_p| Some("dev:1".to_string()))),
        get_window_option: Some(Arc::new(|w, _k| (w == "dev:1").then(|| "tok".to_string()))),
        ..Default::default()
    });
    let snap = TickSnapshot::collect();
    assert!(snap.reachable());
    assert!(snap.is_alive("%1"));
    assert!(!snap.is_alive("%2"));
    assert_eq!(snap.window_of("%1").as_deref(), Some("dev:1"));
    assert_eq!(snap.cli_profile("%1").map(|p| p.name), Some("claude"));
    assert_eq!(snap.window_token("dev:1").as_deref(), Some("tok"));
    assert_eq!(snap.window_token("dev:2"), None);
}

#[test]
fn test_team_member_bindings_join_the_roster_to_tagged_panes_without_tmux() {
    let mut env = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    let dir = tmp.path().join(".hive/teams/team-a");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("team.json"),
        serde_json::json!({
            "team": "team-a",
            "workspace": dir.to_string_lossy(),
            "createdAt": "1",
            "members": [
                {"name": "orch", "cli": "claude"},
                {"name": "rex", "cli": "codex"},
                {"name": "dodo"}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let listing = "%2\tcodex\tcodex\tagent\trex\tteam-a\tcodex\t\tdev:1\t0\t/dev/ttys004\t502\t/tmp\n\
                   %3\tzsh\tzsh\tagent\tghost\tteam-a\tclaude\t\tdev:1\t0\t/dev/ttys005\t503\t/tmp\n\
                   %4\tgrok\tgrok\tagent\tdodo\tteam-b\tgrok\t\tdev:2\t0\t/dev/ttys006\t504\t/tmp\n\
                   %5\tview\thive\tmirror\tdodo\tteam-a\t\t\tdev:1\t0\t/dev/ttys007\t505\t/tmp\n";
    let (panes, extras) = crate::tmux::parse_panes_snapshot(listing);
    let snap = TickSnapshot::with_extras("ok", panes, extras, None, None);
    let _guard = testhook::install(Hook::default());
    let rows = team_member_bindings_impl("team-a", &snap).unwrap();
    let names: Vec<&str> = rows.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        ["dodo", "orch", "rex"],
        "roster names sorted; a pane tagged for a name the roster lacks is not a member"
    );
    let row = |name: &str| rows.iter().find(|(n, _)| n == name).unwrap().1.clone();
    assert_eq!(row("rex")["pane"], "%2");
    assert_eq!(row("rex")["role"], "agent");
    assert_eq!(row("rex")["cli"], "codex");
    assert_eq!(
        row("orch")["pane"],
        "",
        "a member without a pane still binds"
    );
    assert_eq!(row("orch")["role"], "agent");
    // dodo's team-b pane belongs to another team; the team-a mirror pane
    // binds with its role, and a roster row without a cli defaults to claude.
    assert_eq!(row("dodo")["pane"], "%5");
    assert_eq!(row("dodo")["role"], "mirror");
    assert_eq!(row("dodo")["cli"], "claude");
    assert!(team_member_bindings_impl("team-z", &snap).is_err());
}

#[test]
fn test_shutdown_refused_while_operations_pending_keeps_serving() {
    let env = loop_probe_env("ok");
    let path =
        prepare_operation(&env.workspace, "probe", "123", "nd-busy", "worker", "node").unwrap();
    operation_handle(&path, TurnHandle::Unknown("awaiting native result".into())).unwrap();
    let workspace = env.workspace.clone();
    let serves = Arc::clone(&env.serves);
    testhook::update(|h| {
        h.wait_tick = Some(Arc::new(move || {
            let mut count = serves.lock().unwrap();
            *count += 1;
            assert!(!admission().lock().unwrap().closed);
            let mut request = action_payload("shutdown");
            if *count == 2 {
                request.insert("force".into(), Value::Bool(true));
            }
            let (answer, keep_running) =
                handle_request(&workspace, "probe", "", "", "start", &request);
            if *count == 1 {
                assert_eq!(answer["draining"], true);
                assert_eq!(answer["pendingOperations"], 1);
                assert!(keep_running);
                assert_eq!(
                    handle_request(
                        &workspace,
                        "probe",
                        "",
                        "",
                        "start",
                        &action_payload("ping")
                    )
                    .0["ok"],
                    true
                );
            } else {
                assert_eq!(*count, 2);
                assert_eq!(answer["ok"], true);
                assert!(!keep_running);
                SHUTDOWN.store(true, Ordering::SeqCst);
            }
            keep_running
        }))
    });
    hived_loop(&env.workspace, "probe", "probe:1", "@1");
    assert_eq!(*env.serves.lock().unwrap(), 2);
    assert_eq!(*env.bindings.lock().unwrap(), 2);
    let record: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(record["result"]["status"], "interrupted");
    assert_eq!(record["result"]["reason"], "forced shutdown");
}

#[test]
fn test_ensure_hived_uses_old_generation_when_graceful_shutdown_is_deferred() {
    let tmp = short_workspace();
    let mut env = EnvGuard::new();
    env.set("HIVE_HOME", tmp.path().join("home"));
    let workspace = tmp.path().to_str().unwrap().to_string();
    let server = Arc::new(open_server_socket(&workspace).unwrap());
    let _guard = testhook::install(Hook {
        request_ping: Some(Arc::new(|_, _| {
            Some(json_obj(&[
                ("ok", Value::Bool(true)),
                ("team", Value::from("t")),
                (
                    "hiveHome",
                    Value::from(crate::paths::hive_home().to_string_lossy().to_string()),
                ),
                ("apiVersion", Value::from(HIVED_API_VERSION)),
                ("buildHash", Value::from("old")),
                ("hived", Value::Object(hived_metadata("start"))),
            ]))
        })),
        popen: Some(Arc::new(|_, _| panic!("old generation is still serving"))),
        cleanup_socket: Some(Arc::new(|_| panic!("must not unlink live socket"))),
        ..Default::default()
    });
    let path = prepare_operation(&workspace, "t", "123", "nd-busy", "worker", "node").unwrap();
    operation_handle(&path, TurnHandle::Unknown("pending".into())).unwrap();
    let serving_ws = workspace.clone();
    let serving_socket = Arc::clone(&server);
    let serving = thread::spawn(move || {
        serve_requests(
            serving_socket.as_ref(),
            &serving_ws,
            "t",
            "",
            "",
            "start",
            0.2,
        )
    });
    assert_eq!(ensure_hived(&workspace, "t", "", "").unwrap(), None);
    assert_eq!(
        request_hived(&workspace, &action_payload("ping"), 1.0).unwrap()["ok"],
        true
    );
    assert!(serving.join().unwrap());
    assert!(!admission().lock().unwrap().closed);
    server.close();
}

#[test]
fn test_shutdown_lease_timeout_resumes_graceful_service_but_bounds_force() {
    let _guard = testhook::install(Hook::default());
    let tmp = short_workspace();
    let lease = RequestLease::reserve(&mut admission().lock().unwrap());
    SHUTDOWN.store(true, Ordering::SeqCst);
    close_admission();
    assert!(!finish_shutdown(
        tmp.path().to_str().unwrap(),
        Duration::ZERO
    ));
    assert!(!SHUTDOWN.load(Ordering::SeqCst));
    assert!(!admission().lock().unwrap().closed);
    FORCE_SHUTDOWN.store(true, Ordering::SeqCst);
    assert!(finish_shutdown(
        tmp.path().to_str().unwrap(),
        Duration::ZERO
    ));
    drop(lease);
}

fn sleep_probe_env() -> LoopProbeEnv {
    let env = loop_probe_env("no-server");
    testhook::update(|h| {
        h.gl_idle_owned_keys = Some(Arc::new(|_| Some(Vec::new())));
        // The sleep commit takes the real startup lock on the probe
        // workspace; the lock it releases is real too.
        h.release_reexec_lock_fd = Some(Arc::new(release_reexec_lock_fd_impl));
    });
    env
}

/// A team window the display probe sees, on a session nobody watches
/// unless the test says so. The viewer count is asked by session id.
fn unwatched_probe_env(watching: Option<usize>) -> LoopProbeEnv {
    let env = loop_probe_env("ok");
    testhook::update(|h| {
        h.gl_idle_owned_keys = Some(Arc::new(|_| Some(Vec::new())));
        h.release_reexec_lock_fd = Some(Arc::new(release_reexec_lock_fd_impl));
        h.watching_clients = Some(Arc::new(move |session| {
            assert_eq!(session, "$1");
            watching
        }));
    });
    env
}

#[test]
fn test_hived_sleeps_when_no_terminal_watches_its_window() {
    let env = unwatched_probe_env(Some(0));
    let serves = Arc::clone(&env.serves);
    let clock = Arc::clone(&serves);
    testhook::update(|h| {
        h.monotonic = Some(Arc::new(move || {
            *clock.lock().unwrap() as f64 * HIVED_SLEEP_AFTER_SECONDS
        }));
        h.wait_tick = Some(Arc::new(move || {
            let mut n = serves.lock().unwrap();
            *n += 1;
            assert!(*n <= 2, "an unwatched hived failed to sleep");
            true
        }));
    });
    hived_loop(&env.workspace, "probe", "probe:1", "@1");
    let events = display_events(&env, "hived.sleep");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["reason"], "unwatched");
    assert!(SHUTDOWN.load(Ordering::SeqCst));
    assert!(crate::registry::load("probe").is_some());
    // The marker `hive wake` reads: this desk left for want of a viewer.
    let marker = fs::read_to_string(asleep_marker_path(&env.workspace)).unwrap();
    assert_eq!(asleep_reason(&marker).as_deref(), Some("unwatched"));
    assert_eq!(asleep_reason("not json"), None);
    assert_eq!(asleep_reason("{}"), None);
}

/// A tmux server restart hands out window ids from `@0` again: a hived
/// from before the restart must not read another team's `@0` as its own
/// display.
#[test]
fn test_hived_sleeps_when_its_window_id_now_belongs_to_another_team() {
    let env = sleep_probe_env();
    let serves = Arc::clone(&env.serves);
    let clock = Arc::clone(&serves);
    crate::registry::set_display("probe", "@1").unwrap();
    let workspace = env.workspace.clone();
    testhook::update(|h| {
        h.list_panes_all_status = Some(Arc::new(|| (Some(Vec::new()), "ok")));
        // The id exists on the server, but the window is another team's.
        h.list_windows_snapshot = Some(Arc::new(move || {
            let mut window = probe_window("other:0", "@1", "$0", &workspace);
            window.team = "other".to_string();
            (Some(vec![window]), "ok")
        }));
        h.monotonic = Some(Arc::new(move || {
            *clock.lock().unwrap() as f64 * HIVED_SLEEP_AFTER_SECONDS
        }));
        h.wait_tick = Some(Arc::new(move || {
            let mut n = serves.lock().unwrap();
            *n += 1;
            assert!(*n <= 2, "a hived on a recycled window id failed to sleep");
            true
        }));
    });
    hived_loop(&env.workspace, "probe", "probe:1", "@1");
    let events = display_events(&env, "hived.sleep");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["reason"], "window-gone");
}

#[test]
fn test_a_starting_hived_installs_the_wake_hooks_on_its_team_session() {
    let env = unwatched_probe_env(Some(1));
    let installed = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&installed);
    testhook::update(|h| {
        h.install_wake_hooks = Some(Arc::new(move |session| {
            sink.lock().unwrap().push(session.to_string());
            Ok(())
        }));
        h.wait_tick = Some(Arc::new(|| false));
    });
    hived_loop(&env.workspace, "probe", "probe:1", "@1");
    // Installed where the display was found, by session id.
    assert_eq!(*installed.lock().unwrap(), vec!["$1".to_string()]);
}

/// An unwatched desk retires only behind wake hooks that installed: while
/// tmux refuses the install the idle clock keeps running, the failure is
/// reported and the install retried, and the first success lets the
/// desk go.
#[test]
fn test_install_failure_blocks_unwatched_sleep_until_it_succeeds() {
    let env = unwatched_probe_env(Some(0));
    let serves = Arc::clone(&env.serves);
    let clock = Arc::clone(&serves);
    let installs = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::clone(&installs);
    testhook::update(|h| {
        h.install_wake_hooks = Some(Arc::new(move |session| {
            assert_eq!(session, "$1");
            if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                Err("set-hook refused".to_string())
            } else {
                Ok(())
            }
        }));
        h.monotonic = Some(Arc::new(move || {
            *clock.lock().unwrap() as f64 * HIVED_SLEEP_AFTER_SECONDS
        }));
        h.wait_tick = Some(Arc::new(move || {
            let mut n = serves.lock().unwrap();
            *n += 1;
            assert!(*n <= 3, "a desk whose hooks installed failed to sleep");
            true
        }));
    });
    hived_loop(&env.workspace, "probe", "probe:1", "@1");

    assert_eq!(installs.load(Ordering::SeqCst), 3);
    let failed = display_events(&env, "hived.wake_hooks_failed");
    assert_eq!(failed.len(), 2, "{failed:?}");
    assert_eq!(failed[0]["session"], "$1");
    assert_eq!(failed[0]["error"], "set-hook refused");
    assert_eq!(failed[0]["retryInSeconds"], WAKE_HOOK_RETRY_SECONDS);
    let slept = display_events(&env, "hived.sleep");
    assert_eq!(slept.len(), 1, "{slept:?}");
    assert_eq!(slept[0]["reason"], "unwatched");
    // The clock was never reset by the refusals: idle since the first tick.
    assert_eq!(slept[0]["idleSeconds"], 2.0 * HIVED_SLEEP_AFTER_SECONDS);
    let order: Vec<String> = env
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|(event, _)| event.clone())
        .filter(|event| event == "hived.sleep" || event == "hived.wake_hooks_failed")
        .collect();
    assert_eq!(
        order,
        vec![
            "hived.wake_hooks_failed",
            "hived.wake_hooks_failed",
            "hived.sleep"
        ]
    );
    let marker = fs::read_to_string(asleep_marker_path(&env.workspace)).unwrap();
    assert_eq!(asleep_reason(&marker).as_deref(), Some("unwatched"));
}

/// This home's wake hooks follow the display from session to session,
/// and leave a session behind only when no team of this home shows there
/// any more; the same location again installs nothing.
#[test]
fn test_wake_hooks_follow_display_and_remove_only_owned_entries() {
    let env = loop_probe_env("ok");
    let ws = env.workspace.clone();
    // Another team of this home, whose window sits in `$2` from tick 2 on.
    let other_ws = env
        ._tmp
        .path()
        .join("other-ws")
        .to_string_lossy()
        .into_owned();
    crate::registry::record_team("other", &other_ws, "50", &[], "").unwrap();
    let mut other = probe_window("main:4", "@5", "$2", &other_ws);
    other.team = "other".to_string();
    other.created = "50".to_string();
    // A same-named window of another instance in `$3`: not this home's.
    let mut stale = probe_window("work:2", "@6", "$3", &ws);
    stale.created = "99".to_string();
    let listings: Vec<Vec<WindowExtra>> = vec![
        vec![probe_window("probe:1", "@1", "$1", &ws)],
        vec![probe_window("main:3", "@1", "$2", &ws)],
        vec![probe_window("main:3", "@1", "$2", &ws), other.clone()],
        vec![
            probe_window("work:3", "@1", "$3", &ws),
            other.clone(),
            stale.clone(),
        ],
        vec![other.clone(), stale.clone()],
        vec![probe_window("probe:1", "@9", "$1", &ws), other.clone()],
        vec![probe_window("probe:1", "@9", "$1", &ws), other.clone()],
    ];
    let ticks = Arc::new(AtomicUsize::new(0));
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    testhook::update(|h| {
        let sink = Arc::clone(&log);
        h.install_wake_hooks = Some(Arc::new(move |session| {
            sink.lock().unwrap().push(format!("install {session}"));
            Ok(())
        }));
        let sink = Arc::clone(&log);
        h.remove_wake_hooks = Some(Arc::new(move |session| {
            sink.lock().unwrap().push(format!("remove {session}"));
        }));
        let tick = Arc::clone(&ticks);
        h.list_windows_snapshot = Some(Arc::new(move || {
            let listing = listings[tick.load(Ordering::SeqCst).min(listings.len() - 1)].clone();
            (Some(listing), "ok")
        }));
        let tick = Arc::clone(&ticks);
        h.wait_tick = Some(Arc::new(move || {
            tick.fetch_add(1, Ordering::SeqCst) + 1 < 7
        }));
    });
    hived_loop(&ws, "probe", "probe:1", "@1");

    assert_eq!(
        *log.lock().unwrap(),
        vec![
            "install $1", // born
            "remove $1",  // moved: nothing of this home's left in $1
            "install $2",
            // `other` arrives in $2: the same location, nothing to do
            "install $3", // moved again: $2 still shows `other`, kept
            "remove $3",  // windowless: the stale instance in $3 is not this home's
            "install $1", // rebuilt; the repeated snapshot installs nothing
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>()
    );
}

/// A second window of the instance arrives in another session while the
/// primary stays put: that session gets this home's wake hooks too — a
/// terminal arriving there is a viewer the sleep gate counts, so it must
/// be able to bring the desk back — and loses them again once the window
/// leaves it. The primary's monitor is not restarted for either.
#[test]
fn test_a_second_session_of_the_display_gets_the_wake_hooks_while_the_primary_stays() {
    let env = loop_probe_env("ok");
    let ws = env.workspace.clone();
    let primary = probe_window("probe:1", "@1", "$1", &ws);
    let second = probe_window("human:3", "@2", "$2", &ws);
    let listings: Vec<Vec<WindowExtra>> = vec![
        vec![primary.clone()],
        vec![primary.clone(), second.clone()],
        vec![primary.clone(), second.clone()],
        vec![primary.clone()],
        vec![primary.clone()],
    ];
    let ticks = Arc::new(AtomicUsize::new(0));
    let hooks: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let monitors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    testhook::update(|h| {
        h.make_busy_monitor = Some(tracked_monitors(&monitors));
        let sink = Arc::clone(&hooks);
        h.install_wake_hooks = Some(Arc::new(move |session| {
            sink.lock().unwrap().push(format!("install {session}"));
            Ok(())
        }));
        let sink = Arc::clone(&hooks);
        h.remove_wake_hooks = Some(Arc::new(move |session| {
            sink.lock().unwrap().push(format!("remove {session}"));
        }));
        let tick = Arc::clone(&ticks);
        h.list_windows_snapshot = Some(Arc::new(move || {
            let listing = listings[tick.load(Ordering::SeqCst).min(listings.len() - 1)].clone();
            (Some(listing), "ok")
        }));
        let tick = Arc::clone(&ticks);
        h.wait_tick = Some(Arc::new(move || {
            tick.fetch_add(1, Ordering::SeqCst) + 1 < 5
        }));
    });
    hived_loop(&ws, "probe", "probe:1", "@1");

    assert_eq!(
        *hooks.lock().unwrap(),
        vec!["install $1", "install $2", "remove $2"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>(),
        "the second session is armed on arrival and released when the window leaves it"
    );
    assert_eq!(
        *monitors.lock().unwrap(),
        vec!["make $1", "start $1", "stop $1"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>(),
        "the primary did not move: one monitor, stopped at teardown"
    );
    let sessions: Vec<Value> = display_events(&env, "hived.display")
        .into_iter()
        .map(|e| e["sessions"].clone())
        .collect();
    assert_eq!(
        sessions,
        vec![json!(["$1"]), json!(["$1", "$2"]), json!(["$1"])]
    );
}

/// The display sits in two sessions and nobody watches either: the desk
/// may retire unwatched only once *both* carry this home's wake hooks.
/// While the second session refuses the install the primary's hooks are
/// not enough — a terminal arriving at the second alone could not wake
/// the desk — so the idle clock keeps running, the failure is reported
/// per session and retried, and the first success there lets the desk go.
#[test]
fn test_unwatched_sleep_waits_for_the_second_sessions_wake_hooks() {
    let env = loop_probe_env("ok");
    let ws = env.workspace.clone();
    let serves = Arc::clone(&env.serves);
    let clock = Arc::clone(&serves);
    let installs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let windows = vec![
        probe_window("probe:1", "@1", "$1", &ws),
        probe_window("human:3", "@2", "$2", &ws),
    ];
    testhook::update(|h| {
        h.gl_idle_owned_keys = Some(Arc::new(|_| Some(Vec::new())));
        h.release_reexec_lock_fd = Some(Arc::new(release_reexec_lock_fd_impl));
        h.list_windows_snapshot = Some(Arc::new(move || (Some(windows.clone()), "ok")));
        h.watching_clients = Some(Arc::new(|session| {
            assert!(session == "$1" || session == "$2", "{session}");
            Some(0)
        }));
        let sink = Arc::clone(&installs);
        h.install_wake_hooks = Some(Arc::new(move |session| {
            let mut installs = sink.lock().unwrap();
            installs.push(session.to_string());
            let refusals = installs.iter().filter(|s| *s == "$2").count();
            if session == "$2" && refusals <= 2 {
                Err("set-hook refused".to_string())
            } else {
                Ok(())
            }
        }));
        h.monotonic = Some(Arc::new(move || {
            *clock.lock().unwrap() as f64 * HIVED_SLEEP_AFTER_SECONDS
        }));
        h.wait_tick = Some(Arc::new(move || {
            let mut n = serves.lock().unwrap();
            *n += 1;
            assert!(
                *n <= 3,
                "a desk whose every session is armed failed to sleep"
            );
            true
        }));
    });
    hived_loop(&ws, "probe", "probe:1", "@1");

    assert_eq!(
        *installs.lock().unwrap(),
        vec!["$1", "$2", "$2", "$2"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>(),
        "the primary armed at once; the second session was retried until it took"
    );
    let failed = display_events(&env, "hived.wake_hooks_failed");
    assert_eq!(failed.len(), 2, "{failed:?}");
    assert!(failed.iter().all(|e| e["session"] == "$2"), "{failed:?}");
    assert_eq!(failed[0]["error"], "set-hook refused");
    let slept = display_events(&env, "hived.sleep");
    assert_eq!(slept.len(), 1, "{slept:?}");
    assert_eq!(slept[0]["reason"], "unwatched");
    // Not a tick earlier: the primary's hooks alone did not let it go, and
    // the refusals never reset the clock — idle since the first tick.
    assert_eq!(slept[0]["idleSeconds"], 2.0 * HIVED_SLEEP_AFTER_SECONDS);
    let order: Vec<String> = env
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|(event, _)| event.clone())
        .filter(|event| event == "hived.sleep" || event == "hived.wake_hooks_failed")
        .collect();
    assert_eq!(
        order,
        vec![
            "hived.wake_hooks_failed",
            "hived.wake_hooks_failed",
            "hived.sleep"
        ]
    );
    let marker = fs::read_to_string(asleep_marker_path(&ws)).unwrap();
    assert_eq!(asleep_reason(&marker).as_deref(), Some("unwatched"));
}

#[test]
fn test_a_starting_hived_clears_the_asleep_marker() {
    let env = unwatched_probe_env(Some(1));
    let marker = asleep_marker_path(&env.workspace);
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    fs::write(&marker, "{\"reason\":\"unwatched\"}\n").unwrap();
    testhook::update(|h| {
        h.wait_tick = Some(Arc::new(|| false));
    });
    hived_loop(&env.workspace, "probe", "probe:1", "@1");
    assert!(!marker.exists());
}

#[test]
fn test_hived_stays_up_while_a_terminal_watches_or_the_count_is_unknown() {
    for watching in [Some(1), None] {
        let env = unwatched_probe_env(watching);
        let serves = Arc::clone(&env.serves);
        let clock = Arc::clone(&serves);
        testhook::update(|h| {
            h.monotonic = Some(Arc::new(move || {
                *clock.lock().unwrap() as f64 * HIVED_SLEEP_AFTER_SECONDS
            }));
            h.wait_tick = Some(Arc::new(move || {
                let mut n = serves.lock().unwrap();
                *n += 1;
                *n < 4
            }));
        });
        hived_loop(&env.workspace, "probe", "probe:1", "@1");
        assert_eq!(*env.serves.lock().unwrap(), 4, "{watching:?}");
        assert!(
            display_events(&env, "hived.sleep").is_empty(),
            "{watching:?}"
        );
    }
}

#[test]
fn test_hived_sleeps_without_display_or_obligations_and_preserves_registry() {
    let env = sleep_probe_env();
    let serves = Arc::clone(&env.serves);
    let clock = Arc::clone(&serves);
    let backfills = Arc::new(Mutex::new(0));
    let observed = Arc::clone(&backfills);
    let swept = Arc::new(Mutex::new(Vec::new()));
    let dropped = Arc::clone(&swept);
    testhook::update(|h| {
        h.monotonic = Some(Arc::new(move || {
            *clock.lock().unwrap() as f64 * HIVED_SLEEP_AFTER_SECONDS
        }));
        h.wait_tick = Some(Arc::new(move || {
            let mut n = serves.lock().unwrap();
            *n += 1;
            assert!(*n <= 2, "idle hived failed to sleep");
            true
        }));
        h.team_load = Some(Arc::new(move |_| {
            *observed.lock().unwrap() += 1;
            anyhow::bail!("no pane observation")
        }));
        h.gl_idle_owned_keys = Some(Arc::new(|team| {
            assert_eq!(team, "probe");
            Some(vec!["m-probe.worker".into()])
        }));
        h.gl_pool_drop_key = Some(Arc::new(move |key| {
            dropped.lock().unwrap().push(format!("drop {key}"))
        }));
    });
    hived_loop(&env.workspace, "probe", "probe:1", "@1");
    assert_eq!(*env.serves.lock().unwrap(), 2);
    let events = display_events(&env, "hived.sleep");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["reason"], "display-unreachable");
    assert_eq!(events[0]["idleSeconds"], HIVED_SLEEP_AFTER_SECONDS);
    assert!(SHUTDOWN.load(Ordering::SeqCst));
    assert!(!FORCE_SHUTDOWN.load(Ordering::SeqCst));
    assert!(!socket_path(&env.workspace).exists());
    assert!(crate::registry::load("probe").is_some());
    assert!(!hooked_run_dir(&env.workspace).join("operations").exists());
    assert_eq!(*backfills.lock().unwrap(), 3);
    assert_eq!(*swept.lock().unwrap(), ["drop m-probe.worker"]);
}

/// Sleep gives up this desk's own clients and nothing else. The leader on
/// a member's socket is grok's own process and the TUI in its pane is one
/// of that leader's clients: a reap here would take a human's session down
/// with the desk. The cost is a leader that outlives the hived, which the
/// next send finds already up.
#[test]
fn test_sleep_drops_owned_clients_without_parking_leaders() {
    use crate::adapters::grok_leader as gl;
    let env = sleep_probe_env();
    // The real pool answers which keys are idle and takes the drops.
    testhook::update(|h| h.gl_idle_owned_keys = None);
    let alpha = "m-probe.alpha";
    let beta = "m-probe.beta";
    let foreign = "m-other.worker";
    let mut table = Vec::new();
    for (key, pid) in [(alpha, 4100), (beta, 4200), (foreign, 4300)] {
        let (leader, tui) = gl::tests::socket_process_args(key);
        table.push((pid, leader));
        table.push((pid + 1, tui));
    }
    let signalled = gl::tests::watch_process_signals(table);
    let alpha_proc = gl::tests::pool_idle_fake_client(alpha);
    let beta_proc = gl::tests::pool_idle_fake_client(beta);
    let foreign_proc = gl::tests::pool_idle_fake_client(foreign);
    for (key, pid) in [(alpha, 4100), (beta, 4200), (foreign, 4300)] {
        let sock = gl::socket_path_for_key(key);
        fs::write(&sock, "").unwrap();
        fs::write(sock.with_extension("pid"), pid.to_string()).unwrap();
    }
    assert_eq!(
        gl::pool().idle_owned_keys("probe").map(|mut keys| {
            keys.sort();
            keys
        }),
        Some(vec![alpha.to_string(), beta.to_string()])
    );

    let mut state = SleepState::default();
    // A member mid-turn is not idle: no sleep, however long the desk sits.
    gl::tests::feed_turn_open(&alpha_proc, alpha, true);
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 0.0));
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 601.0));
    // A key whose alias rebound it elsewhere is evidence nobody can read.
    gl::tests::feed_turn_open(&alpha_proc, alpha, false);
    fs::write(gl::alias_path_for_key(beta), "l-rebound").unwrap();
    assert_eq!(gl::pool().idle_owned_keys("probe"), None);
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 1202.0));
    fs::remove_file(gl::alias_path_for_key(beta)).unwrap();
    assert!(!alpha_proc.terminated() && !beta_proc.terminated());

    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 1800.0));
    assert!(state.tick(&env.workspace, "probe", None, None, true, "", 2401.0));

    assert_eq!(display_events(&env, "hived.sleep").len(), 1);
    // The commit names the clients; they go only once the loop's teardown
    // has closed the listener, still under the startup lock it holds.
    let retirement = state.take_retirement().expect("the commit holds the lock");
    assert!(
        !alpha_proc.terminated() && !beta_proc.terminated(),
        "no client is dropped before the listener closes"
    );
    let lock_fd = retirement.lock_fd;
    retirement.drop_clients();
    release_reexec_lock_fd_impl(Some(lock_fd));
    assert!(
        alpha_proc.terminated() && beta_proc.terminated(),
        "both owned clients are closed"
    );
    assert!(
        !foreign_proc.terminated(),
        "another team's client is not this desk's to drop"
    );
    assert_eq!(
        gl::pool().idle_owned_keys("other"),
        Some(vec![foreign.to_string()])
    );
    assert!(
        signalled.lock().unwrap().is_empty(),
        "no leader or TUI was signalled: {:?}",
        signalled.lock().unwrap()
    );
    for key in [alpha, beta] {
        let sock = gl::socket_path_for_key(key);
        assert!(sock.exists(), "{key} keeps its leader socket");
        assert!(sock.with_extension("pid").exists());
        assert!(
            gl::read_session_key(key).is_some(),
            "{key} keeps its session"
        );
    }
}

#[test]
fn test_sleep_timer_resets_when_display_returns() {
    let env = sleep_probe_env();
    let mut state = SleepState::default();
    let snap = TickSnapshot::with_extras("ok", Vec::new(), HashMap::new(), None, None);
    let location = DisplayLocation {
        window: "probe:1".into(),
        window_id: "@2".into(),
        session_id: "$1".into(),
        sessions: vec!["$1".into()],
    };
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 0.0));
    assert_eq!(state.idle_since(), Some(0.0));
    // The display comes back: the clock is dropped.
    assert!(!state.tick(
        &env.workspace,
        "probe",
        Some(&snap),
        Some(&location),
        true,
        "",
        599.0
    ));
    assert_eq!(state.idle_since(), None);
    assert!(!state.tick(&env.workspace, "probe", Some(&snap), None, true, "", 601.0));
    assert!(!state.tick(&env.workspace, "probe", Some(&snap), None, true, "", 1200.0));
    assert!(state.tick(&env.workspace, "probe", Some(&snap), None, true, "", 1201.0));
    assert_eq!(
        display_events(&env, "hived.sleep")[0]["reason"],
        "window-gone"
    );
}

#[test]
fn test_sleep_obligations_reset_the_timer() {
    let env = sleep_probe_env();
    let mut state = SleepState::default();
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 0.0));
    let mut lease = RequestLease::reserve(&mut admission().lock().unwrap());
    lease.classify("send");
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 600.0));
    drop(lease);
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 1200.0));
    let path = prepare_operation(
        &env.workspace,
        "probe",
        "123",
        "nd-pending",
        "worker",
        "node",
    )
    .unwrap();
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 1800.0));
    operation_terminal(&path, Map::new()).unwrap();
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 2400.0));
    testhook::update(|h| h.gl_idle_owned_keys = Some(Arc::new(|_| None)));
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 3000.0));
    testhook::update(|h| h.gl_idle_owned_keys = Some(Arc::new(|_| Some(Vec::new()))));
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 3600.0));
    assert!(state.tick(&env.workspace, "probe", None, None, true, "", 4200.0));
}

#[test]
fn test_read_only_requests_do_not_renew_sleep_but_short_send_does() {
    let env = sleep_probe_env();
    let mut state = SleepState::default();
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 0.0));
    for action in [
        "ping",
        "doctor",
        "team-runtime",
        "runtime-snapshot",
        "node-result",
        "turn-open",
    ] {
        let mut lease = RequestLease::reserve(&mut admission().lock().unwrap());
        lease.classify(action);
        assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 599.0));
        drop(lease);
    }
    let mut usage = RequestLease::reserve(&mut admission().lock().unwrap());
    usage.classify("send");
    drop(usage);
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 600.0));
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 601.0));
    let mut reader = RequestLease::reserve(&mut admission().lock().unwrap());
    reader.classify("ping");
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 1201.0));
    drop(reader);
    assert!(state.tick(&env.workspace, "probe", None, None, true, "", 1201.0));
}

#[test]
fn test_sleep_drain_new_connection_cancels_retirement() {
    // A connection that arrives once the gate is shut is refused by the
    // accept worker with notAdmitted — nothing of it is served — and its
    // arrival cancels the retirement at the final commit, so the retry
    // finds this desk up. The idle clock is kept: the refused request was
    // never classified, and a real use shows up as usage on the next tick.
    let env = sleep_probe_env();
    let workspace = env.workspace.clone();
    let raw = open_server_socket(&workspace).unwrap();
    let server = RequestServer::start(Box::new(raw), &workspace, "probe", "", "", "start").unwrap();
    let refused = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&refused);
    let ws = workspace.clone();
    testhook::update(|h| {
        h.gl_idle_owned_keys = Some(Arc::new(move |_| {
            // Asked again under the shut gate, before the commit: that is
            // when a send arrives.
            if admission().lock().unwrap().closed && sink.lock().unwrap().is_empty() {
                let err = request_admitted(&ws, &action_payload("send"), 2.0).unwrap_err();
                sink.lock().unwrap().push(err);
            }
            Some(Vec::new())
        }));
    });
    let mut state = SleepState::default();
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 0.0));
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 600.0));
    assert!(!admission().lock().unwrap().closed);
    assert!(display_events(&env, "hived.sleep").is_empty());
    assert!(
        matches!(
            refused.lock().unwrap().as_slice(),
            [RequestFailure::NotAdmitted(_)]
        ),
        "{:?}",
        refused.lock().unwrap()
    );
    assert_eq!(admission().lock().unwrap().usage, 0);
    assert_eq!(
        state.idle_since(),
        Some(0.0),
        "a refused arrival keeps the clock"
    );
    assert!(!requests_in_flight());
    // The desk serves again at once.
    let ping = request_hived(&workspace, &action_payload("ping"), 2.0).unwrap();
    assert_eq!(ping["ok"], true);
    settle_leases();
    testhook::update(|h| h.gl_idle_owned_keys = Some(Arc::new(|_| Some(Vec::new()))));
    assert!(state.tick(&env.workspace, "probe", None, None, true, "", 601.0));
    assert_eq!(display_events(&env, "hived.sleep").len(), 1);
    let retirement = state.take_retirement().expect("the commit holds the lock");
    server.close();
    release_reexec_lock_fd_impl(Some(retirement.lock_fd));
}

#[test]
fn test_send_wakes_a_sleeping_hived_and_delivers_once() {
    let env = sleep_probe_env();
    bus::init_workspace(Path::new(&env.workspace)).unwrap();
    let serves = Arc::clone(&env.serves);
    let clock = Arc::clone(&serves);
    testhook::update(|h| {
        h.open_server_socket = Some(Arc::new(|workspace| {
            Ok(Box::new(open_server_socket(workspace)?) as Box<dyn HivedServerApi>)
        }));
        h.cleanup_socket = Some(Arc::new(cleanup_socket_impl));
        h.release_reexec_lock_fd = Some(Arc::new(release_reexec_lock_fd_impl));
        h.monotonic = Some(Arc::new(move || {
            *clock.lock().unwrap() as f64 * HIVED_SLEEP_AFTER_SECONDS
        }));
        h.wait_tick = Some(Arc::new(move || {
            let mut n = serves.lock().unwrap();
            *n += 1;
            assert!(*n <= 2);
            true
        }));
    });
    hived_loop(&env.workspace, "probe", "probe:1", "@1");
    assert!(!socket_path(&env.workspace).exists());
    assert_eq!(display_events(&env, "hived.sleep").len(), 1);
    let worker = Arc::new(Mutex::new(None));
    let spawned = Arc::clone(&worker);
    let started = Arc::new(Mutex::new(0));
    let starts = Arc::clone(&started);
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let sent = Arc::clone(&delivered);
    let workspace = env.workspace.clone();
    testhook::update(|h| {
        h.monotonic = None;
        wire_send(h, Path::new(&workspace));
        h.agent_send = Some(Arc::new(move |_, body, _| {
            sent.lock().unwrap().push(body.to_string());
            Ok("udsWriteAccepted".into())
        }));
        h.popen = Some(Arc::new(move |argv, _| {
            assert!(argv.iter().any(|arg| arg == "--hived"));
            *starts.lock().unwrap() += 1;
            let server = open_server_socket(&workspace).unwrap();
            let ws = workspace.clone();
            SHUTDOWN.store(false, Ordering::SeqCst);
            reopen_admission();
            *spawned.lock().unwrap() = Some(thread::spawn(move || {
                while serve_requests(
                    &server,
                    &ws,
                    "probe",
                    "probe:1",
                    "@1",
                    "new-generation",
                    0.1,
                ) {}
                server.close();
            }));
            4242
        }));
    });
    let team = Team {
        name: "probe".into(),
        workspace: env.workspace.clone(),
        tmux_window: "probe:1".into(),
        tmux_window_id: "@1".into(),
        ..Default::default()
    };
    let result = crate::send::request_send_payload(
        &env.workspace,
        &team,
        "a",
        "b",
        "wake-message",
        "",
        "send",
        false,
    )
    .unwrap();
    assert!(result["seq"].as_i64().unwrap() > 0);
    assert_eq!(*started.lock().unwrap(), 1);
    assert_eq!(delivered.lock().unwrap().len(), 1);
    assert!(delivered.lock().unwrap()[0].contains("wake-message"));
    let answer = request_hived(&env.workspace, &action_payload("shutdown"), 1.0).unwrap();
    assert_eq!(answer["ok"], true);
    worker.lock().unwrap().take().unwrap().join().unwrap();
    assert!(crate::registry::load("probe").is_some());
}

#[test]
fn test_sleep_drain_new_node_cancels_without_interrupting_it() {
    // A node that becomes pending after the gate shut but before the
    // commit — one an admitted handler journaled while the coordinator
    // was between its checks — cancels the retirement and is left as it
    // is: no interruption, no terminal result written for it.
    let env = sleep_probe_env();
    let workspace = env.workspace.clone();
    testhook::update(|h| {
        h.gl_idle_owned_keys = Some(Arc::new(move |_| {
            if admission().lock().unwrap().closed && pending_operations(&workspace) == 0 {
                prepare_operation(&workspace, "probe", "123", "nd-late", "worker", "node").unwrap();
            }
            Some(Vec::new())
        }));
    });
    let mut state = SleepState::default();
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 0.0));
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 600.0));
    assert!(!admission().lock().unwrap().closed);
    assert_eq!(pending_operations(&env.workspace), 1);
    let record = saved_operation(Path::new(&env.workspace), "nd-late");
    assert_eq!(record["state"], "prepared");
    assert!(record.get("result").is_none());
    assert!(display_events(&env, "hived.sleep").is_empty());
    assert!(state.take_retirement().is_none());
}

#[test]
fn test_hived_answers_ping_and_shutdown_while_display_sampling_is_blocked() {
    assert_hived_answers_while_sampling(false);
}

#[test]
fn test_hived_reexec_failure_restarts_accept_worker_before_sampling() {
    assert_hived_answers_while_sampling(true);
}

fn assert_hived_answers_while_sampling(reexec: bool) {
    let env = loop_probe_env("ok");
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    testhook::update(|h| {
        h.open_server_socket = Some(Arc::new(|workspace| {
            Ok(Box::new(open_server_socket(workspace)?) as Box<dyn HivedServerApi>)
        }));
        h.list_panes_all_status = Some(Arc::new(move || {
            entered_tx.send(()).unwrap();
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
            (None, "no-server")
        }));
        h.wait_tick = Some(Arc::new(|| !SHUTDOWN.load(Ordering::SeqCst)));
        if reexec {
            h.stale_disk_build_hash = Some(Arc::new(|| Some("new-build".into())));
            h.try_acquire_reexec_lock = Some(Arc::new(|_| Some(42)));
            h.execv = Some(Arc::new(|_| {
                assert!(admission().lock().unwrap().closed);
                assert!(!requests_in_flight());
                ExecOutcome::Failed(std::io::Error::from_raw_os_error(8))
            }));
        }
    });
    thread::scope(|scope| {
        let serving = scope.spawn(|| hived_loop(&env.workspace, "probe", "probe:1", "@1"));
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let ping = request_hived(&env.workspace, &action_payload("ping"), 0.5);
        let shutdown = request_hived(&env.workspace, &action_payload("shutdown"), 0.5);
        // Release even if either RPC failed, so the failure is reported without
        // leaving the coordinator blocked in a test barrier.
        SHUTDOWN.store(true, Ordering::SeqCst);
        release_tx.send(()).unwrap();
        serving.join().unwrap();
        assert_eq!(
            ping.expect("ping must answer before sampling is released")["ok"],
            true
        );
        assert_eq!(
            shutdown.expect("shutdown must answer before sampling is released")["ok"],
            true
        );
    });
    assert!(!requests_in_flight());
}

#[test]
fn test_idle_accept_worker_holds_no_lease_and_refuses_closed_arrival() {
    use std::io::{Read, Write};
    struct Observed {
        server: ServerSocket,
        entered: std::sync::mpsc::Sender<()>,
    }
    impl HivedServerApi for Observed {
        fn close(&self) {
            self.server.close();
        }
        fn wait_readable(&self, timeout: f64) -> bool {
            let _ = self.entered.send(());
            self.server.wait_readable(timeout)
        }
        fn accept_timeout(&self, timeout: f64) -> Option<UnixStream> {
            self.server.accept_timeout(timeout)
        }
    }
    let _guard = testhook::install(Hook {
        handle_request: Some(Arc::new(|_| panic!("closed arrival was admitted"))),
        ..Default::default()
    });
    let tmp = short_workspace();
    let workspace = tmp.path().to_str().unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let raw = Observed {
        server: open_server_socket(workspace).unwrap(),
        entered: entered_tx,
    };
    let server = RequestServer::start(Box::new(raw), workspace, "t", "", "", "start").unwrap();
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        close_admission(),
        "an idle poll must not count as a request"
    );
    let mut client = UnixStream::connect(socket_path(workspace)).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    client.write_all(b"{\"action\":\"send\"}\n").unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();
    let mut reply = String::new();
    client.read_to_string(&mut reply).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&reply).unwrap()["notAdmitted"],
        true
    );
    assert!(!requests_in_flight());
    assert_eq!(admission().lock().unwrap().arrivals, 1);
    server.close();
}

// --------------------------------------------------------------------------
// admission: the preflight, the lease it holds, what a shut gate refuses,
// and the generation change a retiring desk makes under its lock
// --------------------------------------------------------------------------

/// A team whose node dispatches land on a codex member whose turn ends at
/// once, so the journal reaches terminal on the next flush. Returns the
/// count of turns handed to the engine.
fn wire_node_dispatch(hook: &mut Hook, workspace: &str, team: &str) -> Arc<AtomicUsize> {
    let dispatches = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&dispatches);
    let t = Team {
        name: team.to_string(),
        workspace: workspace.to_string(),
        created_at: 123.0,
        tmux_session: "dev".to_string(),
        tmux_window: "dev:0".to_string(),
        ..Default::default()
    };
    let loaded = t.clone();
    hook.team_load = Some(Arc::new(move |_| Ok(loaded.clone())));
    hook.resolve_live_agent = Some(Arc::new(move |_team, _agent| {
        Ok((t.clone(), fake_agent("b", "%9", "codex")))
    }));
    hook.check_send_gate = Some(Arc::new(|_target| Ok(())));
    hook.agent_dispatch_turn = Some(Arc::new(move |_agent, _text| {
        counted.fetch_add(1, Ordering::SeqCst);
        Ok(TurnHandle::Codex {
            thread_id: "thr-1".to_string(),
            turn_id: "turn-9".to_string(),
        })
    }));
    hook.cas_turn_result = Some(Arc::new(|_turn| {
        Some(TurnResult {
            thread_id: "thr-1".to_string(),
            status: Some("completed".to_string()),
            error: None,
            messages: vec!["done".to_string()],
        })
    }));
    dispatches
}

/// A point inside the hived a test can hold: the hook reports it was
/// reached and waits to be released, so the test asserts on a state that
/// is not moving. A one-off point releases itself after its first pass.
struct Checkpoint {
    reached: Mutex<std::sync::mpsc::Receiver<()>>,
    release: Mutex<std::sync::mpsc::Sender<()>>,
    hook: testhook::F0<()>,
}

impl Checkpoint {
    fn new() -> Checkpoint {
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let hook: testhook::F0<()> = Arc::new(move || {
            let _ = reached_tx.send(());
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10));
        });
        Checkpoint {
            reached: Mutex::new(reached_rx),
            release: Mutex::new(release_tx),
            hook,
        }
    }

    fn hook(&self) -> testhook::F0<()> {
        Arc::clone(&self.hook)
    }

    fn wait_reached(&self) {
        self.reached
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .expect("checkpoint reached");
    }

    fn release(&self) {
        self.release.lock().unwrap().send(()).unwrap();
    }
}

/// The node journal records written for the team incarnation the tests
/// wire (`created_at` 123).
fn journal_records(workspace: &str) -> Vec<String> {
    let dir = hooked_run_dir(workspace).join("operations/123");
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".json"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn node_dispatch_payload(team: &str, dispatch_id: &str) -> Map<String, Value> {
    let mut payload = action_payload("node-dispatch");
    payload.insert("team".to_string(), Value::from(team));
    payload.insert("targetAgent".to_string(), Value::from("b"));
    payload.insert(
        "body".to_string(),
        Value::from(format!("task {dispatch_id}")),
    );
    payload.insert("artifact".to_string(), Value::from(""));
    payload.insert("dispatchId".to_string(), Value::from(dispatch_id));
    payload
}

/// A raw client's preflight: connect, send the admit line for `action`,
/// and hand back the connection with the answer line it got (None at EOF
/// or reset). No business byte is ever written by this helper.
fn raw_preflight(workspace: &str, action: &str) -> (UnixStream, Option<Value>) {
    use std::io::BufRead;
    let conn = UnixStream::connect(socket_path(workspace)).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut preflight = action_payload(ADMIT_ACTION);
    preflight.insert("forAction".to_string(), Value::from(action));
    (&conn)
        .write_all(format!("{}\n", Value::Object(preflight)).as_bytes())
        .unwrap();
    let mut line = String::new();
    let answer = match std::io::BufReader::new(&conn).read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => serde_json::from_str(&line).ok(),
    };
    (conn, answer)
}

fn request_server(workspace: &str, team: &str) -> Box<dyn HivedServerApi> {
    let raw = open_server_socket(workspace).unwrap();
    RequestServer::start(Box::new(raw), workspace, team, "", "", "start").unwrap()
}

#[test]
fn test_preflight_lease_spans_admission_body_and_reply() {
    let tmp = short_workspace();
    let workspace = tmp.path().to_str().unwrap().to_string();
    bus::init_workspace(tmp.path()).unwrap();
    let mut hook = Hook::default();
    let dispatches = wire_node_dispatch(&mut hook, &workspace, "team-x");
    let admitted = Checkpoint::new();
    let handling = Checkpoint::new();
    let replying = Checkpoint::new();
    hook.after_admit = Some(admitted.hook());
    hook.before_handler = Some(handling.hook());
    hook.before_reply = Some(replying.hook());
    hook.execv = Some(Arc::new(|_| panic!("a leased request must block reexec")));
    let _guard = testhook::install(hook);
    let server = request_server(&workspace, "team-x");
    let side_effects = || {
        (
            dispatches.load(Ordering::SeqCst),
            bus::read_all_events(tmp.path()).unwrap().len(),
            journal_records(&workspace).len(),
        )
    };
    let client = {
        let ws = workspace.clone();
        thread::spawn(move || {
            request_node_dispatch(&ws, "team-x", "b", "task one", "", "nd-aaaaaaaaaaaa")
        })
    };

    // The admission line is out, the body not yet read: one lease, and
    // nothing of the request has happened.
    admitted.wait_reached();
    assert_eq!(admission().lock().unwrap().leases, 1);
    assert_eq!(side_effects(), (0, 0, 0));
    assert!(
        !drain_ready(&workspace),
        "sleep cannot pass a leased preflight"
    );
    reopen_admission();
    assert!(
        reexec_hived(&workspace, "team-x", "", "", server.as_ref(), None, None).is_none(),
        "reexec cannot pass a leased preflight"
    );
    assert!(!admission().lock().unwrap().closed);
    admitted.release();

    // The body is in and classified, the handler about to run.
    handling.wait_reached();
    assert_eq!(admission().lock().unwrap().leases, 1);
    assert_eq!(side_effects(), (0, 0, 0));
    handling.release();

    // The handler ran: the lease still holds until the reply is out.
    replying.wait_reached();
    assert_eq!(admission().lock().unwrap().leases, 1);
    assert_eq!(side_effects(), (1, 1, 1));
    replying.release();
    let answer = client.join().unwrap().unwrap();
    assert!(answer["seq"].as_i64().unwrap() > 0);
    assert_eq!(answer["dispatchId"], "nd-aaaaaaaaaaaa");
    settle_leases();
    assert_eq!(admission().lock().unwrap().leases, 0);

    testhook::update(|h| {
        h.after_admit = None;
        h.before_handler = None;
        h.before_reply = None;
    });
    // A body whose action is not the one admitted is not served.
    let (conn, answer) = raw_preflight(&workspace, "send");
    assert_eq!(answer.unwrap()["admitted"], true);
    (&conn)
        .write_all(
            format!(
                "{}\n",
                Value::Object(node_dispatch_payload("team-x", "nd-bbbbbbbbbbbb"))
            )
            .as_bytes(),
        )
        .unwrap();
    conn.shutdown(std::net::Shutdown::Write).unwrap();
    let mut reply = String::new();
    (&conn).read_to_string(&mut reply).unwrap();
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["ok"], false);
    assert!(reply["error"]
        .as_str()
        .unwrap()
        .contains("admitted for 'send'"));
    settle_leases();
    assert_eq!(side_effects(), (1, 1, 1));

    // A malformed prelude is answered and never reaches a handler.
    let mut conn = UnixStream::connect(socket_path(&workspace)).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    conn.write_all(b"{\"action\":\"admit\"}\n").unwrap();
    let mut reply = String::new();
    conn.read_to_string(&mut reply).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&reply).unwrap()["ok"], false);
    settle_leases();
    assert_eq!(side_effects(), (1, 1, 1));

    // A client that hangs up after the handshake releases the lease with
    // nothing served.
    let (conn, answer) = raw_preflight(&workspace, "node-dispatch");
    assert_eq!(answer.unwrap()["admitted"], true);
    assert_eq!(admission().lock().unwrap().leases, 1);
    drop(conn);
    settle_leases();
    assert_eq!(admission().lock().unwrap().leases, 0);
    assert_eq!(side_effects(), (1, 1, 1));
    assert_eq!(admission().lock().unwrap().usage, 1, "one classified write");
    server.close();
}

#[test]
fn test_answer_lost_after_business_body_remains_unknown() {
    // The handler took the dispatch and the reply is held past the
    // client's budget: the client hears a lost answer, not a refusal, and
    // the journal keeps the dispatch as with the member.
    let tmp = short_workspace();
    let mut env = EnvGuard::new();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    let workspace = tmp.path().to_str().unwrap().to_string();
    bus::init_workspace(tmp.path()).unwrap();
    crate::registry::record_team("team-x", &workspace, "123", &[], "").unwrap();
    let mut hook = Hook::default();
    let dispatches = wire_node_dispatch(&mut hook, &workspace, "team-x");
    let replying = Checkpoint::new();
    hook.before_reply = Some(replying.hook());
    let _guard = testhook::install(hook);
    let server = request_server(&workspace, "team-x");

    let err = request_admitted(
        &workspace,
        &node_dispatch_payload("team-x", "nd-cccccccccccc"),
        0.3,
    )
    .unwrap_err();
    assert!(matches!(err, RequestFailure::AnswerLost(_)), "{err:?}");
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(journal_records(&workspace), ["nd-cccccccccccc.json"]);
    replying.wait_reached();
    replying.release();
    settle_leases();
    assert_eq!(admission().lock().unwrap().leases, 0);
    // The runner's read-back finds the dispatch: it ended, once.
    let result = durable_node_result(&workspace, "team-x", "nd-cccccccccccc");
    assert_eq!(result["state"], "ended");
    assert_eq!(result["text"], "done");
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    server.close();
}

/// A stand-in desk for one production client: the workspace socket,
/// bound here so the client's connect finds it, and one accepted
/// connection handed to `serve` with a recorder of every byte the client
/// put on the wire. No hived is involved: what `serve` answers is the
/// whole protocol the client sees. Returns what was received.
fn one_peer(
    workspace: &str,
    serve: impl FnOnce(&UnixStream, &mut Vec<u8>) + Send + 'static,
) -> thread::JoinHandle<Vec<u8>> {
    let path = socket_path(workspace);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    thread::spawn(move || {
        let (conn, _) = listener.accept().unwrap();
        let mut received = Vec::new();
        serve(&conn, &mut received);
        received
    })
}

/// The peer's read of one line: whatever arrives up to a newline goes
/// onto `received`.
fn peer_take_line(conn: &UnixStream, received: &mut Vec<u8>) {
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut byte = [0u8; 1];
    loop {
        match (&*conn).read(&mut byte) {
            Ok(1) => {
                received.push(byte[0]);
                if byte[0] == b'\n' {
                    return;
                }
            }
            _ => return,
        }
    }
}

/// The peer listens for `hold` without answering: whatever the client
/// sends meanwhile goes onto `received`.
fn peer_hold(conn: &UnixStream, received: &mut Vec<u8>, hold: Duration) {
    let until = std::time::Instant::now() + hold;
    let mut chunk = [0u8; 4096];
    loop {
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return;
        }
        conn.set_read_timeout(Some(left)).unwrap();
        match (&*conn).read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => received.extend_from_slice(&chunk[..n]),
        }
    }
}

/// The peer drips one byte of a line it never finishes every 100ms until
/// the client hangs up — the write fails — or `cap` bytes went out;
/// returns how many did. No read probe: a client that shut its write
/// side after the body looks like a hangup to one.
fn peer_drip(conn: &UnixStream, cap: usize) -> usize {
    let mut sent = 0;
    while sent < cap {
        if (&*conn).write_all(b" ").is_err() {
            break;
        }
        sent += 1;
        thread::sleep(Duration::from_millis(100));
    }
    sent
}

fn preflight_line(action: &str) -> Vec<u8> {
    let mut preflight = action_payload(ADMIT_ACTION);
    preflight.insert("forAction".to_string(), Value::from(action));
    format!("{}\n", Value::Object(preflight)).into_bytes()
}

#[test]
fn test_request_admitted_writes_no_business_byte_before_admission() {
    // The production client against a desk that takes its preflight and
    // does not answer: nothing but the preflight is on the wire while it
    // waits, whether the desk then hangs up, refuses, or admits — and
    // only after the admission does the body follow.
    let tmp = short_workspace();
    let workspace = tmp.path().to_str().unwrap().to_string();
    let frames: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&frames);
    let _guard = testhook::install(Hook {
        client_wrote: Some(Arc::new(move |frame| {
            sink.lock().unwrap().push(frame.to_string())
        })),
        ..Default::default()
    });
    let payload = node_dispatch_payload("team-x", "nd-eeeeeeeeeeee");
    let body_line = format!("{}\n", Value::Object(payload.clone())).into_bytes();
    let preflight = preflight_line("node-dispatch");
    let hold = Duration::from_millis(300);

    // The desk hangs up without a word.
    let peer = one_peer(&workspace, move |conn, received| {
        peer_take_line(conn, received);
        peer_hold(conn, received, hold);
    });
    let err = request_admitted(&workspace, &payload, 2.0).unwrap_err();
    assert_eq!(
        err,
        RequestFailure::NotAdmitted("hived closed the connection before admitting".to_string())
    );
    assert_eq!(
        peer.join().unwrap(),
        preflight,
        "the preflight and nothing else"
    );

    // The desk refuses.
    let peer = one_peer(&workspace, move |conn, received| {
        peer_take_line(conn, received);
        peer_hold(conn, received, hold);
        (&*conn)
            .write_all(b"{\"ok\":false,\"notAdmitted\":true,\"error\":\"hived is draining\"}\n")
            .unwrap();
        conn.shutdown(std::net::Shutdown::Write).unwrap();
        peer_hold(conn, received, Duration::from_secs(5));
    });
    let err = request_admitted(&workspace, &payload, 2.0).unwrap_err();
    assert_eq!(
        err,
        RequestFailure::NotAdmitted("hived is draining".to_string())
    );
    assert_eq!(
        peer.join().unwrap(),
        preflight,
        "refused: still only the preflight"
    );

    // The desk admits: the body follows, and only then.
    let peer = one_peer(&workspace, move |conn, received| {
        peer_take_line(conn, received);
        peer_hold(conn, received, hold);
        received.extend_from_slice(b"<admitted>");
        (&*conn)
            .write_all(
                format!("{{\"ok\":true,\"admitted\":true,\"apiVersion\":{HIVED_API_VERSION}}}\n")
                    .as_bytes(),
            )
            .unwrap();
        peer_take_line(conn, received);
        (&*conn)
            .write_all(b"{\"ok\":true,\"dispatchId\":\"nd-eeeeeeeeeeee\"}\n")
            .unwrap();
        conn.shutdown(std::net::Shutdown::Write).unwrap();
    });
    let answer = request_admitted(&workspace, &payload, 2.0).unwrap();
    assert_eq!(answer["dispatchId"], "nd-eeeeeeeeeeee");
    let mut expected = preflight.clone();
    expected.extend_from_slice(b"<admitted>");
    expected.extend_from_slice(&body_line);
    assert_eq!(
        peer.join().unwrap(),
        expected,
        "the body came after the admission"
    );

    // The client's own writes agree: one preflight per attempt, one body.
    let frames = frames.lock().unwrap();
    let preflight = String::from_utf8(preflight).unwrap();
    let body_line = String::from_utf8(body_line).unwrap();
    assert_eq!(
        *frames,
        vec![preflight.clone(), preflight.clone(), preflight, body_line]
    );
}

#[test]
fn test_request_admitted_gives_up_on_answers_that_never_end() {
    // A desk that drips an admission line, or an answer, a byte at a time
    // and never finishes it: each read fits the socket timeout, and the
    // caller is released at the budget's end — the identity budget for
    // the admission, the request's for the answer — not held forever.
    let tmp = short_workspace();
    let workspace = tmp.path().to_str().unwrap().to_string();
    let payload = node_dispatch_payload("team-x", "nd-ffffffffffff");
    let preflight = preflight_line("node-dispatch");

    let peer = one_peer(&workspace, |conn, received| {
        peer_take_line(conn, received);
        let dripped = peer_drip(conn, 90);
        received.extend_from_slice(format!("<dripped {dripped}>").as_bytes());
    });
    let began = std::time::Instant::now();
    let err = request_admitted(&workspace, &payload, 0.5).unwrap_err();
    let waited = began.elapsed();
    assert!(
        matches!(&err, RequestFailure::NotAdmitted(why) if why.starts_with("preflight answer lost")),
        "{err:?}"
    );
    assert!(
        waited >= Duration::from_secs_f64(IDENTITY_PING_TIMEOUT - 0.5)
            && waited < Duration::from_secs_f64(IDENTITY_PING_TIMEOUT + 2.0),
        "released at the identity budget, not per byte: {waited:?}"
    );
    let received = String::from_utf8(peer.join().unwrap()).unwrap();
    let dripped: usize = received
        .rsplit("<dripped ")
        .next()
        .unwrap()
        .trim_end_matches('>')
        .parse()
        .unwrap();
    assert!(
        dripped < 90,
        "the client hung up before the drip ran out: {dripped}"
    );
    assert!(
        received.as_bytes().starts_with(&preflight) && !received.contains("nd-ffffffffffff"),
        "no business byte went out: {received}"
    );

    // Admitted, body sent, then an answer that never ends.
    let peer = one_peer(&workspace, |conn, received| {
        peer_take_line(conn, received);
        (&*conn)
            .write_all(
                format!("{{\"ok\":true,\"admitted\":true,\"apiVersion\":{HIVED_API_VERSION}}}\n")
                    .as_bytes(),
            )
            .unwrap();
        peer_take_line(conn, received);
        peer_drip(conn, 40);
    });
    let began = std::time::Instant::now();
    let err = request_admitted(&workspace, &payload, 0.5).unwrap_err();
    let waited = began.elapsed();
    assert!(matches!(err, RequestFailure::AnswerLost(_)), "{err:?}");
    assert!(
        waited >= Duration::from_millis(400) && waited < Duration::from_secs(2),
        "released at the request budget: {waited:?}"
    );
    assert!(String::from_utf8(peer.join().unwrap())
        .unwrap()
        .contains("nd-ffffffffffff"));
}

#[test]
fn test_busy_identity_does_not_restart_matching_hived() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = EnvGuard::new();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    let run_tmp = tempfile::Builder::new()
        .prefix("hbsy")
        .tempdir_in("/tmp")
        .unwrap();
    let run_dir = run_tmp.path().to_path_buf();
    // Anything the CLI sends besides its hooked ping lands here: a
    // shutdown for the desk would be a connection in this backlog.
    let listener = UnixListener::bind(run_dir.join("hived.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    fn busy() -> Map<String, Value> {
        json_obj(&[
            ("ok", Value::Bool(false)),
            ("notAdmitted", Value::Bool(true)),
            (
                "error",
                Value::from("hived is draining; request not admitted; retry later"),
            ),
        ])
    }
    let pings = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&pings);
    let _guard = testhook::install(Hook {
        run_dir: Some(Arc::new(move |_ws| run_dir.clone())),
        request_ping: Some(Arc::new(move |_ws, _timeout| {
            if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                Some(busy())
            } else {
                Some(matching_identity())
            }
        })),
        popen: Some(Arc::new(|_, _| panic!("a busy desk was replaced"))),
        cleanup_socket: Some(Arc::new(|_| panic!("a busy desk's socket was unlinked"))),
        ..Default::default()
    });
    assert_eq!(
        ensure_hived("/tmp/ws-busy", "team-a", "dev:3", "@99").unwrap(),
        None
    );
    assert_eq!(pings.load(Ordering::SeqCst), 2, "asked again after busy");

    // Busy for the whole identity budget: an error that says so, and
    // still no restart.
    testhook::update(|h| {
        h.monotonic = Some(stepping_clock(10.0));
        h.request_ping = Some(Arc::new(move |_ws, _timeout| Some(busy())));
    });
    let err = ensure_hived("/tmp/ws-busy", "team-a", "dev:3", "@99")
        .unwrap_err()
        .to_string();
    assert!(err.contains("busy"), "{err}");
    assert!(err.contains("team-a"), "{err}");

    // A foreign home is refused first, busy or not.
    testhook::update(|h| {
        h.monotonic = None;
        h.request_ping = Some(Arc::new(|_ws, _timeout| {
            let mut answer = busy();
            answer.insert("hiveHome".to_string(), Value::from("/elsewhere/.hive"));
            Some(answer)
        }));
    });
    let err = ensure_hived("/tmp/ws-busy", "team-a", "dev:3", "@99")
        .unwrap_err()
        .to_string();
    assert!(err.contains("/elsewhere/.hive"), "{err}");

    assert!(
        matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "no shutdown or other request reached the socket"
    );
}

#[test]
fn test_identity_ping_spends_one_budget_across_busy_retries() {
    // One identity budget for the whole exchange: the first ping gets all
    // of it, each ping after a busy answer what is left. A desk that was
    // busy and then gave no answer by the budget's end is still busy — an
    // error, nothing replaced — while an empty answer that came back with
    // budget to spare is a desk gone, which the connect guard then checks.
    let tmp = tempfile::tempdir().unwrap();
    let mut env = EnvGuard::new();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    let run_tmp = tempfile::Builder::new()
        .prefix("hbdg")
        .tempdir_in("/tmp")
        .unwrap();
    let run_dir = run_tmp.path().to_path_buf();
    let listener = UnixListener::bind(run_dir.join("hived.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let busy = json_obj(&[
        ("ok", Value::Bool(false)),
        ("notAdmitted", Value::Bool(true)),
        ("error", Value::from("hived is draining")),
    ]);
    let clock = Arc::new(Mutex::new(0.0f64));
    let budgets: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    // Each answer with the time it took on the desk's clock.
    type Exchange = Vec<(f64, Option<Map<String, Value>>)>;
    let script: Arc<Mutex<Exchange>> = Arc::new(Mutex::new(Vec::new()));
    let reading = Arc::clone(&clock);
    let ticking = Arc::clone(&clock);
    let asked = Arc::clone(&budgets);
    let answers = Arc::clone(&script);
    let _guard = testhook::install(Hook {
        run_dir: Some(Arc::new(move |_ws| run_dir.clone())),
        monotonic: Some(Arc::new(move || *reading.lock().unwrap())),
        request_ping: Some(Arc::new(move |_ws, timeout| {
            asked.lock().unwrap().push(timeout);
            let mut answers = answers.lock().unwrap();
            assert!(!answers.is_empty(), "pinged past the scripted exchange");
            let (took, answer) = answers.remove(0);
            *ticking.lock().unwrap() += took;
            answer
        })),
        popen: Some(Arc::new(|_, _| panic!("a busy desk was replaced"))),
        cleanup_socket: Some(Arc::new(|_| panic!("a busy desk's socket was unlinked"))),
        ..Default::default()
    });
    let close_enough = |got: &[f64], want: &[f64]| {
        assert_eq!(got.len(), want.len(), "{got:?} vs {want:?}");
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() < 1e-6, "{got:?} vs {want:?}");
        }
    };

    // Busy at 0.5s, busy again at 4.9s, then the last 0.1s of budget run
    // out with no answer: still busy, not gone.
    *script.lock().unwrap() = vec![
        (0.5, Some(busy.clone())),
        (4.4, Some(busy.clone())),
        (0.2, None),
    ];
    let err = ensure_hived("/tmp/ws-budget", "team-a", "dev:3", "@99")
        .unwrap_err()
        .to_string();
    assert!(err.contains("busy"), "{err}");
    assert!(err.contains("team-a"), "{err}");
    assert!(
        script.lock().unwrap().is_empty(),
        "every scripted ping went out"
    );
    close_enough(&budgets.lock().unwrap(), &[5.0, 4.5, 0.1]);
    assert!(
        matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "no shutdown or other request reached the socket"
    );

    // Busy, then an empty answer with budget to spare: no desk answers,
    // and the connect guard — not the ping — decides against replacing
    // one whose socket still accepts.
    *clock.lock().unwrap() = 0.0;
    budgets.lock().unwrap().clear();
    *script.lock().unwrap() = vec![(0.5, Some(busy)), (0.1, None)];
    let err = ensure_hived("/tmp/ws-budget", "team-a", "dev:3", "@99")
        .unwrap_err()
        .to_string();
    assert!(err.contains("still accepts connections"), "{err}");
    close_enough(&budgets.lock().unwrap(), &[5.0, 4.5]);
}

#[test]
fn test_busy_identity_waits_out_a_sleep_rejection_on_a_real_socket() {
    // The desk is at its sleep commit, gate shut, when a CLI's ensure
    // pings it over the real socket: the ping is refused (busy), asked
    // again, and answered by the same generation once the commit is
    // cancelled — the same pid and owner token, nothing spawned.
    let env = sleep_probe_env();
    let workspace = env.workspace.clone();
    let (shut_tx, shut_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    let ticks = Arc::clone(&env.serves);
    let clock = Arc::clone(&ticks);
    // The clock runs a sleep's worth per tick to reach the commit; ensure's
    // identity budget reads the same clock, so it stops at the tick the
    // commit is held on — the desk stays awake after it on the pool's
    // unknown answer, not on the clock.
    let held_at = Arc::new(AtomicUsize::new(usize::MAX));
    let cap = Arc::clone(&held_at);
    let spawns = Arc::new(AtomicUsize::new(0));
    let spawned = Arc::clone(&spawns);
    testhook::update(|h| {
        h.open_server_socket = Some(Arc::new(|workspace| {
            Ok(Box::new(open_server_socket(workspace)?) as Box<dyn HivedServerApi>)
        }));
        h.cleanup_socket = Some(Arc::new(cleanup_socket_impl));
        h.monotonic = Some(Arc::new(move || {
            let ticks = *clock.lock().unwrap();
            ticks.min(cap.load(Ordering::SeqCst)) as f64 * HIVED_SLEEP_AFTER_SECONDS
        }));
        h.wait_tick = Some(Arc::new(move || {
            *ticks.lock().unwrap() += 1;
            !SHUTDOWN.load(Ordering::SeqCst)
        }));
        let held = AtomicUsize::new(0);
        let ticks_held = Arc::clone(&env.serves);
        h.gl_idle_owned_keys = Some(Arc::new(move |_| {
            // The first commit is held under the shut gate; after it the
            // pool answers unknown, which keeps the desk awake.
            match held.fetch_add(1, Ordering::SeqCst) {
                0 if !admission().lock().unwrap().closed => {
                    held.store(0, Ordering::SeqCst);
                    Some(Vec::new())
                }
                0 => {
                    held_at.store(*ticks_held.lock().unwrap(), Ordering::SeqCst);
                    let _ = shut_tx.send(());
                    let _ = go_rx.lock().unwrap().recv_timeout(Duration::from_secs(10));
                    Some(Vec::new())
                }
                _ => None,
            }
        }));
        h.popen = Some(Arc::new(move |_, _| {
            spawned.fetch_add(1, Ordering::SeqCst);
            4242
        }));
    });
    let owner_file = hooked_run_dir(&workspace).join("hived.owner.json");
    thread::scope(|scope| {
        let ws = workspace.clone();
        let serving = scope.spawn(move || hived_loop(&ws, "probe", "probe:1", "@1"));
        shut_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let owner_before = fs::read_to_string(&owner_file).unwrap();
        // The gate is shut: a raw ping now is refused, not answered.
        let refused = request_hived(&workspace, &action_payload("ping"), 2.0).unwrap();
        assert_eq!(refused["notAdmitted"], true);
        assert_eq!(hived_identity(Some(&refused), "probe"), HivedIdentity::Busy);
        let ensure = {
            let ws = workspace.clone();
            scope.spawn(move || ensure_hived(&ws, "probe", "", ""))
        };
        // ensure holds the startup lock and keeps asking; release the
        // commit, which then fails on the lock and cancels.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while admission().lock().unwrap().arrivals < 2 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            admission().lock().unwrap().arrivals >= 2,
            "ensure's ping was refused"
        );
        go_tx.send(()).unwrap();
        assert_eq!(ensure.join().unwrap().unwrap(), None);
        assert_eq!(fs::read_to_string(&owner_file).unwrap(), owner_before);
        assert_eq!(spawns.load(Ordering::SeqCst), 0);
        assert!(display_events(&env, "hived.sleep").is_empty());
        let bye = request_hived(&workspace, &action_payload("shutdown"), 2.0).unwrap();
        assert_eq!(bye["ok"], true);
        serving.join().unwrap();
    });
    assert!(!socket_path(&workspace).exists());
}

#[test]
fn test_unclassified_and_readonly_leases_preserve_sleep_deadline() {
    let env = sleep_probe_env();
    let mut state = SleepState::default();
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 0.0));
    assert_eq!(state.idle_since(), Some(0.0));
    // Accepted, action not yet read: the clock runs on, the exit waits.
    let mut lease = RequestLease::reserve(&mut admission().lock().unwrap());
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 599.0));
    assert_eq!(state.idle_since(), Some(0.0));
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 601.0));
    assert_eq!(
        state.idle_since(),
        Some(0.0),
        "an unread action does not renew"
    );
    assert_eq!(admission().lock().unwrap().usage, 0);
    // Classified as a read: still no renewal, still delays the exit.
    lease.classify("ping");
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 601.0));
    assert_eq!(state.idle_since(), Some(0.0));
    assert_eq!(admission().lock().unwrap().usage, 0);
    drop(lease);
    // A refused arrival during the commit cancels it and keeps the clock.
    let arrived = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&arrived);
    testhook::update(|h| {
        h.gl_idle_owned_keys = Some(Arc::new(move |_| {
            if admission().lock().unwrap().closed && count.fetch_add(1, Ordering::SeqCst) == 0 {
                admission().lock().unwrap().arrivals += 1;
            }
            Some(Vec::new())
        }));
    });
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 601.0));
    assert_eq!(arrived.load(Ordering::SeqCst), 1);
    assert_eq!(
        state.idle_since(),
        Some(0.0),
        "a refused arrival keeps the clock"
    );
    assert!(!admission().lock().unwrap().closed);
    // The next tick retires on the original deadline.
    assert!(state.tick(&env.workspace, "probe", None, None, true, "", 602.0));
    assert_eq!(display_events(&env, "hived.sleep")[0]["idleSeconds"], 602.0);
    let retirement = state.take_retirement().unwrap();
    release_reexec_lock_fd_impl(Some(retirement.lock_fd));
    SHUTDOWN.store(false, Ordering::SeqCst);
    reopen_admission();

    // A classified send is use: the clock resets and runs the full 600s.
    let mut state = SleepState::default();
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 1000.0));
    let mut send = RequestLease::reserve(&mut admission().lock().unwrap());
    send.classify("send");
    assert_eq!(admission().lock().unwrap().usage, 1);
    drop(send);
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 1601.0));
    assert_eq!(state.idle_since(), None, "use resets the clock");
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 1602.0));
    assert_eq!(state.idle_since(), Some(1602.0));
    assert!(!state.tick(&env.workspace, "probe", None, None, true, "", 2201.0));
    assert!(state.tick(&env.workspace, "probe", None, None, true, "", 2202.0));
    assert_eq!(display_events(&env, "hived.sleep")[1]["idleSeconds"], 600.0);
    let retirement = state.take_retirement().unwrap();
    release_reexec_lock_fd_impl(Some(retirement.lock_fd));
}

#[test]
fn test_closed_admission_rejects_multiple_connections_without_business_usage() {
    let tmp = short_workspace();
    let workspace = tmp.path().to_str().unwrap().to_string();
    let _guard = testhook::install(Hook {
        handle_request: Some(Arc::new(|_| panic!("a request passed the shut gate"))),
        ..Default::default()
    });
    let server = request_server(&workspace, "t");
    close_admission();
    let began = std::time::Instant::now();
    let clients: Vec<_> = (0..8)
        .map(|n| {
            let ws = workspace.clone();
            thread::spawn(move || {
                request_node_dispatch(&ws, "t", "b", "task", "", &format!("nd-{n:012}"))
            })
        })
        .collect();
    for client in clients {
        let err = client.join().unwrap().unwrap_err();
        assert!(
            matches!(
                err,
                RequestFailure::NotAdmitted(_) | RequestFailure::NotSent(_)
            ),
            "{err:?}"
        );
    }
    assert!(
        began.elapsed() < Duration::from_secs(3),
        "refusals are bounded: {:?}",
        began.elapsed()
    );
    {
        let state = admission().lock().unwrap();
        assert_eq!(state.usage, 0);
        assert_eq!(state.leases, 0);
        assert_eq!(state.arrivals, 8);
    }
    assert!(!hooked_run_dir(&workspace).join("operations").exists());

    // A peer that sends half a line and keeps the connection open: the
    // refusal comes at the read budget, holds no lease, and the next
    // request goes through once the gate reopens.
    let mut half = UnixStream::connect(socket_path(&workspace)).unwrap();
    half.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    half.write_all(b"{\"action\":\"adm").unwrap();
    let mut reply = String::new();
    half.read_to_string(&mut reply).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&reply).unwrap()["notAdmitted"],
        true
    );
    assert_eq!(admission().lock().unwrap().leases, 0);
    reopen_admission();
    testhook::update(|h| {
        h.handle_request = Some(Arc::new(|_| (json_obj(&[("ok", Value::Bool(true))]), true)));
    });
    let ping = request_hived(&workspace, &action_payload("ping"), 2.0).unwrap();
    assert_eq!(ping["ok"], true);
    settle_leases();
    drop(half);
    // Closing the worker does not wait for a peer still being drained.
    close_admission();
    let mut draining = UnixStream::connect(socket_path(&workspace)).unwrap();
    draining.write_all(b"{\"action\":\"adm").unwrap();
    let began = std::time::Instant::now();
    server.close();
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "{:?}",
        began.elapsed()
    );
    drop(draining);
}

/// A client that drips one byte of a line it never finishes every 100ms
/// — each byte within the desk's socket timeout — until the desk hangs
/// up or `cap` bytes went out. How many went out, and how long the
/// connection lived.
fn drip_until_dropped(conn: &UnixStream, cap: usize) -> (usize, Duration) {
    let began = std::time::Instant::now();
    let mut sent = 0;
    let mut probe = [0u8; 64];
    conn.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    while sent < cap {
        if (&*conn).write_all(b"{").is_err() {
            break;
        }
        sent += 1;
        if matches!((&*conn).read(&mut probe), Ok(0)) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    (sent, began.elapsed())
}

/// The desk under test hangs up on a dripping frame at the read budget
/// (one second on the accept worker), with nothing served and no lease
/// left; `cap` bytes at 100ms apiece would outlive that several times.
fn assert_dropped_at_budget(sent: usize, lived: Duration, cap: usize) {
    assert!(
        sent < cap,
        "the desk hung up before the drip ran out: {sent} of {cap} bytes"
    );
    assert!(
        lived >= Duration::from_millis(800) && lived < Duration::from_millis(2500),
        "dropped at the read budget, not per byte: {lived:?}"
    );
}

#[test]
fn test_open_gate_drops_a_dripping_prelude_at_the_read_budget() {
    // The gate is open and a client drips its first line a byte at a
    // time, each within the socket timeout, never the newline: the desk
    // ends the connection at the frame budget, calls no handler, keeps
    // no lease, and serves the next request.
    let tmp = short_workspace();
    let workspace = tmp.path().to_str().unwrap().to_string();
    let handled = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&handled);
    let _guard = testhook::install(Hook {
        handle_request: Some(Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            (json_obj(&[("ok", Value::Bool(true))]), true)
        })),
        ..Default::default()
    });
    let server = request_server(&workspace, "t");
    let conn = UnixStream::connect(socket_path(&workspace)).unwrap();
    let (sent, lived) = drip_until_dropped(&conn, 40);
    assert_dropped_at_budget(sent, lived, 40);
    settle_leases();
    assert_eq!(
        handled.load(Ordering::SeqCst),
        0,
        "no handler for a frame that never came"
    );
    {
        let state = admission().lock().unwrap();
        assert_eq!(state.leases, 0);
        assert_eq!(state.usage, 0);
    }
    drop(conn);
    let ping = request_hived(&workspace, &action_payload("ping"), 2.0).unwrap();
    assert_eq!(ping["ok"], true);
    settle_leases();
    assert_eq!(handled.load(Ordering::SeqCst), 1);
    assert!(drain_ready(&workspace), "nothing holds the desk");
    reopen_admission();
    server.close();
}

#[test]
fn test_open_gate_drops_a_dripping_body_at_the_read_budget() {
    // Admitted, then the body dripped a byte at a time and never
    // finished: the desk ends the connection at the body's budget, the
    // dispatch never reaches the engine, bus or journal, the lease goes,
    // and the next dispatch on a fresh connection lands.
    let tmp = short_workspace();
    let workspace = tmp.path().to_str().unwrap().to_string();
    bus::init_workspace(tmp.path()).unwrap();
    let mut hook = Hook::default();
    let dispatches = wire_node_dispatch(&mut hook, &workspace, "team-x");
    let _guard = testhook::install(hook);
    let server = request_server(&workspace, "team-x");
    let (conn, answer) = raw_preflight(&workspace, "node-dispatch");
    assert_eq!(answer.unwrap()["admitted"], true);
    assert_eq!(admission().lock().unwrap().leases, 1);
    let (sent, lived) = drip_until_dropped(&conn, 40);
    assert_dropped_at_budget(sent, lived, 40);
    settle_leases();
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    assert_eq!(bus::read_all_events(tmp.path()).unwrap().len(), 0);
    assert_eq!(journal_records(&workspace).len(), 0);
    {
        let state = admission().lock().unwrap();
        assert_eq!(state.leases, 0);
        assert_eq!(state.usage, 0, "an unfinished body is no classified write");
    }
    drop(conn);
    let answer =
        request_node_dispatch(&workspace, "team-x", "b", "task", "", "nd-dddddddddddd").unwrap();
    assert_eq!(answer["dispatchId"], "nd-dddddddddddd");
    settle_leases();
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(admission().lock().unwrap().leases, 0);
    assert!(drain_ready(&workspace), "nothing holds the desk");
    reopen_admission();
    server.close();
}

/// A listener whose `close` — reached by `RequestServer::close` only once
/// the accept worker is joined — holds at a checkpoint before the listener
/// itself goes: what arrives in between is queued behind nobody.
struct HeldClose {
    inner: ServerSocket,
    at_close: testhook::F0<()>,
    closed: Arc<AtomicUsize>,
}

impl HivedServerApi for HeldClose {
    fn close(&self) {
        (self.at_close)();
        self.inner.close();
        self.closed.fetch_add(1, Ordering::SeqCst);
    }
    fn wait_readable(&self, timeout: f64) -> bool {
        self.inner.wait_readable(timeout)
    }
    fn accept_timeout(&self, timeout: f64) -> Option<UnixStream> {
        self.inner.accept_timeout(timeout)
    }
}

/// A clock and tick counter for a loop under test: `monotonic` reads
/// `ticks * 600`, and `wait_tick` advances it after running `at_tick` with
/// the tick number about to start.
fn drive_ticks(h: &mut Hook, at_tick: impl Fn(usize) + Send + Sync + 'static) -> Arc<AtomicUsize> {
    let ticks = Arc::new(AtomicUsize::new(0));
    let clock = Arc::clone(&ticks);
    let counter = Arc::clone(&ticks);
    h.monotonic = Some(Arc::new(move || {
        clock.load(Ordering::SeqCst) as f64 * HIVED_SLEEP_AFTER_SECONDS
    }));
    h.wait_tick = Some(Arc::new(move || {
        let next = counter.load(Ordering::SeqCst) + 1;
        at_tick(next);
        counter.store(next, Ordering::SeqCst);
        !SHUTDOWN.load(Ordering::SeqCst)
    }));
    ticks
}

/// The fresh generation after a retirement, in this process: a serve loop
/// on a new listener, answering until told to shut down.
fn next_generation(workspace: &str, team: &str) -> thread::JoinHandle<()> {
    SHUTDOWN.store(false, Ordering::SeqCst);
    reopen_admission();
    let server = open_server_socket(workspace).unwrap();
    let ws = workspace.to_string();
    let team = team.to_string();
    thread::spawn(move || {
        while serve_requests(&server, &ws, &team, "", "", "next-generation", 0.1) {}
        server.close();
        cleanup_socket_impl(&ws);
    })
}

fn stop_generation(workspace: &str, generation: thread::JoinHandle<()>) {
    let bye = request_hived(workspace, &action_payload("shutdown"), 2.0).unwrap();
    assert_eq!(bye["ok"], true);
    generation.join().unwrap();
    settle_leases();
    SHUTDOWN.store(false, Ordering::SeqCst);
}

#[test]
fn test_sleep_backlog_before_close_is_retryable_without_dispatch() {
    // Eight dispatches around one retirement: two admitted before the
    // gate, two refused at the shut gate (the retirement cancels for
    // them), two queued between the worker's last accept and the
    // listener close, two arriving at the slow cleanup. None that was not
    // admitted has a payload byte on the wire; each retries under its own
    // nonce and every nonce lands exactly once.
    let env = sleep_probe_env();
    let workspace = env.workspace.clone();
    bus::init_workspace(Path::new(&workspace)).unwrap();
    let before_close = Checkpoint::new();
    let slow_cleanup = Checkpoint::new();
    let closed = Arc::new(AtomicUsize::new(0));
    let (phase_a_tx, phase_a_rx) = std::sync::mpsc::channel::<()>();
    let phase_a_rx = Mutex::new(phase_a_rx);
    let refused_b: Arc<Mutex<Vec<RequestFailure>>> = Arc::new(Mutex::new(Vec::new()));
    let mut dispatches = None;
    testhook::update(|h| {
        dispatches = Some(wire_node_dispatch(h, &workspace, "probe"));
        let at_close = before_close.hook();
        let closed = Arc::clone(&closed);
        h.open_server_socket = Some(Arc::new(move |ws| {
            Ok(Box::new(HeldClose {
                inner: open_server_socket(ws)?,
                at_close: Arc::clone(&at_close),
                closed: Arc::clone(&closed),
            }) as Box<dyn HivedServerApi>)
        }));
        h.cleanup_socket = Some(Arc::new(cleanup_socket_impl));
        let drop_hook = slow_cleanup.hook();
        h.gl_pool_drop_key = Some(Arc::new(move |_key| drop_hook()));
        let ws_b = workspace.clone();
        let sink = Arc::clone(&refused_b);
        h.gl_idle_owned_keys = Some(Arc::new(move |_| {
            if admission().lock().unwrap().closed && sink.lock().unwrap().is_empty() {
                // B: arrive at the shut gate, before the commit.
                let clients: Vec<_> = ["nd-b00000000001", "nd-b00000000002"]
                    .into_iter()
                    .map(|id| {
                        let ws = ws_b.clone();
                        thread::spawn(move || {
                            request_node_dispatch(&ws, "probe", "b", &format!("task {id}"), "", id)
                        })
                    })
                    .collect();
                let mut sink = sink.lock().unwrap();
                for client in clients {
                    sink.push(client.join().unwrap().unwrap_err());
                }
            }
            Some(vec!["m-probe.w".to_string()])
        }));
        let ws_tick = workspace.clone();
        drive_ticks(h, move |tick| match tick {
            // A: admitted and answered before the gate shuts.
            1 => phase_a_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap(),
            // B retries under the same nonces, on the same generation.
            4 => {
                for id in ["nd-b00000000001", "nd-b00000000002"] {
                    let answer = request_node_dispatch(
                        &ws_tick,
                        "probe",
                        "b",
                        &format!("task {id}"),
                        "",
                        id,
                    )
                    .unwrap();
                    assert_eq!(answer["dispatchId"], id);
                }
            }
            _ => {}
        });
    });
    let dispatches = dispatches.unwrap();
    let ws = workspace.clone();
    let serving = thread::spawn(move || hived_loop(&ws, "probe", "probe:1", "@1"));
    wait_for_desk(&workspace);
    for id in ["nd-a00000000001", "nd-a00000000002"] {
        let answer =
            request_node_dispatch(&workspace, "probe", "b", &format!("task {id}"), "", id).unwrap();
        assert!(answer["seq"].as_i64().unwrap() > 0);
    }
    settle_leases();
    phase_a_tx.send(()).unwrap();

    // C: the worker has stopped accepting, the listener is still bound.
    before_close.wait_reached();
    assert!(
        SHUTDOWN.load(Ordering::SeqCst),
        "the retirement is committed"
    );
    assert!(socket_path(&workspace).exists());
    let (queued_raw, _) = {
        let (conn, _) = (UnixStream::connect(socket_path(&workspace)).unwrap(), ());
        conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut preflight = action_payload(ADMIT_ACTION);
        preflight.insert("forAction".to_string(), Value::from("node-dispatch"));
        (&conn)
            .write_all(format!("{}\n", Value::Object(preflight)).as_bytes())
            .unwrap();
        (conn, ())
    };
    // The production client: the receipt of its preflight write says it
    // is in the backlog before the close goes on, and the recorder keeps
    // every frame it puts on the wire from here.
    let frames: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&frames);
    testhook::update(|h| {
        h.client_wrote = Some(Arc::new(move |frame| {
            sink.lock().unwrap().push(frame.to_string())
        }));
    });
    let queued_real = {
        let ws = workspace.clone();
        thread::spawn(move || {
            request_node_dispatch(
                &ws,
                "probe",
                "b",
                "task nd-c00000000002",
                "",
                "nd-c00000000002",
            )
        })
    };
    let receipt = std::time::Instant::now();
    while frames.lock().unwrap().is_empty() {
        assert!(
            receipt.elapsed() < Duration::from_secs(10),
            "the production client never wrote its preflight"
        );
        thread::sleep(Duration::from_millis(2));
    }
    before_close.release();

    // D: the listener is closed and the socket unlinked; the lock is held.
    slow_cleanup.wait_reached();
    assert_eq!(
        closed.load(Ordering::SeqCst),
        1,
        "the listener closed before the pool drop"
    );
    assert!(!socket_path(&workspace).exists());
    assert!(
        try_acquire_reexec_lock_impl(&workspace).is_none(),
        "the startup lock is held through the slow cleanup"
    );
    let refused_d: Vec<RequestFailure> = ["nd-d00000000001", "nd-d00000000002"]
        .into_iter()
        .map(|id| {
            request_node_dispatch(&workspace, "probe", "b", &format!("task {id}"), "", id)
                .unwrap_err()
        })
        .collect();
    slow_cleanup.release();
    serving.join().unwrap();
    assert_eq!(display_events(&env, "hived.sleep").len(), 1);

    // What the queued and late arrivals heard: nothing admitted, no
    // payload sent.
    let mut line = String::new();
    let queued_answer = {
        use std::io::BufRead;
        std::io::BufReader::new(&queued_raw).read_line(&mut line)
    };
    assert!(
        matches!(queued_answer, Ok(0) | Err(_)),
        "the queued preflight was reset unanswered: {line:?}"
    );
    let queued_real = queued_real.join().unwrap().unwrap_err();
    assert!(
        matches!(queued_real, RequestFailure::NotAdmitted(_)),
        "queued behind the close, the production client stopped at its preflight: {queued_real:?}"
    );
    let queued_frames = std::mem::take(&mut *frames.lock().unwrap());
    testhook::update(|h| h.client_wrote = None);
    assert_eq!(
        queued_frames,
        vec![String::from_utf8(preflight_line("node-dispatch")).unwrap()],
        "the only frame on the wire from the backlog on was that preflight; \
         the late arrivals found no socket to write to"
    );
    for err in refused_b.lock().unwrap().iter() {
        assert!(matches!(err, RequestFailure::NotAdmitted(_)), "{err:?}");
    }
    for err in &refused_d {
        assert_eq!(*err, RequestFailure::NoListener);
    }
    assert_eq!(
        dispatches.load(Ordering::SeqCst),
        4,
        "a, b and their retries only"
    );

    // The next generation takes the retries of C and D under their nonces.
    let generation = next_generation(&workspace, "probe");
    for id in [
        "nd-c00000000001",
        "nd-c00000000002",
        "nd-d00000000001",
        "nd-d00000000002",
    ] {
        let answer =
            request_node_dispatch(&workspace, "probe", "b", &format!("task {id}"), "", id).unwrap();
        assert_eq!(answer["dispatchId"], id);
    }
    stop_generation(&workspace, generation);
    assert_eq!(dispatches.load(Ordering::SeqCst), 8);
    let mut nonces: Vec<String> = bus::read_all_events(Path::new(&workspace))
        .unwrap()
        .iter()
        .map(|event| event.body.trim_start_matches("task ").to_string())
        .collect();
    nonces.sort();
    let mut expected: Vec<String> = [
        "nd-a00000000001",
        "nd-a00000000002",
        "nd-b00000000001",
        "nd-b00000000002",
        "nd-c00000000001",
        "nd-c00000000002",
        "nd-d00000000001",
        "nd-d00000000002",
    ]
    .iter()
    .map(|id| id.to_string())
    .collect();
    expected.sort();
    assert_eq!(nonces, expected, "one bus dispatch per nonce");
    assert_eq!(
        journal_records(&workspace),
        expected
            .iter()
            .map(|id| format!("{id}.json"))
            .collect::<Vec<_>>(),
        "one journal record per nonce"
    );
}

#[test]
fn test_retirement_closes_listener_before_slow_cleanup_under_owner_lock() {
    struct HeldMonitor {
        at_stop: testhook::F0<()>,
        stops: Arc<AtomicUsize>,
    }
    impl OutputMonitor for HeldMonitor {
        fn is_busy(&self, _pane_id: &str, _threshold_seconds: f64) -> bool {
            false
        }
        fn last_output_age(&self, _pane_id: &str) -> Option<f64> {
            None
        }
        fn stop(&self) {
            (self.at_stop)();
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
    }
    let env = unwatched_probe_env(Some(0));
    let workspace = env.workspace.clone();
    let monitor_stop = Checkpoint::new();
    let pool_drop = Checkpoint::new();
    let closed = Arc::new(AtomicUsize::new(0));
    let stops = Arc::new(AtomicUsize::new(0));
    let flock_refusals = Arc::new(AtomicUsize::new(0));
    let spawned_at: Arc<Mutex<Option<std::time::Instant>>> = Arc::new(Mutex::new(None));
    let generation: Arc<Mutex<Option<thread::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    testhook::update(|h| {
        let closed = Arc::clone(&closed);
        h.open_server_socket = Some(Arc::new(move |ws| {
            Ok(Box::new(HeldClose {
                inner: open_server_socket(ws)?,
                at_close: Arc::new(|| {}),
                closed: Arc::clone(&closed),
            }) as Box<dyn HivedServerApi>)
        }));
        h.cleanup_socket = Some(Arc::new(cleanup_socket_impl));
        let at_stop = monitor_stop.hook();
        let stops = Arc::clone(&stops);
        h.make_busy_monitor = Some(Arc::new(move |_session| {
            Some(Arc::new(HeldMonitor {
                at_stop: Arc::clone(&at_stop),
                stops: Arc::clone(&stops),
            }) as Arc<dyn OutputMonitor>)
        }));
        h.gl_idle_owned_keys = Some(Arc::new(|_| Some(vec!["m-probe.w".to_string()])));
        let drop_hook = pool_drop.hook();
        h.gl_pool_drop_key = Some(Arc::new(move |_key| drop_hook()));
        let refusals = Arc::clone(&flock_refusals);
        h.flock_nb = Some(Arc::new(move |fd| {
            let result = flock_nb_impl(fd);
            if result.is_err() {
                refusals.fetch_add(1, Ordering::SeqCst);
            }
            result
        }));
        let spawned = Arc::clone(&spawned_at);
        let generation = Arc::clone(&generation);
        let ws = workspace.clone();
        h.popen = Some(Arc::new(move |argv, _| {
            assert!(argv.iter().any(|arg| arg == "--hived"));
            *spawned.lock().unwrap() = Some(std::time::Instant::now());
            write_hived_owner_impl(&ws, getpid(), "next", "next-token");
            *generation.lock().unwrap() = Some(next_generation(&ws, "probe"));
            4242
        }));
        drive_ticks(h, |_| {});
    });
    let owner_file = hooked_run_dir(&workspace).join("hived.owner.json");
    let released_at = thread::scope(|scope| {
        let ws = workspace.clone();
        let serving = scope.spawn(move || hived_loop(&ws, "probe", "probe:1", "@1"));

        // At the monitor join: the worker is joined, the listener closed,
        // the socket gone, and the startup lock held.
        monitor_stop.wait_reached();
        assert_eq!(display_events(&env, "hived.sleep").len(), 1);
        assert_eq!(
            closed.load(Ordering::SeqCst),
            1,
            "listener closed before monitor.stop"
        );
        assert!(!socket_path(&workspace).exists());
        assert!(try_acquire_reexec_lock_impl(&workspace).is_none());
        let ensure = {
            let ws = workspace.clone();
            scope.spawn(move || ensure_hived(&ws, "probe", "", ""))
        };
        // ensure is refused the lock while the old generation cleans up.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while flock_refusals.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            flock_refusals.load(Ordering::SeqCst) >= 2,
            "ensure waited on the lock"
        );
        assert!(
            spawned_at.lock().unwrap().is_none(),
            "nothing spawned under the old lock"
        );
        monitor_stop.release();

        // At the pool drop: the lock is still held, still nothing spawned.
        pool_drop.wait_reached();
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert!(try_acquire_reexec_lock_impl(&workspace).is_none());
        assert!(spawned_at.lock().unwrap().is_none());
        let released_at = std::time::Instant::now();
        pool_drop.release();
        serving.join().unwrap();

        assert_eq!(ensure.join().unwrap().unwrap(), Some(4242));
        released_at
    });
    let spawned_at = spawned_at
        .lock()
        .unwrap()
        .expect("the next generation was spawned");
    assert!(
        spawned_at > released_at,
        "spawned only once the old lock was released"
    );
    // The old generation's cleanup left the new owner and socket alone.
    let owner: Value = serde_json::from_str(&fs::read_to_string(&owner_file).unwrap()).unwrap();
    assert_eq!(owner["token"], "next-token");
    assert!(socket_path(&workspace).exists());
    let ping = request_hived(&workspace, &action_payload("ping"), 2.0).unwrap();
    assert_eq!(ping["hived"]["started_at"], "next-generation");
    let generation = generation.lock().unwrap().take().unwrap();
    stop_generation(&workspace, generation);
    assert!(try_acquire_reexec_lock_impl(&workspace)
        .map(|fd| release_reexec_lock_fd_impl(Some(fd)))
        .is_some());
}

/// An old generation's stand-in on the workspace socket: answers a
/// shutdown as told and records every line it is sent.
struct OldGeneration {
    lines: Arc<Mutex<Vec<Value>>>,
    shutdown_answer: Arc<Mutex<Value>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    path: PathBuf,
}

impl OldGeneration {
    fn bind(workspace: &str, api: i64) -> OldGeneration {
        use std::io::BufRead;
        let path = socket_path(workspace);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        let lines: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let shutdown_answer = Arc::new(Mutex::new(serde_json::json!({"ok": true})));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (log, answer, stop_seen) = (
            Arc::clone(&lines),
            Arc::clone(&shutdown_answer),
            Arc::clone(&stop),
        );
        let thread = thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_seen.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { break };
                let mut reader = std::io::BufReader::new(&stream);
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let Ok(request) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                log.lock().unwrap().push(request.clone());
                let reply = match request["action"].as_str() {
                    Some("shutdown") => answer.lock().unwrap().clone(),
                    Some(ADMIT_ACTION) => {
                        let admitted =
                            serde_json::json!({"ok": true, "admitted": true, "apiVersion": api});
                        let _ = (&stream).write_all(format!("{admitted}\n").as_bytes());
                        line.clear();
                        let _ = reader.read_line(&mut line);
                        if let Ok(body) = serde_json::from_str::<Value>(&line) {
                            log.lock().unwrap().push(body);
                        }
                        serde_json::json!({"ok": true, "seq": 1})
                    }
                    _ => serde_json::json!({"ok": true}),
                };
                let _ = (&stream).write_all(format!("{reply}\n").as_bytes());
            }
        });
        OldGeneration {
            lines,
            shutdown_answer,
            stop,
            thread: Some(thread),
            path,
        }
    }

    fn actions(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .map(|line| line["action"].as_str().unwrap_or("").to_string())
            .collect()
    }
}

impl Drop for OldGeneration {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

#[test]
fn test_deferred_upgrade_requires_compatible_api() {
    let tmp = short_workspace();
    let mut env = EnvGuard::new();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    let workspace = tmp.path().to_str().unwrap().to_string();
    let home = crate::paths::hive_home().to_string_lossy().to_string();
    let identity = |api: i64, build: &str, team: &str, home: &str| {
        json_obj(&[
            ("ok", Value::Bool(true)),
            ("apiVersion", Value::from(api)),
            ("buildHash", Value::from(build)),
            ("team", Value::from(team)),
            ("hiveHome", Value::from(home)),
            ("hived", Value::Object(hived_metadata("old"))),
        ])
    };
    let team = Team {
        name: "t".to_string(),
        workspace: workspace.clone(),
        tmux_window: "dev:1".to_string(),
        tmux_window_id: "@7".to_string(),
        ..Default::default()
    };
    // The identities the ping hook hands out, in order; the last repeats.
    let pings: Arc<Mutex<std::collections::VecDeque<Map<String, Value>>>> =
        Arc::new(Mutex::new(Default::default()));
    let queue = Arc::clone(&pings);
    let _guard = testhook::install(Hook {
        request_ping: Some(Arc::new(move |_ws, _timeout| {
            let mut queue = queue.lock().unwrap();
            if queue.len() > 1 {
                queue.pop_front()
            } else {
                queue.front().cloned()
            }
        })),
        // Deferred and timed-out stops return at once on this clock; the
        // identity ping is never busy here, so its budget is untouched.
        monotonic: Some(stepping_clock(10.0)),
        popen: Some(Arc::new(|_, _| panic!("the old generation was replaced"))),
        cleanup_socket: Some(Arc::new(|_| {
            panic!("the old generation's socket was unlinked")
        })),
        ..Default::default()
    });
    let set_pings = |sequence: &[Map<String, Value>]| {
        *pings.lock().unwrap() = sequence.iter().cloned().collect();
    };
    let draining = serde_json::json!({"ok": false, "draining": true, "pendingOperations": 1});

    // Deferred, same api: the old build keeps serving and takes the
    // payload it can read.
    let old = OldGeneration::bind(&workspace, HIVED_API_VERSION);
    *old.shutdown_answer.lock().unwrap() = draining.clone();
    set_pings(&[identity(HIVED_API_VERSION, "old", "t", &home)]);
    assert_eq!(ensure_hived(&workspace, "t", "", "").unwrap(), None);
    assert_eq!(old.actions(), ["shutdown"]);
    let answer =
        crate::send::request_node_dispatch(&workspace, &team, "b", "task", "", "nd-000000000001")
            .unwrap();
    assert_eq!(answer["seq"], 1);
    assert_eq!(
        old.actions(),
        ["shutdown", "shutdown", ADMIT_ACTION, "node-dispatch"]
    );
    drop(old);

    // Deferred, another api: refused, and no payload goes out.
    let old = OldGeneration::bind(&workspace, HIVED_API_VERSION - 1);
    *old.shutdown_answer.lock().unwrap() = draining.clone();
    set_pings(&[identity(HIVED_API_VERSION - 1, "old", "t", &home)]);
    let err = ensure_hived(&workspace, "t", "", "")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(&format!("api {}", HIVED_API_VERSION - 1)),
        "{err}"
    );
    assert!(err.contains(&format!("api {HIVED_API_VERSION}")), "{err}");
    let err =
        crate::send::request_node_dispatch(&workspace, &team, "b", "task", "", "nd-000000000002")
            .unwrap_err();
    assert!(
        matches!(err, crate::send::DispatchFailure::Refused(_)),
        "{err:?}"
    );
    assert_eq!(
        old.actions(),
        ["shutdown", "shutdown"],
        "no admit, no node-dispatch reached the old generation"
    );
    drop(old);

    // Timed out: the stop was accepted but the generation is still there.
    let old = OldGeneration::bind(&workspace, HIVED_API_VERSION);
    set_pings(&[identity(HIVED_API_VERSION, "old", "t", &home)]);
    assert_eq!(ensure_hived(&workspace, "t", "", "").unwrap(), None);
    set_pings(&[identity(HIVED_API_VERSION - 1, "old", "t", &home)]);
    let err = ensure_hived(&workspace, "t", "", "")
        .unwrap_err()
        .to_string();
    assert!(err.contains("api"), "{err}");
    // Whoever answers after the stop must still be this team under this
    // home.
    set_pings(&[
        identity(HIVED_API_VERSION, "old", "t", &home),
        identity(HIVED_API_VERSION, "old", "t", "/elsewhere/.hive"),
    ]);
    let err = ensure_hived(&workspace, "t", "", "")
        .unwrap_err()
        .to_string();
    assert!(err.contains("/elsewhere/.hive"), "{err}");
    set_pings(&[
        identity(HIVED_API_VERSION, "old", "t", &home),
        identity(HIVED_API_VERSION, "old", "u", &home),
    ]);
    let err = ensure_hived(&workspace, "t", "", "")
        .unwrap_err()
        .to_string();
    assert!(err.contains("'u'"), "{err}");
    drop(old);
}

#[test]
fn test_new_hived_refuses_old_format_side_effects_but_answers_ping_and_shutdown() {
    let tmp = short_workspace();
    let workspace = tmp.path().to_str().unwrap().to_string();
    let handled = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&handled);
    let _guard = testhook::install(Hook {
        handle_request: Some(Arc::new(move |request| {
            let action = request.get("action").and_then(Value::as_str).unwrap_or("");
            assert!(
                !admission_required(action),
                "{action} reached the handler without a preflight"
            );
            counted.fetch_add(1, Ordering::SeqCst);
            (json_obj(&[("ok", Value::Bool(true))]), action != "shutdown")
        })),
        ..Default::default()
    });
    let server = request_server(&workspace, "t");
    for action in ["send", "node-dispatch", "connect-codex", "connect-grok"] {
        let reply = request_hived(&workspace, &action_payload(action), 2.0).unwrap();
        assert_eq!(reply["ok"], false, "{action}");
        assert!(
            reply["error"].as_str().unwrap().contains("preflight"),
            "{action}: {reply:?}"
        );
    }
    assert_eq!(handled.load(Ordering::SeqCst), 0);
    // The old-format control entries still complete: identification and
    // a graceful stop.
    let ping = request_hived(&workspace, &action_payload("ping"), 2.0).unwrap();
    assert_eq!(ping["ok"], true);
    let bye = request_hived(&workspace, &action_payload("shutdown"), 2.0).unwrap();
    assert_eq!(bye["ok"], true);
    assert!(SHUTDOWN.load(Ordering::SeqCst));
    assert_eq!(handled.load(Ordering::SeqCst), 2);
    settle_leases();
    server.close();
    SHUTDOWN.store(false, Ordering::SeqCst);
}

#[test]
fn test_generation_change_keeps_one_dispatch_and_its_terminal_result() {
    // The reexec branch: a dispatch admitted before the build changed is
    // delivered once, its result persisted before the exec, and read back
    // under the same dispatch id by the next generation.
    let env = sleep_probe_env();
    let workspace = env.workspace.clone();
    bus::init_workspace(Path::new(&workspace)).unwrap();
    let execs: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let mut dispatches = None;
    testhook::update(|h| {
        dispatches = Some(wire_node_dispatch(h, &workspace, "probe"));
        h.open_server_socket = Some(Arc::new(|ws| {
            Ok(Box::new(open_server_socket(ws)?) as Box<dyn HivedServerApi>)
        }));
        h.cleanup_socket = Some(Arc::new(cleanup_socket_impl));
        h.current_exe = Some(Arc::new(|| "/tmp/fake-hive".to_string()));
        let stale = AtomicUsize::new(0);
        h.stale_disk_build_hash = Some(Arc::new(move || {
            (stale.fetch_add(1, Ordering::SeqCst) >= 1).then(|| "new-build".to_string())
        }));
        let ws = workspace.clone();
        let sink = Arc::clone(&execs);
        h.execv = Some(Arc::new(move |argv| {
            assert!(admission().lock().unwrap().closed);
            assert!(!requests_in_flight());
            let record = saved_operation(Path::new(&ws), "nd-000000000001");
            assert_eq!(record["state"], "terminal", "persisted before the exec");
            sink.lock().unwrap().push(argv.to_vec());
            ExecOutcome::Replaced
        }));
        let ws = workspace.clone();
        let sink = Arc::clone(&execs);
        h.wait_tick = Some(Arc::new(move || {
            if sink.lock().unwrap().is_empty() && dispatches_so_far(&ws) == 0 {
                let answer =
                    request_node_dispatch(&ws, "probe", "b", "task", "", "nd-000000000001")
                        .unwrap();
                assert_eq!(answer["dispatchId"], "nd-000000000001");
                settle_leases();
            }
            sink.lock().unwrap().is_empty()
        }));
    });
    let dispatches = dispatches.unwrap();
    hived_loop(&workspace, "probe", "probe:1", "@1");
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    // The display never answered this generation: the next one is told
    // of no window, not of the birth target.
    assert_eq!(
        *execs.lock().unwrap(),
        vec![hived_reexec_argv(&workspace, "probe", "", "")]
    );
    let generation = next_generation(&workspace, "probe");
    let result = request_node_result(&workspace, "nd-000000000001").unwrap();
    assert_eq!(result["state"], "ended");
    assert_eq!(result["text"], "done");
    stop_generation(&workspace, generation);
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);

    // The restart branch: the CLI finds a stale build, stops it gracefully
    // and starts the next generation, which reads the same terminal
    // result; nothing is dispatched again.
    let old_socket = Arc::new(open_server_socket(&workspace).unwrap());
    let old = {
        let server = Arc::clone(&old_socket);
        let ws = workspace.clone();
        SHUTDOWN.store(false, Ordering::SeqCst);
        reopen_admission();
        thread::spawn(move || {
            while serve_requests(server.as_ref(), &ws, "probe", "", "", "old-generation", 0.1) {}
            server.close();
            cleanup_socket_impl(&ws);
        })
    };
    let answer =
        request_node_dispatch(&workspace, "probe", "b", "task", "", "nd-000000000002").unwrap();
    assert_eq!(answer["dispatchId"], "nd-000000000002");
    settle_leases();
    assert_eq!(dispatches.load(Ordering::SeqCst), 2);
    let spawns = Arc::new(AtomicUsize::new(0));
    let generation: Arc<Mutex<Option<thread::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    testhook::update(|h| {
        let stale = json_obj(&[
            ("ok", Value::Bool(true)),
            ("apiVersion", Value::from(HIVED_API_VERSION)),
            ("buildHash", Value::from("old")),
            ("team", Value::from("probe")),
            (
                "hiveHome",
                Value::from(crate::paths::hive_home().to_string_lossy().to_string()),
            ),
            ("hived", Value::Object(hived_metadata("old-generation"))),
        ]);
        let started = Arc::clone(&spawns);
        h.request_ping = Some(Arc::new(move |ws, timeout| {
            if started.load(Ordering::SeqCst) > 0 {
                request_ping_impl(ws, timeout)
            } else if socket_path(ws).exists() {
                Some(stale.clone())
            } else {
                None
            }
        }));
        let spawned = Arc::clone(&spawns);
        let generation = Arc::clone(&generation);
        let ws = workspace.clone();
        h.popen = Some(Arc::new(move |_, _| {
            spawned.fetch_add(1, Ordering::SeqCst);
            *generation.lock().unwrap() = Some(next_generation(&ws, "probe"));
            4242
        }));
        h.monotonic = None;
    });
    assert_eq!(
        ensure_hived(&workspace, "probe", "", "").unwrap(),
        Some(4242)
    );
    old.join().unwrap();
    assert_eq!(spawns.load(Ordering::SeqCst), 1);
    let ping = request_hived(&workspace, &action_payload("ping"), 2.0).unwrap();
    assert_eq!(ping["hived"]["started_at"], "next-generation");
    for id in ["nd-000000000001", "nd-000000000002"] {
        let result = request_node_result(&workspace, id).unwrap();
        assert_eq!(result["state"], "ended", "{id}");
        assert_eq!(result["text"], "done", "{id}");
    }
    assert_eq!(
        dispatches.load(Ordering::SeqCst),
        2,
        "nothing dispatched again"
    );
    let generation = generation.lock().unwrap().take().unwrap();
    stop_generation(&workspace, generation);
}

/// The desk under test answers on its socket.
fn wait_for_desk(workspace: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if request_hived(workspace, &action_payload("ping"), 1.0).is_some() {
            settle_leases();
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("the desk never answered");
}

fn dispatches_so_far(workspace: &str) -> usize {
    journal_records(workspace).len()
}

#[test]
fn test_admission_handshake_latency_within_budget() {
    // The desk's coordinator is stuck sampling the display; the accept
    // worker answers alone. One hundred pings and one hundred preflights,
    // each a round trip on the local socket: the handshake adds one such
    // trip to a request with a side effect, and neither budget is
    // approached.
    let env = loop_probe_env("ok");
    let workspace = env.workspace.clone();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    testhook::update(|h| {
        h.open_server_socket = Some(Arc::new(|workspace| {
            Ok(Box::new(open_server_socket(workspace)?) as Box<dyn HivedServerApi>)
        }));
        h.cleanup_socket = Some(Arc::new(cleanup_socket_impl));
        h.list_panes_all_status = Some(Arc::new(move || {
            entered_tx.send(()).unwrap();
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(60))
                .unwrap();
            (None, "no-server")
        }));
        h.wait_tick = Some(Arc::new(|| !SHUTDOWN.load(Ordering::SeqCst)));
    });
    let stats = |mut samples: Vec<f64>| {
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let at = |q: f64| samples[((samples.len() as f64 - 1.0) * q).round() as usize];
        (at(0.5), at(0.95), *samples.last().unwrap())
    };
    thread::scope(|scope| {
        let serving = scope.spawn(|| hived_loop(&workspace, "probe", "probe:1", "@1"));
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // Warm up: the first connection pays for thread creation.
        for _ in 0..5 {
            request_hived(&workspace, &action_payload("ping"), 2.0).unwrap();
            raw_preflight(&workspace, "send");
        }
        let mut pings = Vec::new();
        let mut preflights = Vec::new();
        for _ in 0..100 {
            let began = std::time::Instant::now();
            let ping = request_hived(&workspace, &action_payload("ping"), 2.0).unwrap();
            pings.push(began.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(ping["ok"], true);
            let began = std::time::Instant::now();
            let (_conn, answer) = raw_preflight(&workspace, "send");
            preflights.push(began.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(answer.unwrap()["admitted"], true);
        }
        let (ping_median, ping_p95, ping_max) = stats(pings);
        let (admit_median, admit_p95, admit_max) = stats(preflights);
        eprintln!(
            "ping ms median={ping_median:.3} p95={ping_p95:.3} max={ping_max:.3}; \
             preflight-to-admitted ms median={admit_median:.3} p95={admit_p95:.3} max={admit_max:.3}"
        );
        let shutdown = request_hived(&workspace, &action_payload("shutdown"), 0.5);
        SHUTDOWN.store(true, Ordering::SeqCst);
        release_tx.send(()).unwrap();
        serving.join().unwrap();
        assert_eq!(
            shutdown.expect("shutdown answers while sampling is blocked")["ok"],
            true
        );
        assert!(ping_p95 < 50.0, "ping p95 {ping_p95:.3}ms");
        assert!(admit_p95 < 50.0, "preflight p95 {admit_p95:.3}ms");
    });
    settle_leases();
}

// --------------------------------------------------------------------------
// the display: where the team's windows are now, tick by tick
// --------------------------------------------------------------------------

/// A busy monitor that records its life on *log*, as `make`/`start`/`stop`
/// with the session it was made for.
struct TrackedMonitor {
    session: String,
    log: Arc<Mutex<Vec<String>>>,
}

impl OutputMonitor for TrackedMonitor {
    fn is_busy(&self, _pane_id: &str, _threshold_seconds: f64) -> bool {
        false
    }
    fn last_output_age(&self, _pane_id: &str) -> Option<f64> {
        None
    }
    fn start(&self) {
        self.log
            .lock()
            .unwrap()
            .push(format!("start {}", self.session));
    }
    fn stop(&self) {
        self.log
            .lock()
            .unwrap()
            .push(format!("stop {}", self.session));
    }
}

fn tracked_monitors(log: &Arc<Mutex<Vec<String>>>) -> testhook::S1<Option<Arc<dyn OutputMonitor>>> {
    let log = Arc::clone(log);
    Arc::new(move |session| {
        log.lock().unwrap().push(format!("make {session}"));
        Some(Arc::new(TrackedMonitor {
            session: session.to_string(),
            log: Arc::clone(&log),
        }) as Arc<dyn OutputMonitor>)
    })
}

fn owner_bytes(workspace: &str) -> String {
    fs::read_to_string(hooked_run_dir(workspace).join("hived.owner.json")).unwrap_or_default()
}

#[test]
fn test_display_location_tracks_move_rename_and_rebuild() {
    let env = loop_probe_env("ok");
    let ws = env.workspace.clone();
    // The window listing the display answers on each tick: born in the
    // team session, moved whole into `main`, that session renamed, gone,
    // rebuilt in a new team session, then held while an upgrade fails.
    let listings: Vec<Vec<WindowExtra>> = vec![
        vec![probe_window("probe:1", "@1", "$1", &ws)],
        vec![probe_window("probe:1", "@1", "$1", &ws)],
        vec![probe_window("main:3", "@1", "$2", &ws)],
        vec![probe_window("work:3", "@1", "$2", &ws)],
        vec![],
        vec![probe_window("probe:1", "@9", "$3", &ws)],
        vec![probe_window("probe:1", "@9", "$3", &ws)],
    ];
    let ticks = Arc::new(AtomicUsize::new(0));
    let monitors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let hooks: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let viewers_asked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify_sessions: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let execs: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let owners: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    testhook::update(|h| {
        h.gl_idle_owned_keys = Some(Arc::new(|_| Some(Vec::new())));
        // The failed exec takes and releases the real reexec lock.
        h.release_reexec_lock_fd = Some(Arc::new(release_reexec_lock_fd_impl));
        h.make_busy_monitor = Some(tracked_monitors(&monitors));
        let sink = Arc::clone(&hooks);
        h.install_wake_hooks = Some(Arc::new(move |session| {
            sink.lock().unwrap().push(session.to_string());
            Ok(())
        }));
        let sink = Arc::clone(&viewers_asked);
        h.watching_clients = Some(Arc::new(move |session| {
            sink.lock().unwrap().push(session.to_string());
            Some(1)
        }));
        let sink = Arc::clone(&notify_sessions);
        h.get_most_recent_client_window = Some(Arc::new(move |session| {
            sink.lock().unwrap().push(session.to_string());
            None
        }));
        let tick = Arc::clone(&ticks);
        h.list_windows_snapshot = Some(Arc::new(move || {
            let listing = listings[tick.load(Ordering::SeqCst).min(listings.len() - 1)].clone();
            (Some(listing), "ok")
        }));
        let tick = Arc::clone(&ticks);
        h.stale_disk_build_hash = Some(Arc::new(move || {
            (tick.load(Ordering::SeqCst) == 6).then(|| "next-build".to_string())
        }));
        h.current_exe = Some(Arc::new(|| "/opt/hive".to_string()));
        let sink = Arc::clone(&execs);
        h.execv = Some(Arc::new(move |argv| {
            sink.lock().unwrap().push(argv.to_vec());
            ExecOutcome::Failed(std::io::Error::other("exec refused by the test"))
        }));
        let tick = Arc::clone(&ticks);
        let owners = Arc::clone(&owners);
        let owner_ws = ws.clone();
        h.wait_tick = Some(Arc::new(move || {
            let next = tick.fetch_add(1, Ordering::SeqCst) + 1;
            owners.lock().unwrap().push(owner_bytes(&owner_ws));
            next < 7
        }));
    });
    hived_loop(&ws, "probe", "probe:1", "@1");

    assert_eq!(
        *monitors.lock().unwrap(),
        vec![
            "make $1", "start $1", // born
            "stop $1", "make $2", "start $2", // moved: the same window, another session
            "stop $2",  // windowless
            "make $3", "start $3", // rebuilt
            "stop $3", "start $3", // the failed exec rebinds the same monitor
            "stop $3",  // teardown
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>(),
        "a rename keeps the session and restarts nothing; a repeated snapshot changes nothing"
    );
    assert_eq!(
        *hooks.lock().unwrap(),
        vec!["$1", "$2", "$3"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>(),
        "the wake hooks follow the display into each new session"
    );
    // The last tick ends at `wait_tick`, before the idle and sleep checks.
    assert_eq!(
        *viewers_asked.lock().unwrap(),
        vec!["$1", "$1", "$2", "$2", "$3"],
        "the sleep gate counts viewers on the display's current session, by id, and none without a display"
    );
    assert_eq!(
        *notify_sessions.lock().unwrap(),
        vec!["$1", "$1", "$2", "$2", "", "$3"],
        "the idle notifier reads the active window from the current session"
    );
    let moves: Vec<(Value, Value, Value)> = display_events(&env, "hived.display")
        .into_iter()
        .map(|e| {
            (
                e["window"].clone(),
                e["windowId"].clone(),
                e["session"].clone(),
            )
        })
        .collect();
    assert_eq!(
        moves,
        vec![
            (json!("probe:1"), json!("@1"), json!("$1")),
            (json!("main:3"), json!("@1"), json!("$2")),
            (json!("work:3"), json!("@1"), json!("$2")),
            (Value::Null, Value::Null, Value::Null),
            (json!("probe:1"), json!("@9"), json!("$3")),
        ]
    );
    assert_eq!(
        *execs.lock().unwrap(),
        vec![hived_reexec_argv(&ws, "probe", "probe:1", "@9")],
        "the next generation is told of the display as last seen, not the birth target"
    );
    let reexec = display_events(&env, "hived.reexec");
    assert_eq!(reexec.len(), 1);
    assert_eq!(reexec[0]["tmux_window"], "probe:1");
    assert_eq!(reexec[0]["tmux_window_id"], "@9");
    let owners = owners.lock().unwrap();
    assert_eq!(owners.len(), 7);
    assert!(
        owners[0].contains(&format!("{}", getpid())),
        "{}",
        owners[0]
    );
    assert!(
        owners.iter().all(|owner| *owner == owners[0]),
        "the hived's pid and token outlive every display change: {owners:?}"
    );
    assert!(display_events(&env, "hived.sleep").is_empty());
}

#[test]
fn test_display_location_prefers_the_registry_display_among_several_windows() {
    let env = sleep_probe_env();
    let ws = env.workspace.clone();
    let instance = TeamInstance::from_registry("probe", &ws);
    assert_eq!(instance.created, "123");
    let mut other = probe_window("dev:2", "@4", "$0", &ws);
    other.team = "other".to_string();
    let mut stale = probe_window("dev:3", "@5", "$0", &ws);
    stale.created = "99".to_string();
    let mut elsewhere = probe_window("dev:4", "@6", "$0", &ws);
    elsewhere.workspace = "/ws/elsewhere".to_string();
    let listing = |windows: Vec<WindowExtra>| {
        TickSnapshot::with_extras(
            "ok",
            Vec::new(),
            HashMap::new(),
            Some(windows.into_iter().map(|w| (w.window.clone(), w)).collect()),
            None,
        )
    };
    let asked = std::cell::Cell::new(0);
    let preferred = |id: &'static str| {
        let asked = &asked;
        move || {
            asked.set(asked.get() + 1);
            Some(id.to_string())
        }
    };

    // Same-name windows of another instance, and other teams' windows,
    // are not this display; nothing to prefer among one candidate.
    let snap = listing(vec![
        other.clone(),
        stale.clone(),
        elsewhere.clone(),
        probe_window("main:3", "@8", "$2", &ws),
    ]);
    let location = snap.display_location(&instance, preferred("@5")).unwrap();
    assert_eq!(asked.get(), 0, "one window needs no tie-break");
    assert_eq!(location.window, "main:3");
    assert_eq!(location.window_id, "@8");
    assert_eq!(location.session_id, "$2");
    assert_eq!(location.sessions, vec!["$2"]);
    assert_eq!(
        listing(vec![other, stale, elsewhere]).display_location(&instance, preferred("@5")),
        None
    );

    // Two windows across two sessions: the registry's display is the
    // primary, every session is a viewer session.
    let snap = listing(vec![
        probe_window("main:3", "@8", "$2", &ws),
        probe_window("probe:1", "@1", "$1", &ws),
    ]);
    let location = snap.display_location(&instance, preferred("@8")).unwrap();
    assert_eq!(asked.get(), 1);
    assert_eq!(
        (location.window.as_str(), location.session_id.as_str()),
        ("main:3", "$2")
    );
    assert_eq!(location.sessions, vec!["$1", "$2"]);
    // A cache naming neither: the first in id order, stably.
    let location = snap.display_location(&instance, preferred("@77")).unwrap();
    assert_eq!(
        (location.window.as_str(), location.session_id.as_str()),
        ("probe:1", "$1")
    );
    let location = snap.display_location(&instance, || None).unwrap();
    assert_eq!(location.window_id, "@1");

    // The createdAt tag matches numerically, in whatever spelling.
    let mut spelled = probe_window("probe:1", "@1", "$1", &ws);
    spelled.created = "123.0".to_string();
    assert!(listing(vec![spelled])
        .display_location(&instance, || None)
        .is_some());
    let mut untagged = probe_window("probe:1", "@1", "$1", &ws);
    untagged.created = String::new();
    assert!(listing(vec![untagged])
        .display_location(&instance, || None)
        .is_none());
}

#[test]
fn test_viewers_use_exact_current_sessions() {
    let env = sleep_probe_env();
    let snap = TickSnapshot::with_extras("ok", Vec::new(), HashMap::new(), None, None);
    let location = DisplayLocation {
        window: "probe:1".into(),
        window_id: "@1".into(),
        session_id: "$1".into(),
        sessions: vec!["$1".into(), "$2".into()],
    };
    let asked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let answers: Arc<Mutex<HashMap<String, Option<usize>>>> = Arc::new(Mutex::new(HashMap::new()));
    testhook::update(|h| {
        let asked = Arc::clone(&asked);
        let answers = Arc::clone(&answers);
        h.watching_clients = Some(Arc::new(move |session| {
            asked.lock().unwrap().push(session.to_string());
            answers.lock().unwrap()[session]
        }));
    });
    let set = |a: Option<usize>, b: Option<usize>| {
        let mut answers = answers.lock().unwrap();
        answers.insert("$1".to_string(), a);
        answers.insert("$2".to_string(), b);
    };
    let tick = |state: &mut SleepState, now: f64| {
        state.tick(
            &env.workspace,
            "probe",
            Some(&snap),
            Some(&location),
            true,
            "",
            now,
        )
    };

    // A terminal on either session is a viewer.
    set(Some(0), Some(1));
    let mut state = SleepState::default();
    assert!(!tick(&mut state, 0.0));
    assert!(!tick(&mut state, 601.0));
    assert_eq!(state.idle_since(), None);

    // A session tmux would not count is not "nobody".
    set(Some(0), None);
    let mut state = SleepState::default();
    assert!(!tick(&mut state, 0.0));
    assert!(!tick(&mut state, 601.0));
    assert_eq!(state.idle_since(), None);

    // Every session known empty: unwatched, at the threshold.
    set(Some(0), Some(0));
    let mut state = SleepState::default();
    assert!(!tick(&mut state, 0.0));
    assert_eq!(state.idle_since(), Some(0.0));
    assert!(!tick(&mut state, 599.0));
    assert!(tick(&mut state, 601.0));
    let events = display_events(&env, "hived.sleep");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["reason"], "unwatched");
    let lock_fd = state.take_retirement().unwrap().lock_fd;
    release_reexec_lock_fd_impl(Some(lock_fd));

    let asked = asked.lock().unwrap();
    assert!(!asked.is_empty());
    assert!(
        asked.iter().all(|s| s == "$1" || s == "$2"),
        "only the display's sessions are asked, by id: {asked:?}"
    );
}

#[test]
fn test_removed_team_ignores_recycled_foreign_window_id() {
    // An external workspace outlives its registry entry; the tmux server
    // restarted since this hived was born, and its `@1` is another team's
    // window now — with a pane still tagged for this team by name.
    let env = loop_probe_env("ok");
    let ws = env.workspace.clone();
    let ticks = Arc::new(AtomicUsize::new(0));
    let entry = crate::registry::entry_path("probe").unwrap();
    testhook::update(|h| {
        h.gl_idle_owned_keys = Some(Arc::new(|_| Some(Vec::new())));
        h.cv_journal_signature = Some(Arc::new(Vec::new));
        let recycled = ws.clone();
        h.list_windows_snapshot = Some(Arc::new(move || {
            let mut window = probe_window("other:0", "@1", "$0", &recycled);
            window.team = "other".to_string();
            (Some(vec![window]), "ok")
        }));
        h.list_panes_all_status = Some(Arc::new(|| {
            (
                Some(vec![PaneInfo {
                    pane_id: "%7".to_string(),
                    role: "agent".to_string(),
                    team: "probe".to_string(),
                    ..Default::default()
                }]),
                "ok",
            )
        }));
        let clock = Arc::clone(&ticks);
        h.monotonic = Some(Arc::new(move || clock.load(Ordering::SeqCst) as f64 * 30.0));
        let tick = Arc::clone(&ticks);
        let entry = entry.clone();
        h.wait_tick = Some(Arc::new(move || {
            let next = tick.fetch_add(1, Ordering::SeqCst) + 1;
            if next == 1 {
                fs::remove_file(&entry).unwrap();
            }
            next < 4
        }));
    });
    hived_loop(&ws, "probe", "probe:1", "@1");
    assert_eq!(
        ticks.load(Ordering::SeqCst),
        1,
        "the next 30s check retires the desk, not the 600s sleep"
    );
    assert!(display_events(&env, "hived.sleep").is_empty());
    assert!(!asleep_marker_path(&ws).exists());
    assert!(
        Path::new(&ws).is_dir(),
        "the external workspace is left alone"
    );
}

#[test]
fn test_removed_team_with_its_display_up_keeps_the_idle_policy() {
    let env = loop_probe_env("ok");
    let ws = env.workspace.clone();
    let ticks = Arc::new(AtomicUsize::new(0));
    let entry = crate::registry::entry_path("probe").unwrap();
    testhook::update(|h| {
        h.gl_idle_owned_keys = Some(Arc::new(|_| Some(Vec::new())));
        let clock = Arc::clone(&ticks);
        h.monotonic = Some(Arc::new(move || clock.load(Ordering::SeqCst) as f64 * 30.0));
        let tick = Arc::clone(&ticks);
        h.wait_tick = Some(Arc::new(move || {
            let next = tick.fetch_add(1, Ordering::SeqCst) + 1;
            if next == 1 {
                fs::remove_file(&entry).unwrap();
            }
            next < 4
        }));
    });
    hived_loop(&ws, "probe", "probe:1", "@1");
    assert_eq!(
        ticks.load(Ordering::SeqCst),
        4,
        "a window of this instance still on screen keeps the desk on the sleep policy"
    );
    assert!(display_events(&env, "hived.sleep").is_empty());
}

#[test]
fn test_orphan_interrupts_owned_node_without_unlinking_new_owner() {
    let env = loop_probe_env("ok");
    let ws = env.workspace.clone();
    let path = prepare_operation(&ws, "probe", "123", "nd-orphan", "worker", "node").unwrap();
    assert_eq!(
        saved_operation(Path::new(&ws), "nd-orphan")["state"],
        "prepared"
    );
    testhook::update(|h| {
        h.open_server_socket = Some(Arc::new(|workspace| {
            Ok(Box::new(open_server_socket(workspace)?) as Box<dyn HivedServerApi>)
        }));
        h.cleanup_socket = Some(Arc::new(cleanup_socket_impl));
        // Another generation took the owner file right after this one
        // published itself.
        h.write_hived_owner = Some(Arc::new(|workspace, pid, started_at, token| {
            write_hived_owner_impl(workspace, pid, started_at, token);
            write_hived_owner_impl(workspace, pid + 1, "next", "next-token");
        }));
        h.wait_tick = Some(Arc::new(|| panic!("an orphan never serves a tick")));
    });
    hived_loop(&ws, "probe", "probe:1", "@1");
    let retire = display_events(&env, "hived.retire_orphan");
    assert_eq!(retire.len(), 1);
    assert_eq!(retire[0]["socketPid"], Value::from(getpid() + 1));
    let record = saved_operation(Path::new(&ws), "nd-orphan");
    assert_eq!(record["state"], "terminal");
    assert_eq!(record["result"]["status"], "interrupted");
    assert_eq!(record["result"]["reason"], "hived replaced");
    assert_eq!(
        serde_json::from_str::<Value>(&owner_bytes(&ws)).unwrap()["token"],
        "next-token",
        "the new owner's file is not touched"
    );
    assert!(
        socket_path(&ws).exists(),
        "the socket is the new owner's; the orphan unlinks nothing"
    );
    drop(path);
    cleanup_socket_impl(&ws);
}

#[test]
fn test_team_removed_interrupts_node_in_external_workspace() {
    let env = loop_probe_env("ok");
    let ws = env.workspace.clone();
    prepare_operation(&ws, "probe", "123", "nd-removed", "worker", "node").unwrap();
    let ticks = Arc::new(AtomicUsize::new(0));
    let entry = crate::registry::entry_path("probe").unwrap();
    testhook::update(|h| {
        h.gl_idle_owned_keys = Some(Arc::new(|_| Some(Vec::new())));
        h.list_windows_snapshot = Some(Arc::new(|| (Some(Vec::new()), "ok")));
        let clock = Arc::clone(&ticks);
        h.monotonic = Some(Arc::new(move || clock.load(Ordering::SeqCst) as f64 * 30.0));
        let tick = Arc::clone(&ticks);
        h.wait_tick = Some(Arc::new(move || {
            let next = tick.fetch_add(1, Ordering::SeqCst) + 1;
            if next == 1 {
                fs::remove_file(&entry).unwrap();
            }
            next < 4
        }));
    });
    hived_loop(&ws, "probe", "probe:1", "@1");
    assert_eq!(ticks.load(Ordering::SeqCst), 1);
    let record = saved_operation(Path::new(&ws), "nd-removed");
    assert_eq!(record["state"], "terminal");
    assert_eq!(record["result"]["status"], "interrupted");
    assert_eq!(record["result"]["reason"], "team removed");
    assert!(Path::new(&ws).is_dir());
}

#[test]
fn test_workspace_removed_does_not_recreate_journal() {
    let env = loop_probe_env("ok");
    let ws = env.workspace.clone();
    prepare_operation(&ws, "probe", "123", "nd-gone", "worker", "node").unwrap();
    let journal = hooked_run_dir(&ws).join("operations");
    assert!(journal.is_dir());
    let ticks = Arc::new(AtomicUsize::new(0));
    testhook::update(|h| {
        h.gl_idle_owned_keys = Some(Arc::new(|_| Some(Vec::new())));
        let tick = Arc::clone(&ticks);
        let gone = ws.clone();
        h.wait_tick = Some(Arc::new(move || {
            let next = tick.fetch_add(1, Ordering::SeqCst) + 1;
            if next == 1 {
                fs::remove_dir_all(&gone).unwrap();
            }
            next < 4
        }));
    });
    hived_loop(&ws, "probe", "probe:1", "@1");
    assert_eq!(
        ticks.load(Ordering::SeqCst),
        1,
        "the next tick retires the desk"
    );
    assert!(
        !Path::new(&ws).exists(),
        "nothing of the removed workspace is written back: no journal, no lock, no marker"
    );
    assert!(display_events(&env, "hived.sleep").is_empty());
}
