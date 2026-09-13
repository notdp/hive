//! The hived's per-tick view of the display: one `list-panes -a`, one
//! `list-windows -a` (the windows' instance tags and notify tokens) and
//! one `ps`, answering every per-pane question a tick asks — alive,
//! window, CLI on the tty — as a lookup, and where this team's display is
//! right now. The per-pane probes (`is_pane_alive`, two `display-message`s
//! and a `ps -t` per pane) cost a fork each and scaled with the roster;
//! this costs three forks and scales with nothing.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::agent_cli::CLIProfile;
use crate::tmux::{PaneExtra, PaneInfo, TTYProcessInfo, WindowExtra};

use super::*;

pub(crate) struct TickSnapshot {
    /// `list_panes_all_status`'s verdict: `ok`, `no-server` or `unknown`.
    pub status: &'static str,
    pub panes: Vec<PaneInfo>,
    extras: HashMap<String, PaneExtra>,
    /// `session:index → window`; None when the listing did not answer.
    windows: Option<HashMap<String, WindowExtra>>,
    /// `ttys003 → processes`; None when `ps` did not answer.
    processes: Option<HashMap<String, Vec<TTYProcessInfo>>>,
}

pub(crate) fn notify_token_key() -> &'static str {
    crate::notify_ui::NOTIFY_TOKEN_OPTION.trim_start_matches('@')
}

/// The team instance a hived serves: its name, the workspace it was
/// started on and the `createdAt` of its registry entry. A window is this
/// instance's display only when all three tags agree — the same name on a
/// window of an earlier instance, or of another workspace, is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TeamInstance {
    pub team: String,
    pub workspace: String,
    /// `team::created_at_key` of the entry; empty when it had none.
    pub created: String,
}

impl TeamInstance {
    /// The instance recorded for *team* under this hive home, on
    /// *workspace*. Read once at start: the entry names the instance for
    /// the hived's whole generation.
    pub(crate) fn from_registry(team: &str, workspace: &str) -> TeamInstance {
        let created = crate::registry::load(team)
            .map(|entry| TeamInstance::from_entry(&entry).created)
            .unwrap_or_default();
        TeamInstance {
            team: team.to_string(),
            workspace: workspace.to_string(),
            created,
        }
    }

    /// The instance a registry *entry* names.
    pub(crate) fn from_entry(entry: &Map<String, Value>) -> TeamInstance {
        let created = entry
            .get("createdAt")
            .map(|created| match created {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .and_then(|text| text.trim().parse::<f64>().ok())
            .map(crate::team::created_at_key)
            .unwrap_or_default();
        TeamInstance {
            team: crate::json_fields::map_str(entry, "team"),
            workspace: crate::json_fields::map_str(entry, "workspace"),
            created,
        }
    }

    /// Whether a window's tags name this instance.
    pub(crate) fn owns(&self, window: &WindowExtra) -> bool {
        window.team == self.team
            && same_workspace(&window.workspace, &self.workspace)
            && same_created(&window.created, &self.created)
    }
}

/// Whether *window* is the display of a team this hive home holds: its
/// tags name a registered instance — the same team, workspace and
/// `createdAt` — under the current `HIVE_HOME`. A window of the same team
/// name from another home or an earlier instance is not.
pub(crate) fn window_of_this_home(window: &WindowExtra) -> bool {
    if window.team.is_empty() {
        return false;
    }
    crate::registry::load(&window.team)
        .is_some_and(|entry| TeamInstance::from_entry(&entry).owns(window))
}

/// Whether any of *windows* in session *session_id* is a display of this
/// hive home's: what decides whether the home's wake hooks stay on that
/// session once one team's display leaves it.
pub(crate) fn home_displays_in<'a>(
    session_id: &str,
    windows: impl IntoIterator<Item = &'a WindowExtra>,
) -> bool {
    windows
        .into_iter()
        .any(|window| window.session_id == session_id && window_of_this_home(window))
}

/// Two workspace spellings name the same directory: the tag was written
/// from the same string the hived was started with, so this is an equality
/// with the resolved path as the tie-breaker (`/private/tmp` for `/tmp`).
pub(crate) fn same_workspace(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a).ok(), std::fs::canonicalize(b).ok()) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Two `createdAt` keys name the same instance: the same epoch second, in
/// whatever spelling each side carries. An empty key names nothing.
pub(crate) fn same_created(a: &str, b: &str) -> bool {
    match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Where the team's display is on this tick: the primary window (its
/// `session:index` target, id and session) and every session that holds a
/// window of this instance — the sessions whose terminals count as
/// viewers. Resolved from the tick snapshot, never cached across a move.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DisplayLocation {
    pub window: String,
    pub window_id: String,
    /// The primary window's session id (`$3`): what the busy monitor
    /// attaches to and the idle notifier reads the active window from.
    pub session_id: String,
    /// Session ids of every window of this instance, deduplicated and in
    /// a stable order.
    pub sessions: Vec<String>,
}

/// `$3 → 3`, for a stable numeric order; anything else sorts after.
fn tmux_id_order(id: &str) -> (u64, String) {
    match id.get(1..).and_then(|n| n.parse::<u64>().ok()) {
        Some(n) if id.starts_with('$') || id.starts_with('@') => (n, String::new()),
        _ => (u64::MAX, id.to_string()),
    }
}

impl TickSnapshot {
    /// Read the display once. A status other than `ok` carries no panes.
    pub(crate) fn collect() -> TickSnapshot {
        let (listing, status) = hooked_list_panes_snapshot_status();
        let unreachable = |status| TickSnapshot {
            status,
            panes: Vec::new(),
            extras: HashMap::new(),
            windows: None,
            processes: None,
        };
        let Some((panes, extras)) = listing else {
            return unreachable(status);
        };
        let (windows, status) = hooked_list_windows_snapshot(notify_token_key());
        if status != "ok" {
            return unreachable(status);
        }
        TickSnapshot {
            status,
            panes,
            extras,
            windows,
            processes: hooked_list_all_tty_processes(),
        }
    }

    pub(crate) fn reachable(&self) -> bool {
        self.status == "ok"
    }

    /// A listed pane that is not `pane_dead`. A pane the listing does not
    /// hold is gone, and is never probed again.
    pub(crate) fn is_alive(&self, pane_id: &str) -> bool {
        match self.extras.get(pane_id) {
            Some(extra) => !extra.dead,
            None => {
                // A test fixture that lists panes without snapshot columns
                // answers per pane through the seam it hooked.
                #[cfg(test)]
                if self.extras.is_empty() {
                    return hooked_is_pane_alive(pane_id);
                }
                false
            }
        }
    }

    /// `session:index` of the pane's window.
    pub(crate) fn window_of(&self, pane_id: &str) -> Option<String> {
        match self.extras.get(pane_id) {
            Some(extra) => Some(extra.window.clone()).filter(|w| !w.is_empty()),
            None => {
                #[cfg(test)]
                if self.extras.is_empty() {
                    return hookget(|h| h.get_pane_window_target.clone())
                        .flatten()
                        .and_then(|f| f(pane_id));
                }
                None
            }
        }
    }

    /// The CLI on the pane, from its current command and the processes on
    /// its tty (`agent_cli::detect_cli_process`).
    pub(crate) fn cli_profile(&self, pane_id: &str) -> Option<&'static CLIProfile> {
        let Some(extra) = self.extras.get(pane_id) else {
            #[cfg(test)]
            if self.extras.is_empty() {
                return hooked_detect_cli_process_for_pane(pane_id);
            }
            return None;
        };
        let Some(processes) = self.processes.as_ref() else {
            // `ps` did not answer this tick: one probe for this pane beats a
            // blind verdict.
            return hooked_detect_cli_process_for_pane(pane_id);
        };
        let command = self
            .panes
            .iter()
            .find(|p| p.pane_id == pane_id)
            .map(|p| p.command.as_str())
            .unwrap_or("");
        let on_tty = processes
            .get(crate::tmux::tty_key(&extra.tty))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        crate::agent_cli::detect_cli_process(pane_id, command, on_tty)
    }

    /// The window's notify token from the one `list-windows` read; when
    /// that read did not answer, the window is asked directly.
    pub(crate) fn window_token(&self, window: &str) -> Option<String> {
        match self.windows.as_ref() {
            Some(windows) => windows
                .get(window)
                .map(|w| w.token.clone())
                .filter(|token| !token.is_empty()),
            None => hooked_get_window_option(window, notify_token_key()),
        }
    }

    /// Whether a display of this hive home's is still in *session_id* on
    /// this tick; an unanswered window listing reads as "still there", so
    /// no hook is removed on a read that might be wrong.
    pub(crate) fn home_displays_in(&self, session_id: &str) -> bool {
        match self.windows.as_ref() {
            Some(windows) => home_displays_in(session_id, windows.values()),
            None => true,
        }
    }

    /// This instance's display on this tick, from the windows' own tags:
    /// None when no window carries them. With several such windows the
    /// primary is the one *preferred* names (the registry's display cache,
    /// asked only then), else the first in id order; every session they sit
    /// in is a viewer session.
    pub(crate) fn display_location(
        &self,
        instance: &TeamInstance,
        preferred: impl FnOnce() -> Option<String>,
    ) -> Option<DisplayLocation> {
        let windows = self.windows.as_ref()?;
        let mut owned: Vec<&WindowExtra> = windows
            .values()
            .filter(|window| !window.window_id.is_empty() && instance.owns(window))
            .collect();
        if owned.is_empty() {
            return None;
        }
        owned.sort_by_key(|window| {
            (
                tmux_id_order(&window.session_id),
                tmux_id_order(&window.window_id),
            )
        });
        let primary = if owned.len() > 1 {
            preferred()
                .and_then(|id| owned.iter().find(|window| window.window_id == id).copied())
                .unwrap_or(owned[0])
        } else {
            owned[0]
        };
        let mut sessions: Vec<String> = owned
            .iter()
            .map(|window| window.session_id.clone())
            .filter(|session| !session.is_empty())
            .collect();
        sessions.dedup();
        Some(DisplayLocation {
            window: primary.window.clone(),
            window_id: primary.window_id.clone(),
            session_id: primary.session_id.clone(),
            sessions,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_extras(
        status: &'static str,
        panes: Vec<PaneInfo>,
        extras: HashMap<String, PaneExtra>,
        windows: Option<HashMap<String, WindowExtra>>,
        processes: Option<HashMap<String, Vec<TTYProcessInfo>>>,
    ) -> TickSnapshot {
        TickSnapshot {
            status,
            panes,
            extras,
            windows,
            processes,
        }
    }
}
