// --------------------------------------------------------------------------
// module state (process globals; nextest gives one process per test)
// --------------------------------------------------------------------------

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Map, Value};

use crate::runtime_snapshot::RuntimeSnapshotStore;

/// The pane-output busy monitor the serve loop holds; tests install fakes.
pub trait OutputMonitor: Send + Sync {
    fn is_busy(&self, pane_id: &str, threshold_seconds: f64) -> bool;
    fn last_output_age(&self, pane_id: &str) -> Option<f64>;
    fn start(&self) {}
    fn stop(&self) {}
}

impl OutputMonitor for crate::tmux::ControlModeOutputMonitor {
    fn is_busy(&self, pane_id: &str, threshold_seconds: f64) -> bool {
        crate::tmux::ControlModeOutputMonitor::is_busy(self, pane_id, threshold_seconds)
    }
    fn last_output_age(&self, pane_id: &str) -> Option<f64> {
        crate::tmux::ControlModeOutputMonitor::last_output_age(self, pane_id)
    }
    fn start(&self) {
        crate::tmux::ControlModeOutputMonitor::start(self)
    }
    fn stop(&self) {
        crate::tmux::ControlModeOutputMonitor::stop(self)
    }
}

#[allow(clippy::type_complexity)]
fn output_busy_monitor() -> &'static Mutex<Option<Arc<dyn OutputMonitor>>> {
    static CELL: OnceLock<Mutex<Option<Arc<dyn OutputMonitor>>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

pub(crate) fn set_output_busy_monitor(monitor: Option<Arc<dyn OutputMonitor>>) {
    *output_busy_monitor()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = monitor;
}

pub(crate) fn get_output_busy_monitor() -> Option<Arc<dyn OutputMonitor>> {
    output_busy_monitor()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[allow(clippy::type_complexity)]
pub(super) fn transcript_path_cache() -> &'static Mutex<HashMap<String, (String, f64, String)>> {
    static CELL: OnceLock<Mutex<HashMap<String, (String, f64, String)>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Where each workflow node's task landed, by dispatch id: the engine
/// handle `node-result` reads the turn's end and text under. Held only by
/// the hived that dispatched — a restarted hived knows no handles, and
/// answers `unknown` for every dispatch before it.
pub(super) fn node_turns() -> &'static Mutex<HashMap<String, crate::agent::TurnHandle>> {
    static CELL: OnceLock<Mutex<HashMap<String, crate::agent::TurnHandle>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn runtime_snapshots() -> &'static Mutex<RuntimeSnapshotStore> {
    static CELL: OnceLock<Mutex<RuntimeSnapshotStore>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(RuntimeSnapshotStore::default()))
}

#[allow(clippy::type_complexity)]
pub(super) fn claude_jobs_cache(
) -> &'static Mutex<Option<(f64, Option<HashMap<String, Map<String, Value>>>)>> {
    static CELL: OnceLock<Mutex<Option<(f64, Option<HashMap<String, Map<String, Value>>>)>>> =
        OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// Panes the hived handed a message to and has not seen busy since: the
/// status tick's `@hive-unread`. The turn that reads the message clears it.
pub(super) fn unread_pending() -> &'static Mutex<HashSet<String>> {
    static CELL: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(super) fn codex_reattach_at() -> &'static Mutex<HashMap<String, f64>> {
    static CELL: OnceLock<Mutex<HashMap<String, f64>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) static SHUTDOWN: AtomicBool = AtomicBool::new(false);
pub(super) static INFLIGHT_REQUESTS: AtomicI64 = AtomicI64::new(0);

pub(crate) fn requests_in_flight() -> bool {
    INFLIGHT_REQUESTS.load(Ordering::SeqCst) > 0
}
