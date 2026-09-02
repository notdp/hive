//! hive::flow_rig — `hive flow rig`: the workflow team as one verb.
//!
//! A run gets a tmux session, a team and a board, all named after it
//! (session = team = run). The session's first pane becomes the dock
//! (`hive flow board`), an optional second pane mirrors the orchestrating
//! Claude session read-only (`hive view`), and members spawn above the dock
//! as nodes run — the first spawn anchors on the dock pane, and the
//! dock-aware layout tiles from there. `--down` retires every member,
//! deletes the team and kills the session.

use std::path::Path;

use anyhow::{bail, Context as _, Result};

use crate::team::{Team, LEAD_AGENT_NAME};
use crate::tmux;

const SESSION_COLS: u32 = 220;
const SESSION_ROWS: u32 = 60;

pub fn rig_cmd(run: &str, orch: Option<&str>, workspace: Option<&str>, down: bool) -> i32 {
    let outcome = if down {
        rig_down(run)
    } else {
        rig_up(run, orch, workspace)
    };
    match outcome {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e:#}");
            1
        }
    }
}

fn hive_binary() -> String {
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hive".to_string())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn rig_up(run: &str, orch: Option<&str>, workspace: Option<&str>) -> Result<()> {
    let error = crate::team::validate_team_name(run);
    if !error.is_empty() {
        bail!("{error}");
    }
    if crate::registry::load(run).is_some() {
        bail!("team '{run}' already exists (hive flow rig {run} --down releases it)");
    }
    if tmux::has_session(run) {
        bail!("tmux session '{run}' already exists (tmux kill-session -t {run})");
    }
    let workspace = match workspace {
        Some(dir) if !dir.is_empty() => crate::cli::expanduser(dir),
        _ => crate::team::hive_home()
            .join("workspaces")
            .join(run)
            .to_string_lossy()
            .into_owned(),
    };

    let dock = tmux::new_session(run, SESSION_COLS, SESSION_ROWS)
        .with_context(|| format!("creating tmux session '{run}'"))?;
    let window = tmux::get_pane_window_target(&dock)
        .filter(|w| !w.is_empty())
        .unwrap_or_else(|| format!("{run}:"));

    let team = match Team::create_for_window(
        run,
        &window,
        &dock,
        LEAD_AGENT_NAME,
        "workflow rig",
        "",
        &workspace,
        false,
    ) {
        Ok(team) => team,
        Err(e) => {
            tmux::kill_session(run);
            return Err(e);
        }
    };
    crate::registry::record_team(
        &team.name,
        &workspace,
        &team.created_at_key(),
        &[],
        &team.tmux_window_id,
    )
    .with_context(|| format!("recording team '{run}'"))?;
    crate::bus::init_workspace(Path::new(&workspace))
        .with_context(|| format!("initializing workspace {workspace}"))?;

    let hive = shell_quote(&hive_binary());
    // The board tags its own pane (@hive-role dock) and re-tiles the window
    // as it starts; tagging here too keeps the first spawn's layout right
    // even before the board's first tick.
    tmux::set_pane_option(&dock, "hive-role", "dock");
    tmux::set_pane_title(&dock, "⬡ flow board");
    tmux::respawn_pane(
        &dock,
        &format!("{hive} flow board --team {}", shell_quote(run)),
    )
    .context("starting the board")?;

    if let Some(session_id) = orch.filter(|s| !s.is_empty()) {
        let mirror = tmux::split_window(&dock, false, None, true, None)
            .context("splitting the orch mirror pane")?;
        tmux::set_pane_option(&mirror, "hive-role", "mirror");
        tmux::set_pane_title(&mirror, "⬡ orch 「mirror」");
        tmux::respawn_pane(&mirror, &format!("{hive} view {}", shell_quote(session_id)))
            .context("starting the orch mirror")?;
        let _ = crate::layout::apply_adaptive(&window);
    }

    println!("rig '{run}' up: session={run} team={run} workspace={workspace}");
    println!("attach: hive attach {run}");
    Ok(())
}

fn rig_down(run: &str) -> Result<()> {
    if crate::registry::load(run).is_none() && !tmux::has_session(run) {
        bail!("no rig named '{run}' (no team, no tmux session)");
    }
    if let Ok(mut team) = Team::load(run, "") {
        let names: Vec<String> = team.agents.iter().map(|a| a.name.clone()).collect();
        for name in names {
            if team.retire(&name) {
                println!("retired {name}");
            }
        }
    }
    if crate::registry::load(run).is_some() {
        crate::cli::core_cmds::delete(run, "", false, false);
    }
    if tmux::has_session(run) {
        tmux::kill_session(run);
        println!("session '{run}' killed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_quote_wraps_and_escapes_single_quotes() {
        assert_eq!(shell_quote("review-149"), "'review-149'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
