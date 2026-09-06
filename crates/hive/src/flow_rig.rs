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
    if tmux::has_session(&format!("={run}")) {
        bail!("tmux session '{run}' already exists (tmux kill-session -t ={run})");
    }
    // The team directory is the default, as for `hive create`; the rig
    // only initializes it (never resets), so a run's op journal under
    // `artifacts/flow/` survives a `--down` and re-rig for `--resume`.
    let workspace = match workspace {
        Some(dir) if !dir.is_empty() => crate::paths::expanduser(dir),
        _ => crate::registry::team_dir(run)
            .expect("validated above")
            .to_string_lossy()
            .into_owned(),
    };

    let (window, dock, _) = crate::team_display::new_team_session_window(run)
        .with_context(|| format!("creating tmux session '{run}'"))?;

    let team = match Team::create_for_window(
        run,
        &window,
        &dock,
        LEAD_AGENT_NAME,
        "workflow rig",
        &workspace,
        false,
    ) {
        Ok(team) => team,
        Err(e) => {
            tmux::kill_session(&format!("={run}"));
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

    let hive = shell_quote(&crate::paths::self_exe());
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
        // What makes the status bar's orch chip appear.
        tmux::set_window_option(&window, "@hive-mirror", "on");
        let _ = crate::layout::ensure(&window, false);
    }

    println!("rig '{run}' up: session={run} team={run} workspace={workspace}");
    println!("attach: hive attach {run}");
    Ok(())
}

pub(crate) fn rig_down(run: &str) -> Result<()> {
    // Exact-name targets throughout: once `hive delete` has closed the
    // team window (and with it the session), a bare `-t <run>` would
    // prefix-match a stranger's `<run>-x` session and kill that instead.
    let session = format!("={run}");
    let had_session = tmux::has_session(&session);
    if crate::registry::load(run).is_none() && !had_session {
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
        crate::team::delete_team(run, "", false)?;
    }
    if tmux::has_session(&session) {
        tmux::kill_session(&session);
    }
    if had_session {
        println!("session '{run}' killed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::testkit::{count, display_env_outside, fake_tmux_sessions, has_row, team_dir};

    #[test]
    fn test_shell_quote_wraps_and_escapes_single_quotes() {
        assert_eq!(shell_quote("review-149"), "'review-149'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn test_rig_down_without_a_rig_never_kills_a_prefix_matched_session() {
        let _env = display_env_outside();
        let argv = fake_tmux_sessions("", &[], &[], &["abc-keep"]);

        let err = crate::flow_rig::rig_down("abc").unwrap_err().to_string();

        assert!(err.contains("no rig named 'abc'"), "{err}");
        assert_eq!(count(&argv, "kill-session"), 0);
        assert_eq!(count(&argv, "kill-window"), 0);
    }

    #[test]
    fn test_rig_down_kills_the_team_window_and_the_exact_session() {
        let _env = display_env_outside();
        crate::registry::record_team("abc", "", "100.0", &[], "@7").unwrap();
        let argv = fake_tmux_sessions(
            "abc:1	@7	abc			
    ",
            &[],
            &[("abc:1", "hive-built", "1")],
            &["abc", "abc-keep"],
        );

        crate::flow_rig::rig_down("abc").unwrap();

        assert!(crate::registry::load("abc").is_none());
        assert!(has_row(&argv, &["kill-window", "-t", "@7"]));
        // Every session target is exact: `abc-keep` is never in reach.
        let session_targets: Vec<String> = argv
            .borrow()
            .iter()
            .filter(|a| matches!(a[0].as_str(), "has-session" | "kill-session"))
            .map(|a| a[2].clone())
            .collect();
        assert!(!session_targets.is_empty());
        assert!(
            session_targets.iter().all(|t| t == "=abc"),
            "{session_targets:?}"
        );
    }

    #[test]
    fn test_rig_up_defaults_the_workspace_to_the_team_dir_and_keeps_its_journal() {
        let env = display_env_outside();
        let ws = team_dir(&env, "abc");
        // a previous rig's journal, kept by `--down` for `--resume`
        std::fs::create_dir_all(ws.join("artifacts").join("flow")).unwrap();
        std::fs::write(ws.join("artifacts").join("flow").join("ops.jsonl"), "{}").unwrap();
        let _argv = fake_tmux_sessions("", &[], &[], &[]);

        assert_eq!(crate::flow_rig::rig_cmd("abc", None, None, false), 0);

        let entry = crate::registry::load("abc").expect("team recorded");
        assert_eq!(
            entry["workspace"],
            Value::from(ws.to_string_lossy().as_ref())
        );
        assert!(ws.join("team.json").is_file());
        assert!(ws.join("hive.db").is_file());
        assert!(ws
            .join("artifacts")
            .join("flow")
            .join("ops.jsonl")
            .is_file());
    }
}
