//! Idle retirement of the team's desk; registry, bus and run files survive.

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::Value;

use super::*;

#[derive(Default)]
pub(super) struct SleepState {
    since: Option<f64>,
    usage: u64,
}

fn idle_owned_grok_keys(team: &str) -> Option<Vec<String>> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_idle_owned_keys.clone()).flatten() {
        return f(team);
    }
    crate::adapters::grok_leader::pool().idle_owned_keys(team)
}

fn absent_display(team: &str, window: &str, snap: Option<&TickSnapshot>) -> Option<&'static str> {
    let Some(snap) = snap else {
        return Some("display-unreachable");
    };
    if snap.panes.iter().any(|pane| pane.team == team) || hooked_is_tmux_window_alive(window) {
        return None;
    }
    if let Some(display) = crate::registry::load(team).and_then(|entry| {
        entry
            .get("display")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }) {
        if !display.is_empty() && display != window && hooked_is_tmux_window_alive(&display) {
            return None;
        }
    }
    Some("window-gone")
}

impl SleepState {
    pub(super) fn tick(
        &mut self,
        workspace: &str,
        team: &str,
        window: &str,
        snap: Option<&TickSnapshot>,
        server: &dyn HivedServerApi,
        now: f64,
    ) -> bool {
        let (working, usage) = {
            let state = admission().lock().unwrap_or_else(|e| e.into_inner());
            (state.leases > state.readers, state.usage)
        };
        let reason = absent_display(team, window, snap);
        let used = self.usage != usage;
        self.usage = usage;
        if reason.is_none()
            || working
            || used
            || pending_operations(workspace) != 0
            || idle_owned_grok_keys(team).is_none()
        {
            self.since = None;
            return false;
        }
        let since = *self.since.get_or_insert(now);
        if now - since < HIVED_SLEEP_AFTER_SECONDS || requests_in_flight() {
            // A read holds a lease through its reply, but does not renew the desk.
            return false;
        }
        self.since = None;
        if !self.finish(workspace, team, server, usage) {
            return false;
        }
        SHUTDOWN.store(true, Ordering::SeqCst);
        hooked_notify_debug_emit(
            workspace,
            "hived.sleep",
            &[
                ("team", Value::from(team)),
                ("idleSeconds", Value::from(now - since)),
                ("reason", Value::from(reason.unwrap())),
            ],
        );
        true
    }

    fn finish(&self, workspace: &str, team: &str, server: &dyn HivedServerApi, usage: u64) -> bool {
        // An accept racing the idle observation cancels sleep, even if its
        // handler finishes before the next coordinator observation.
        let quiet = {
            let mut state = admission().lock().unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            state.leases == 0 && state.usage == usage
        };
        if !quiet || !finish_shutdown(workspace, server, Duration::from_secs(5)) {
            reopen_admission();
            return false;
        }
        // Connections queued after the regular accept loop get an explicit
        // notAdmitted response and keep the desk available for their retry.
        if reject_draining_request(server) {
            reopen_admission();
            return false;
        }
        write_registry_backfill(workspace, team);
        let Some(keys) = idle_owned_grok_keys(team) else {
            reopen_admission();
            return false;
        };
        if pending_operations(workspace) != 0 {
            reopen_admission();
            return false;
        }
        for key in keys {
            hooked_gl_pool_drop_key(&key);
            hooked_gl_park_daemon_key(&key);
        }
        true
    }
}
