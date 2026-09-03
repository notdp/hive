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

use crate::agent::{Agent, DeliveryError};
use crate::runtime_snapshot::RuntimeSnapshot;
use crate::team::Team;
use crate::{bus, devlog};

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
        let mut hook =
            interactive_claude_pane(tmp.path(), Some((status.to_string(), String::new())), true);
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
    let events: Arc<Mutex<Vec<(String, Map<String, Value>)>>> = Arc::new(Mutex::new(Vec::new()));

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

    _cleanup_dead_daemons("/tmp/ws", "honey");

    assert!(env.calls.lock().unwrap().is_empty());
}

#[test]
fn test_cleanup_reaps_dead_pane_and_logs_before_kill() {
    let env = reap_env(false);
    *env.keys.lock().unwrap() = vec!["p4".to_string()];

    _cleanup_dead_daemons("/tmp/ws", "honey");

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

    _cleanup_dead_daemons("/tmp/ws", "honey");

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

    _cleanup_dead_daemons("/tmp/ws", "honey");

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

    _cleanup_dead_daemons("/tmp/ws", "honey");

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

    _cleanup_dead_daemons("/tmp/ws", "acc-throwaway");

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
    _cleanup_dead_daemons("/tmp/ws", "honey");
    assert!(env.calls.lock().unwrap().is_empty());

    // past the grace window with no registry entry: orphan
    write_pidfile(env.tmp.path(), "m-honey.rex", 999.0);
    _cleanup_dead_daemons("/tmp/ws", "honey");
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
    let events: Arc<Mutex<Vec<(String, Map<String, Value>)>>> = Arc::new(Mutex::new(Vec::new()));
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
fn test_open_server_socket_relocates_and_links_for_a_long_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp
        .path()
        .join("w".repeat(crate::devlog::max_socket_path_len()));
    let workspace = workspace.to_string_lossy().to_string();
    let sock = _socket_path(&workspace);
    let link = _socket_link_path(&workspace);
    assert_ne!(
        sock, link,
        "a workspace this deep cannot host its socket in tree"
    );
    assert!(sock.as_os_str().len() <= crate::devlog::max_socket_path_len());

    let server = _open_server_socket(&workspace).unwrap();
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
    let client = thread::spawn(move || _request_hived(&ws_client, &action_payload("ping"), 5.0));
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
    _cleanup_socket_impl(&workspace);
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
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("HIVE_HOME", tmp.path().join(".hive"));
    std::env::set_var(_HIVED_REEXEC_LOCK_ENV, "78");
    let workspace = tmp.path().to_string_lossy().to_string();
    let events: Arc<Mutex<Vec<(String, Vec<(String, Value)>)>>> = Arc::new(Mutex::new(Vec::new()));
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

    _hived_loop(&workspace, "team-a", "", "");

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
        let _ = (&conn).write_all(b"{\"ok\": true, \"msgId\": \"x1\", \"delivery\": \"queued\"}\n");
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
    hook.agent_send = Some(Arc::new(|_agent, _text, _sender| {
        Ok("udsWriteAccepted".to_string())
    }));
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

    send_payload_for_test(&workspace, "yoyo", "orch", "hi", "", "");
    send_payload_for_test(&workspace, "other.guest", "orch", "hi", "", "");
    send_payload_for_test(&workspace, "ccd.desk", "orch", "hi", "", "");

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

    let payload = send_payload_for_test(&workspace, "impl", "flow.run", "done", "/tmp/a.md", "m1");

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
