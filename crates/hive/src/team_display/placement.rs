//! Move an orch's existing viewer into a team session. Until the registry
//! commit, the temporary shell keeps its place in the source window so a
//! failed create can swap the viewer back.

use anyhow::{anyhow, bail, Context, Result};

use crate::tmux;

pub(crate) struct OrchSource {
    pub(crate) pane: String,
    pub(crate) window: String,
    pub(crate) session: String,
    pub(crate) cwd: String,
    panes: usize,
    zoomed: bool,
    client: Option<String>,
    tags: Vec<(&'static str, Option<String>)>,
}

fn pane_value(pane: &str, format: &str) -> Result<String> {
    let value = tmux::run_output(&["display-message", "-t", pane, "-p", format])?;
    if value.is_empty() {
        bail!("tmux did not report {format} for pane {pane}");
    }
    Ok(value)
}

impl OrchSource {
    pub(crate) fn read(pane: &str) -> Result<Self> {
        let window = pane_value(pane, "#{window_id}")?;
        match pane_value(pane, "#{window_linked}")?.as_str() {
            "0" => {}
            "1" => {
                bail!("window {window} is linked across sessions; create from an unlinked window")
            }
            _ => bail!("tmux did not report whether window {window} is linked"),
        }
        let panes = pane_value(pane, "#{window_panes}")?.parse::<usize>()?;
        if panes == 0 {
            bail!("source window {window} has no panes");
        }
        Ok(Self {
            client: tmux::sole_window_client(&window),
            tags: ["hive-role", "hive-agent", "hive-team", "hive-cli"]
                .into_iter()
                .map(|key| (key, tmux::get_pane_option(pane, key)))
                .collect(),
            session: pane_value(pane, "#{session_name}")?,
            cwd: pane_value(pane, "#{pane_current_path}")?,
            zoomed: pane_value(pane, "#{window_zoomed_flag}")? == "1",
            pane: pane.to_string(),
            window,
            panes,
        })
    }

    pub(crate) fn place(self, team: &str) -> Result<OrchPlacement> {
        let new_session = !super::checked_team_session(team)?;
        let shell = if new_session {
            tmux::new_session(team, super::TEAM_SESSION_COLS, super::TEAM_SESSION_ROWS)?
        } else {
            tmux::new_window(&format!("={team}"), team, Some(&self.cwd), true)?.1
        };
        if shell.is_empty() {
            bail!("tmux did not report the new team's shell pane");
        }
        let mut placement = OrchPlacement {
            source: self,
            shell,
            window: String::new(),
            window_id: String::new(),
        };
        let prepared = (|| -> Result<()> {
            placement.window = pane_value(&placement.shell, "#{session_name}:#{window_index}")?;
            placement.window_id = pane_value(&placement.shell, "#{window_id}")?;
            if new_session {
                crate::terminal_handoff::set_session_roots(&placement.shell)?;
                // This shell stays in the source window if it was the only
                // pane. Start it with the source cwd and the team's roots.
                let command = format!(
                    "cd {} && exec \"$SHELL\"",
                    crate::shell::shlex_quote(&placement.source.cwd)
                );
                tmux::respawn_pane(&placement.shell, &command)?;
            }
            tmux::run(
                &["rename-window", "-t", &placement.window_id, team],
                true,
                5,
            )?;
            tmux::run(
                &[
                    "set-window-option",
                    "-t",
                    &placement.window_id,
                    "@hive-built",
                    "1",
                ],
                true,
                5,
            )?;
            let session_id = pane_value(&placement.shell, "#{session_id}")?;
            tmux::install_team_status_checked(&session_id)?;
            tmux::swap_pane_checked(&placement.source.pane, &placement.shell)?;
            if pane_value(&placement.source.pane, "#{window_id}")? != placement.window_id {
                bail!("orch pane did not arrive in the team window");
            }
            Ok(())
        })();
        if let Err(error) = prepared {
            return Err(placement.rollback_error(error));
        }
        Ok(placement)
    }
}

pub(crate) struct OrchPlacement {
    source: OrchSource,
    shell: String,
    pub(crate) window: String,
    pub(crate) window_id: String,
}

impl OrchPlacement {
    /// Publish the pane binding before the registry makes the member visible.
    pub(crate) fn bind(&self, team: &str, cli: &str) -> Result<()> {
        for (key, value) in [
            ("hive-role", "agent"),
            ("hive-agent", crate::team::LEAD_AGENT_NAME),
            ("hive-team", team),
            ("hive-cli", cli),
        ] {
            tmux::run(
                &[
                    "set-option",
                    "-p",
                    "-t",
                    &self.source.pane,
                    &format!("@{key}"),
                    value,
                ],
                true,
                5,
            )?;
        }
        Ok(())
    }

    /// Keep the original error, and report recovery failure without killing
    /// a window that may still contain the caller's viewer.
    pub(crate) fn rollback_error(&mut self, error: anyhow::Error) -> anyhow::Error {
        match self.rollback() {
            Ok(()) => error,
            Err(recovery) => anyhow!(
                "{error}; restoring pane {} failed: {recovery}",
                self.source.pane
            ),
        }
    }

    fn rollback(&mut self) -> Result<()> {
        // A timed-out swap may already have moved the pane. Read its
        // location before deciding whether the temporary shell is safe to
        // close; an unknown location leaves both panes for recovery.
        let actual = pane_value(&self.source.pane, "#{window_id}")?;
        if actual == self.window_id {
            tmux::swap_pane_checked(&self.source.pane, &self.shell)
                .context("swap back to the source window")?;
        } else if actual != self.source.window {
            bail!("orch pane is in unexpected window {actual}; both panes were left intact");
        }
        for (key, value) in &self.source.tags {
            let key = format!("@{key}");
            let mut args = vec!["set-option", "-p", "-t", &self.source.pane];
            if let Some(value) = value {
                args.extend([key.as_str(), value.as_str()]);
            } else {
                args.extend(["-u", key.as_str()]);
            }
            tmux::run(&args, true, 5).context("restore the source pane's tags")?;
        }
        // The only pane we created is back in the new window. Killing it
        // closes that window, and its new session if it is now empty.
        tmux::run(&["kill-pane", "-t", &self.shell], true, 5)?;
        if self.source.zoomed && pane_value(&self.source.pane, "#{window_zoomed_flag}")? == "0" {
            tmux::run(&["resize-pane", "-Z", "-t", &self.source.pane], true, 5)?;
        }
        Ok(())
    }

    /// Called only after the registry commit. Errors here leave membership
    /// intact; the human can open the registered window with `hive attach`.
    pub(crate) fn finish(self) -> bool {
        if self.source.panes > 1 {
            if let Err(error) = tmux::run(&["kill-pane", "-t", &self.shell], true, 5) {
                eprintln!(
                    "hive: team registered; removing temporary shell {} failed: {error}",
                    self.shell
                );
            }
        }
        let Some(client) = self.source.client else {
            return false;
        };
        if let Err(error) = tmux::switch_named_client(&client, &self.window) {
            eprintln!("hive: team registered; switching client {client} failed: {error}");
            return false;
        }
        true
    }
}
