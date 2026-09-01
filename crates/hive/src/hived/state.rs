// --------------------------------------------------------------------------
// module state (Python module globals; nextest gives one process per test)
// --------------------------------------------------------------------------

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Map, Value};

use crate::runtime_snapshot::RuntimeSnapshotStore;

/// Public `busy` monitor duck type (Python passes the monitor object around).
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

pub fn _set_output_busy_monitor(monitor: Option<Arc<dyn OutputMonitor>>) {
    *output_busy_monitor()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = monitor;
}

pub(super) fn _get_output_busy_monitor() -> Option<Arc<dyn OutputMonitor>> {
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

pub(super) fn codex_reattach_at() -> &'static Mutex<HashMap<String, f64>> {
    static CELL: OnceLock<Mutex<HashMap<String, f64>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) static _SHUTDOWN: AtomicBool = AtomicBool::new(false);
pub(super) static _INFLIGHT_REQUESTS: AtomicI64 = AtomicI64::new(0);

pub fn _requests_in_flight() -> bool {
    _INFLIGHT_REQUESTS.load(Ordering::SeqCst) > 0
}
