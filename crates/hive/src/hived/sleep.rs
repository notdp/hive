//! Idle retirement of the team's desk; registry, bus and run files survive.

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::Value;

use super::*;

/// Where a retired desk says why it left: `{"reason": …, "at": …}` under
/// the workspace's run dir, removed by the next generation's start. The
/// `unwatched` reason is what `hive wake` acts on — the session hooks bring
/// back only a desk that retired for want of a viewer, never start one a
/// team has not asked for.
pub fn asleep_marker_path(workspace: &str) -> std::path::PathBuf {
    crate::devlog::run_dir(std::path::Path::new(workspace)).join("desk.asleep")
}

/// The reason recorded in a marker's text, if it is one.
pub fn asleep_reason(marker: &str) -> Option<String> {
    serde_json::from_str::<Value>(marker)
        .ok()?
        .get("reason")?
        .as_str()
        .map(str::to_owned)
}

fn write_asleep_marker(workspace: &str, reason: &str) {
    let path = asleep_marker_path(workspace);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = serde_json::json!({"reason": reason, "at": crate::gc::epoch_now()});
    let _ = std::fs::write(path, format!("{body}\n"));
}

pub(super) fn clear_asleep_marker(workspace: &str) {
    let _ = std::fs::remove_file(asleep_marker_path(workspace));
}

#[derive(Default)]
pub(super) struct SleepState {
    since: Option<f64>,
    usage: u64,
    /// The session the team window sits in (from its `session:index`
    /// target): whose terminals count as viewers.
    session: String,
}

impl SleepState {
    pub(super) fn for_window(tmux_window: &str) -> Self {
        SleepState {
            session: tmux_window
                .split_once(':')
                .map_or("", |(session, _)| session)
                .to_string(),
            ..Default::default()
        }
    }
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
        let absent = absent_display(team, window, snap);
        let used = self.usage != usage;
        self.usage = usage;
        if working
            || used
            || pending_operations(workspace) != 0
            || idle_owned_grok_keys(team).is_none()
        {
            self.since = None;
            return false;
        }
        // A window no terminal is attached to is a picture nobody sees:
        // the status bar and colours this desk keeps up have no viewer.
        // hive's own control-mode monitor is not one; a count tmux will
        // not give is not "nobody". Asked only once the cheaper gates
        // pass, so a busy desk never pays for it.
        let reason = match absent {
            Some(reason) => Some(reason),
            None => match hooked_watching_clients(&self.session) {
                Some(0) => Some("unwatched"),
                _ => None,
            },
        };
        let Some(reason) = reason else {
            self.since = None;
            return false;
        };
        let since = *self.since.get_or_insert(now);
        if now - since < HIVED_SLEEP_AFTER_SECONDS || requests_in_flight() {
            // A read holds a lease through its reply, but does not renew the desk.
            return false;
        }
        self.since = None;
        if !self.finish(workspace, team, server, usage) {
            return false;
        }
        write_asleep_marker(workspace, reason);
        SHUTDOWN.store(true, Ordering::SeqCst);
        hooked_notify_debug_emit(
            workspace,
            "hived.sleep",
            &[
                ("team", Value::from(team)),
                ("idleSeconds", Value::from(now - since)),
                ("reason", Value::from(reason)),
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
