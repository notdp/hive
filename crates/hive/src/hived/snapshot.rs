//! The hived's per-tick view of the display: one `list-panes -a`, one
//! `list-windows -a` (the notify token) and one `ps`, answering every
//! per-pane question a tick asks — alive, window, CLI on the tty — as a
//! lookup. The per-pane probes (`is_pane_alive`, two `display-message`s
//! and a `ps -t` per pane) cost a fork each and scaled with the roster;
//! this costs three forks and scales with nothing.

use std::collections::HashMap;

use crate::agent_cli::CLIProfile;
use crate::tmux::{PaneExtra, PaneInfo, TTYProcessInfo};

use super::*;

pub(crate) struct TickSnapshot {
    /// `list_panes_all_status`'s verdict: `ok`, `no-server` or `unknown`.
    pub status: &'static str,
    pub panes: Vec<PaneInfo>,
    extras: HashMap<String, PaneExtra>,
    /// `session:index → notify token`; None when the listing did not answer.
    tokens: Option<HashMap<String, String>>,
    /// `ttys003 → processes`; None when `ps` did not answer.
    processes: Option<HashMap<String, Vec<TTYProcessInfo>>>,
}

pub(crate) fn notify_token_key() -> &'static str {
    crate::notify_ui::NOTIFY_TOKEN_OPTION.trim_start_matches('@')
}

impl TickSnapshot {
    /// Read the display once. A status other than `ok` carries no panes.
    pub(crate) fn collect() -> TickSnapshot {
        let (listing, status) = hooked_list_panes_snapshot_status();
        let Some((panes, extras)) = listing else {
            return TickSnapshot {
                status,
                panes: Vec::new(),
                extras: HashMap::new(),
                tokens: None,
                processes: None,
            };
        };
        TickSnapshot {
            status,
            panes,
            extras,
            tokens: hooked_list_window_option_all(notify_token_key()),
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
        match self.tokens.as_ref() {
            Some(tokens) => tokens.get(window).cloned(),
            None => hooked_get_window_option(window, notify_token_key()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_extras(
        status: &'static str,
        panes: Vec<PaneInfo>,
        extras: HashMap<String, PaneExtra>,
        tokens: Option<HashMap<String, String>>,
        processes: Option<HashMap<String, Vec<TTYProcessInfo>>>,
    ) -> TickSnapshot {
        TickSnapshot {
            status,
            panes,
            extras,
            tokens,
            processes,
        }
    }
}
