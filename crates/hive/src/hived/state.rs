// --------------------------------------------------------------------------
// module state (process globals; nextest gives one process per test)
// --------------------------------------------------------------------------

use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
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
pub(super) static FORCE_SHUTDOWN: AtomicBool = AtomicBool::new(false);
/// Admission and the outstanding counts share one lock. The accept worker
/// accepts without blocking and reserves the handler lease under this lock
/// while the gate is open; while it is shut, an accepted connection is
/// only counted as an arrival and refused, holding no lease.
/// Waiting for listener readiness holds no lease and cannot postpone sleep.
///
/// A lease is `unclassified` from accept until the handler has read the
/// request's action: it delays the final exit but says nothing about
/// whether the desk is in use. Only a classified write bumps `usage`.
#[derive(Default)]
pub(super) struct Admission {
    pub closed: bool,
    pub leases: usize,
    pub readers: usize,
    pub unclassified: usize,
    pub usage: u64,
    /// Connections accepted while the gate was shut. Sleep compares the
    /// count before and after its final checks: an arrival in between, even
    /// one that was refused, cancels this retirement so the retry lands.
    pub arrivals: u64,
}

impl Admission {
    /// Leases that are a request in progress: not a reader, not one whose
    /// action is still unread.
    pub(super) fn working(&self) -> bool {
        self.leases > self.readers + self.unclassified
    }
}

pub(super) fn admission() -> &'static Mutex<Admission> {
    static CELL: OnceLock<Mutex<Admission>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(Admission::default()))
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum LeaseKind {
    #[default]
    Unclassified,
    Read,
    Write,
}

pub(super) struct RequestLease {
    kind: LeaseKind,
}

impl RequestLease {
    /// Reserve a lease under the admission lock the caller holds.
    pub(super) fn reserve(state: &mut Admission) -> RequestLease {
        state.leases += 1;
        state.unclassified += 1;
        RequestLease {
            kind: LeaseKind::Unclassified,
        }
    }

    pub(super) fn classify(&mut self, action: &str) {
        let mut state = admission().lock().unwrap_or_else(|e| e.into_inner());
        if self.kind != LeaseKind::Unclassified {
            return;
        }
        state.unclassified -= 1;
        if read_only_request(action) {
            self.kind = LeaseKind::Read;
            state.readers += 1;
        } else {
            self.kind = LeaseKind::Write;
            state.usage = state.usage.wrapping_add(1);
        }
    }
}

pub(super) fn read_only_request(action: &str) -> bool {
    matches!(
        action,
        "ping" | "doctor" | "team-runtime" | "runtime-snapshot" | "node-result" | "turn-open"
    )
}

/// The actions with a side effect on the line: a new-format client sends
/// them only after the hived admitted the request on the same connection,
/// and the hived refuses them without that preflight.
pub(crate) fn admission_required(action: &str) -> bool {
    matches!(
        action,
        "send" | "node-dispatch" | "connect-codex" | "connect-grok" | "revive"
    )
}

impl Drop for RequestLease {
    fn drop(&mut self) {
        let mut state = admission().lock().unwrap_or_else(|e| e.into_inner());
        state.leases -= 1;
        match self.kind {
            LeaseKind::Unclassified => state.unclassified -= 1,
            LeaseKind::Read => state.readers -= 1,
            LeaseKind::Write => {}
        }
    }
}

pub(super) fn close_admission() -> bool {
    let mut state = admission().lock().unwrap_or_else(|e| e.into_inner());
    state.closed = true;
    state.leases == 0
}

pub(super) fn reopen_admission() {
    admission().lock().unwrap_or_else(|e| e.into_inner()).closed = false;
}

pub(crate) fn requests_in_flight() -> bool {
    admission().lock().unwrap_or_else(|e| e.into_inner()).leases > 0
}
