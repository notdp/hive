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

/// A sleep committed: the startup lock the final checks were made under,
/// held until the loop's teardown has closed the listener and unlinked the
/// socket, and the pool clients to drop only then.
pub(super) struct Retirement {
    pub(super) lock_fd: i32,
    grok_keys: Vec<String>,
}

impl Retirement {
    /// Only this desk's own clients go. A grok leader is the member's
    /// own process, and the TUI in its pane is one of its clients: a
    /// reap here would take the human's session down with it. Retiring
    /// for want of a viewer is not authority to collect someone else's
    /// engine. The price is a leader that outlives the desk; the next
    /// send reuses it if it is still up, else starts one from the
    /// session record.
    pub(super) fn drop_clients(self) {
        for key in &self.grok_keys {
            hooked_gl_pool_drop_key(key);
        }
    }
}

#[derive(Default)]
pub(super) struct SleepState {
    since: Option<f64>,
    usage: u64,
    retirement: Option<Retirement>,
}

impl SleepState {
    /// The commit of a tick that returned true, for the loop's teardown.
    pub(super) fn take_retirement(&mut self) -> Option<Retirement> {
        self.retirement.take()
    }

    #[cfg(test)]
    pub(super) fn idle_since(&self) -> Option<f64> {
        self.since
    }
}

fn idle_owned_grok_keys(team: &str) -> Option<Vec<String>> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_idle_owned_keys.clone()).flatten() {
        return f(team);
    }
    crate::adapters::grok_leader::pool().idle_owned_keys(team)
}

/// Terminals watching the display, over every session that holds one of
/// its windows: any known terminal is a viewer; nobody only when every
/// session answered zero. A session tmux would not count leaves the sum
/// unknown unless another already showed a viewer — an unknown count is
/// not "nobody". Control-mode clients (hive's own monitor) never count.
fn viewers(sessions: &[String]) -> Option<usize> {
    let mut total = 0;
    let mut unknown = false;
    for session in sessions {
        match hooked_watching_clients(session) {
            Some(n) => total += n,
            None => unknown = true,
        }
    }
    if total == 0 && unknown {
        None
    } else {
        Some(total)
    }
}

/// Why the display counts as absent this tick: the tmux server did not
/// answer, or no window on it carries this instance's tags. A window id
/// is never taken on its own: ids restart from `@0` when the tmux server
/// restarts, and a hived from before the restart would otherwise read
/// another team's `@0` as its display and never retire.
fn absent_display(
    snap: Option<&TickSnapshot>,
    location: Option<&DisplayLocation>,
) -> Option<&'static str> {
    if snap.is_none() {
        return Some("display-unreachable");
    }
    location.is_none().then_some("window-gone")
}

impl SleepState {
    /// One idle check against this tick's display (*snap*, None while
    /// tmux does not answer) and the display's current location on it.
    pub(super) fn tick(
        &mut self,
        workspace: &str,
        team: &str,
        snap: Option<&TickSnapshot>,
        location: Option<&DisplayLocation>,
        owner_token: &str,
        now: f64,
    ) -> bool {
        let (working, usage) = {
            let state = admission().lock().unwrap_or_else(|e| e.into_inner());
            (state.working(), state.usage)
        };
        let absent = absent_display(snap, location);
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
        // Asked only once the cheaper gates pass, so a busy desk never
        // pays for it.
        let reason = match (absent, location) {
            (Some(reason), _) => Some(reason),
            (None, Some(location)) => match viewers(&location.sessions) {
                Some(0) => Some("unwatched"),
                _ => None,
            },
            (None, None) => None,
        };
        let Some(reason) = reason else {
            self.since = None;
            return false;
        };
        let since = *self.since.get_or_insert(now);
        if now - since < HIVED_SLEEP_AFTER_SECONDS || requests_in_flight() {
            // A read, or a request whose action is still unread, holds a
            // lease through its reply but does not renew the desk.
            return false;
        }
        // A cancelled attempt keeps the clock: what cancelled it was an
        // arrival still unclassified, or a read. Real use shows up as
        // usage on the next tick and resets the clock there.
        let Some(retirement) = self.finish(workspace, team, owner_token, usage) else {
            return false;
        };
        self.since = None;
        self.retirement = Some(retirement);
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

    /// Shut the gate and commit the retirement, or reopen and say so.
    ///
    /// With the gate shut the accept worker refuses every arrival before
    /// any payload is written, and counts it. The backfill and the
    /// obligation checks run under the shut gate; the commit itself is
    /// made under the startup lock, against the owner file and the
    /// arrival count: a connection that was refused in between is a retry
    /// on its way, and this desk stays to take it.
    fn finish(
        &self,
        workspace: &str,
        team: &str,
        owner_token: &str,
        usage: u64,
    ) -> Option<Retirement> {
        let arrivals = {
            let mut state = admission().lock().unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            if state.leases != 0 || state.usage != usage {
                state.closed = false;
                return None;
            }
            state.arrivals
        };
        if !finish_shutdown(workspace, Duration::from_secs(5)) {
            reopen_admission();
            return None;
        }
        write_registry_backfill(workspace, team);
        let Some(grok_keys) = idle_owned_grok_keys(team) else {
            reopen_admission();
            return None;
        };
        if pending_operations(workspace) != 0 {
            reopen_admission();
            return None;
        }
        // A starter holding the lock is mid-ensure: it finds this desk
        // up, or replaces it. Either way this attempt is off.
        let Some(lock_fd) = hooked_try_acquire_reexec_lock(workspace) else {
            reopen_admission();
            return None;
        };
        let owner_replaced = foreign_owner_pid(workspace, owner_token).is_some();
        let quiet = {
            let state = admission().lock().unwrap_or_else(|e| e.into_inner());
            state.leases == 0 && state.usage == usage && state.arrivals == arrivals
        };
        if owner_replaced || !quiet {
            // Another generation owns the socket: the owner check retires
            // this one as an orphan on its next pass.
            hooked_release_reexec_lock_fd(Some(lock_fd));
            reopen_admission();
            return None;
        }
        Some(Retirement { lock_fd, grok_keys })
    }
}
