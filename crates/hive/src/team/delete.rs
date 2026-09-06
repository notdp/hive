//! `hive delete`'s body: the registry entry goes (that is what makes the
//! team deleted), the display hive built closes, the hived stops, the
//! workspace goes only on request, and the team's grok leaders are swept.
//! `--down` is the whole-run teardown around it: every member retired
//! first, the team's own tmux session killed last.

use std::path::Path;

use anyhow::{bail, Result};

use super::Team;
use crate::identity;
use crate::paths::expanduser;
use crate::tmux;

/// Grok leader keys serving *team*, as the leader directory has them.
fn team_grok_daemon_keys(team: &str) -> Vec<String> {
    let mut keys: Vec<String> = crate::adapters::grok_leader::list_daemon_keys()
        .into_iter()
        .filter(|key| {
            crate::adapters::grok_leader::member_from_key(key)
                .map(|(key_team, _)| key_team == team)
                .unwrap_or(false)
        })
        .collect();
    keys.sort();
    keys
}

/// Stop every grok leader that served *team* and clear its key files.
fn sweep_team_grok_daemons(team: &str) {
    for key in team_grok_daemon_keys(team) {
        crate::adapters::grok_leader::pool().drop_key(&key);
        crate::adapters::grok_leader::kill_daemon_key(&key);
    }
}

/// The delete body; refuses an unsafe name before touching anything, since
/// the team directory is joined onto the registry store from it.
///
/// Without `--delete-workspace` only `team.json` goes: the team directory
/// keeps its bus, run dir and artifacts for reading until the name is
/// recycled (the next create resets them). With it, the workspace — the
/// team directory, or the external one the entry records — is removed,
/// and the team directory with it. An external workspace is never removed
/// without the flag.
///
/// With `down`, every member is retired before the entry goes and the
/// team's tmux session (the one `hive create` built outside tmux, named
/// after the team) is killed after it — the teardown of a workflow run.
/// Every session target is exact (`=<name>`): once the team window has
/// closed, and with it the session, a bare `-t <name>` would prefix-match
/// a stranger's `<name>-x` session and kill that instead.
pub(crate) fn delete_team(
    name: &str,
    workspace: &str,
    delete_workspace: bool,
    down: bool,
) -> Result<()> {
    let error = crate::team::validate_team_name(name);
    if !error.is_empty() {
        bail!("cannot delete: {error}");
    }
    let session = format!("={name}");
    let had_session = down && tmux::has_session(&session);
    // The caller may be a member of this very team (an orch running the
    // teardown). Its engine is this process's host: kill it here and the
    // rest of the delete dies with it. It is held aside and stopped after
    // everything else, the session included.
    let me = crate::identity::default_agent();
    let mut caller = None;
    if down {
        if crate::registry::load(name).is_none() && !had_session {
            bail!("no team named '{name}' (no registry entry, no tmux session)");
        }
        if let Ok(mut team) = Team::load(name, "") {
            caller = team
                .agents
                .iter()
                .find(|a| Some(&a.name) == me.as_ref())
                .cloned();
            let names: Vec<String> = team
                .agents
                .iter()
                .filter(|a| Some(&a.name) != me.as_ref())
                .map(|a| a.name.clone())
                .collect();
            for member in names {
                if team.retire(&member) {
                    println!("retired {member}");
                }
            }
        }
    }
    let mut team_workspace = String::new();
    let mut team_window = String::new();
    let mut team_window_id = String::new();
    if let Ok(t) = Team::load(name, "") {
        team_workspace = t.workspace.clone();
        team_window = t.tmux_window.clone();
        team_window_id = t.tmux_window_id.clone();
        t.cleanup(me.as_deref());
    }

    // Read before the tags go: a window hive built itself (`@hive-built`,
    // in the team session or the caller's) is hive's to close; a window
    // the human's session lent the team (in-tmux create) keeps their pane.
    // The last window going drops the session with it.
    let hive_built =
        !team_window.is_empty() && tmux::get_window_option(&team_window, "hive-built").is_some();
    if !team_window.is_empty() {
        crate::team::clear_window_tags(&team_window);
    }
    // A parked mirror pane (`hive mirror off`) sits in a hidden window hive
    // made; it goes first, or it would keep the team session alive after
    // the team window closes.
    for hidden in tmux::hidden_mirror_windows(name) {
        tmux::kill_window(&hidden);
    }
    let caller_window = identity::current_window_id().unwrap_or_default();
    if hive_built && !team_window_id.is_empty() && caller_window != team_window_id {
        tmux::kill_window(&team_window_id);
    }

    // Explicit -w, else the entry's workspace; with neither there is no
    // workspace to stop or remove, and the team-dir sweep below still
    // clears a leftover directory.
    let resolved_workspace = if !workspace.is_empty() {
        workspace.to_string()
    } else {
        team_workspace
    };

    // Stop hived before workspace cleanup.
    if !resolved_workspace.is_empty() {
        crate::hived::stop_hived(&resolved_workspace);
    }

    if !resolved_workspace.is_empty() && delete_workspace {
        let ws = expanduser(&resolved_workspace);
        if Path::new(&ws).exists() {
            std::fs::remove_dir_all(&ws)?;
            println!("Workspace removed: {ws}");
        }
    }

    let current = crate::context::load_current_context();
    if current.get("team").map(String::as_str) == Some(name) {
        let _ = crate::context::clear_current_context();
    }

    // The registry entry is the team's authoritative existence: removing it
    // is what makes the team deleted (readers and the hived's registry-gone
    // exit key on it).
    crate::registry::delete_team(name)?;
    if delete_workspace {
        // The team directory is the default workspace; with an external
        // one it held only the entry, gone above with its directory.
        if let Some(dir) = crate::registry::team_dir(name) {
            if dir.exists() {
                std::fs::remove_dir_all(&dir)?;
            }
        }
    }

    // Last, because it is the point of no return for the engines: the hived
    // reaps orphan leaders only for its own team, and a deleted team has no
    // hived — an unswept leader would outlive every trace of who it served.
    sweep_team_grok_daemons(name);

    println!("Team '{name}' deleted.");
    if down && tmux::has_session(&session) {
        tmux::kill_session(&session);
    }
    if had_session {
        println!("session '{name}' killed");
    }
    // The caller last: its roster row went with the entry above, so only
    // its engine is left to stop — and a pane inside the session is gone
    // already.
    if let Some(agent) = caller {
        agent.kill();
        println!("retired {}", agent.name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;
    use crate::testkit::{
        args, count, display_env, display_env_outside, fake_tmux_sessions, has_row, member_row,
        team_dir, DisplayEnv,
    };

    #[test]
    fn test_delete_refuses_unsafe_names_before_touching_disk() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::TempDir::new().unwrap();
        let hive_home = tmp.path().join("hive");
        env.set("HIVE_HOME", &hive_home);
        // what `teams/../evil` and an absolute name would have resolved to
        let sibling = hive_home.join("evil");
        let outside = tmp.path().join("outside");
        for dir in [&sibling, &outside] {
            std::fs::create_dir_all(dir.join("marker")).unwrap();
        }

        for name in ["../evil", outside.to_str().unwrap(), "a.b", ""] {
            let err = delete_team(name, "", true, false).unwrap_err().to_string();
            assert!(err.starts_with("cannot delete:"), "{name}: {err}");
        }

        assert!(sibling.join("marker").is_dir());
        assert!(outside.join("marker").is_dir());
    }

    #[test]
    fn test_team_grok_daemon_keys_selects_only_this_teams_members() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::TempDir::new().unwrap();
        env.set("GROK_HOME", tmp.path());
        let hive = tmp.path().join("hive");
        std::fs::create_dir_all(&hive).unwrap();
        for name in [
            "m-hornet.ant.sock",
            "m-hornet.bee.sock",
            "m-comb.ant.sock",
            "p19.sock",
        ] {
            std::fs::write(hive.join(name), "").unwrap();
        }

        assert_eq!(
            team_grok_daemon_keys("hornet"),
            vec!["m-hornet.ant".to_string(), "m-hornet.bee".to_string()]
        );
    }

    #[test]
    fn test_sweep_team_grok_daemons_clears_only_this_teams_keys() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::TempDir::new().unwrap();
        env.set("GROK_HOME", tmp.path());
        let hive = tmp.path().join("hive");
        std::fs::create_dir_all(&hive).unwrap();
        for name in [
            "m-hornet.ant.sock",
            "m-hornet.ant.pid",
            "m-hornet.ant.lock",
            "m-hornet.ant.session",
            "m-comb.ant.sock",
            "p19.sock",
        ] {
            std::fs::write(hive.join(name), "").unwrap();
        }

        sweep_team_grok_daemons("hornet");

        for gone in [
            "m-hornet.ant.sock",
            "m-hornet.ant.pid",
            "m-hornet.ant.lock",
            "m-hornet.ant.session",
        ] {
            assert!(!hive.join(gone).exists(), "{gone} survived the sweep");
        }
        assert!(hive.join("m-comb.ant.sock").exists());
        assert!(hive.join("p19.sock").exists());
    }

    fn team_on_a_built_window() {
        crate::registry::record_team(
            "honey",
            "",
            "100.0",
            &[member_row("orch", "claude", "")],
            "@7",
        )
        .unwrap();
    }

    #[test]
    fn test_delete_from_inside_the_team_window_leaves_it_to_the_caller() {
        let env = display_env();
        let ws = env._tmp.path().join("ws").to_string_lossy().into_owned();
        team_on_a_built_window();
        // The caller's own window is the team window (`display-message
        // #{window_id}` answers `@7` for the caller's pane too).
        let argv = fake_tmux_sessions(
            "honey:1	@7	honey			
    ",
            &[],
            &[("honey:1", "hive-built", "1")],
            &["dev", "honey"],
        );

        crate::team::delete_team("honey", &ws, false, false).unwrap();

        assert!(crate::registry::load("honey").is_none());
        assert_eq!(count(&argv, "kill-window"), 0);
    }

    #[test]
    fn test_delete_from_outside_closes_the_window_hive_built() {
        let env = display_env_outside();
        let ws = env._tmp.path().join("ws").to_string_lossy().into_owned();
        team_on_a_built_window();
        let argv = fake_tmux_sessions(
            "honey:1	@7	honey			
    ",
            &[],
            &[("honey:1", "hive-built", "1")],
            &["honey"],
        );

        crate::team::delete_team("honey", &ws, false, false).unwrap();

        assert!(crate::registry::load("honey").is_none());
        assert!(has_row(&argv, &["kill-window", "-t", "@7"]));
    }

    #[test]
    fn test_delete_leaves_a_window_the_callers_session_lent() {
        let env = display_env_outside();
        let ws = env._tmp.path().join("ws").to_string_lossy().into_owned();
        team_on_a_built_window();
        // An in-tmux create bound the human's own window: no `@hive-built`.
        let argv = fake_tmux_sessions(
            "dev:2	@7	honey			
    ",
            &[],
            &[],
            &["dev"],
        );

        crate::team::delete_team("honey", &ws, false, false).unwrap();

        assert!(crate::registry::load("honey").is_none());
        assert_eq!(count(&argv, "kill-window"), 0);
    }

    #[test]
    fn test_delete_kills_the_hidden_mirror_window_before_the_team_window() {
        let env = display_env_outside();
        let ws = env._tmp.path().join("ws").to_string_lossy().into_owned();
        team_on_a_built_window();
        // The orch mirror is parked (`hive mirror off`): its hidden window
        // would keep the team session alive after the team window closes.
        let argv = fake_tmux_sessions(
            "honey:1	@7	honey			
    ",
            &[],
            &[
                ("honey:1", "hive-built", "1"),
                ("%5", "hive-hidden", "honey"),
            ],
            &["honey"],
        );

        crate::team::delete_team("honey", &ws, false, false).unwrap();

        let kills: Vec<Vec<String>> = argv
            .borrow()
            .iter()
            .filter(|a| a[0] == "kill-window")
            .cloned()
            .collect();
        assert_eq!(
            kills,
            vec![
                args(&["kill-window", "-t", "@9"]),
                args(&["kill-window", "-t", "@7"]),
            ]
        );
    }

    fn team_on_its_own_dir(env: &DisplayEnv) -> std::path::PathBuf {
        let ws = team_dir(env, "honey");
        crate::registry::record_team(
            "honey",
            ws.to_str().unwrap(),
            "100.0",
            &[member_row("orch", "claude", "")],
            "@7",
        )
        .unwrap();
        std::fs::create_dir_all(ws.join("run")).unwrap();
        std::fs::create_dir_all(ws.join("artifacts")).unwrap();
        std::fs::write(ws.join("hive.db"), "bus").unwrap();
        ws
    }

    #[test]
    fn test_delete_without_the_flag_removes_only_team_json_from_the_team_dir() {
        let env = display_env_outside();
        let ws = team_on_its_own_dir(&env);
        let _argv = fake_tmux_sessions("", &[], &[], &[]);

        crate::team::delete_team("honey", "", false, false).unwrap();

        assert!(crate::registry::load("honey").is_none());
        assert!(!ws.join("team.json").exists());
        assert!(ws.join("hive.db").is_file());
        assert!(ws.join("run").is_dir());
        assert!(ws.join("artifacts").is_dir());
    }

    #[test]
    fn test_delete_with_the_flag_removes_the_whole_team_dir() {
        let env = display_env_outside();
        let ws = team_on_its_own_dir(&env);
        let _argv = fake_tmux_sessions("", &[], &[], &[]);

        crate::team::delete_team("honey", "", true, false).unwrap();

        assert!(crate::registry::load("honey").is_none());
        assert!(!ws.exists(), "{}", ws.display());
        // the store itself stays
        assert!(env._tmp.path().join(".hive").join("teams").is_dir());
    }

    #[test]
    fn test_delete_without_an_entry_ignores_a_workspace_named_by_the_environment() {
        let mut env = display_env_outside();
        let stranger = env._tmp.path().join("stranger");
        std::fs::create_dir_all(stranger.join("artifacts")).unwrap();
        env.env.set("HIVE_WORKSPACE", &stranger);
        env.env.set("CR_WORKSPACE", &stranger);
        let _argv = fake_tmux_sessions("", &[], &[], &[]);

        crate::team::delete_team("honey", "", true, false).unwrap();

        assert!(stranger.join("artifacts").is_dir());
        assert!(!team_dir(&env, "honey").exists());
    }

    #[test]
    fn test_delete_never_removes_an_external_workspace_without_the_flag() {
        let env = display_env_outside();
        let external = env._tmp.path().join("elsewhere");
        std::fs::create_dir_all(external.join("artifacts")).unwrap();
        std::fs::write(external.join("hive.db"), "bus").unwrap();
        crate::registry::record_team(
            "honey",
            external.to_str().unwrap(),
            "100.0",
            &[member_row("orch", "claude", "")],
            "@7",
        )
        .unwrap();
        let _argv = fake_tmux_sessions("", &[], &[], &[]);

        crate::team::delete_team("honey", "", false, false).unwrap();

        assert!(crate::registry::load("honey").is_none());
        assert!(external.join("hive.db").is_file());
        assert!(external.join("artifacts").is_dir());
        // the team dir held only the entry, so it is gone
        assert!(!team_dir(&env, "honey").exists());

        // with the flag, the external workspace goes too
        crate::registry::record_team("honey", external.to_str().unwrap(), "200.0", &[], "@7")
            .unwrap();
        crate::team::delete_team("honey", "", true, false).unwrap();
        assert!(!external.exists());
        assert!(!team_dir(&env, "honey").exists());
    }

    #[test]
    fn test_delete_down_without_a_team_never_kills_a_prefix_matched_session() {
        let _env = display_env_outside();
        let argv = fake_tmux_sessions("", &[], &[], &["abc-keep"]);

        let err = delete_team("abc", "", false, true).unwrap_err().to_string();

        assert!(err.contains("no team named 'abc'"), "{err}");
        assert_eq!(count(&argv, "kill-session"), 0);
        assert_eq!(count(&argv, "kill-window"), 0);
    }

    #[test]
    fn test_delete_down_stops_the_calling_member_after_the_session() {
        // The caller is orch, a codex member of the team being torn down:
        // its tool thread id is the identity ladder's session rung.
        let mut env = display_env_outside();
        env.env.set("CODEX_THREAD_ID", "t-orch");
        crate::registry::record_team(
            "abc",
            "",
            "100.0",
            &[
                member_row("orch", "codex", "t-orch"),
                member_row("bee", "claude", ""),
            ],
            "@7",
        )
        .unwrap();
        let argv = fake_tmux_sessions(
            "abc:1\t@7\tabc\t\t\t\n",
            &[],
            &[("abc:1", "hive-built", "1")],
            &["abc"],
        );
        // The team window's panes, one per member, so each kill is a
        // recorded `kill-pane`.
        crate::team::set_fake_tmux_panes(|_| {
            [("%5", "orch", "codex"), ("%6", "bee", "claude")]
                .into_iter()
                .map(|(pane, agent, cli)| crate::tmux::PaneInfo {
                    pane_id: pane.to_string(),
                    role: "agent".to_string(),
                    agent: agent.to_string(),
                    team: "abc".to_string(),
                    cli: cli.to_string(),
                    ..Default::default()
                })
                .collect()
        });

        delete_team("abc", "", false, true).unwrap();

        assert!(crate::registry::load("abc").is_none());
        let at = |row: &[&str]| {
            argv.borrow()
                .iter()
                .position(|a| a.iter().map(String::as_str).eq(row.iter().copied()))
                .unwrap_or_else(|| panic!("{row:?} never ran: {:?}", argv.borrow()))
        };
        let bee = at(&["kill-pane", "-t", "%6"]);
        let session = at(&["kill-session", "-t", "=abc"]);
        let orch = at(&["kill-pane", "-t", "%5"]);
        assert!(bee < session, "the other member goes before the session");
        assert!(session < orch, "the caller goes after the session");
    }

    #[test]
    fn test_delete_down_retires_the_members_and_kills_the_exact_session() {
        let _env = display_env_outside();
        crate::registry::record_team(
            "abc",
            "",
            "100.0",
            &[
                member_row("orch", "claude", ""),
                member_row("bee", "claude", ""),
            ],
            "@7",
        )
        .unwrap();
        let argv = fake_tmux_sessions(
            "abc:1	@7	abc			
    ",
            &[],
            &[("abc:1", "hive-built", "1")],
            &["abc", "abc-keep"],
        );

        delete_team("abc", "", false, true).unwrap();

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
        assert_eq!(count(&argv, "kill-session"), 1);
    }

    #[test]
    fn test_delete_without_down_leaves_the_session_alone() {
        let env = display_env_outside();
        let ws = env._tmp.path().join("ws").to_string_lossy().into_owned();
        team_on_a_built_window();
        let argv = fake_tmux_sessions(
            "honey:1	@7	honey			
    ",
            &[],
            &[("honey:1", "hive-built", "1")],
            &["honey"],
        );

        delete_team("honey", &ws, false, false).unwrap();

        assert_eq!(count(&argv, "kill-session"), 0);
        assert_eq!(count(&argv, "has-session"), 0);
    }
}
