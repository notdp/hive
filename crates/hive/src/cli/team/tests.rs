use serde_json::{Map, Value};

use super::*;
use crate::testenv::EnvGuard;
use crate::testkit::{
    claude_session_me, count, display_env, display_env_outside, fake_tmux, fake_tmux_sessions,
    has_row, hived_answering_ping, member_row, team_dir, Argv,
};

#[test]
fn test_hive_skill_entry_is_each_clis_own_form() {
    assert_eq!(hive_skill_entry("claude"), "/hive:hive");
    assert_eq!(hive_skill_entry("codex"), "$hive");
    assert_eq!(hive_skill_entry("grok"), "/hive");
}

#[test]
fn test_team_workspace_is_the_team_dir_under_the_registry_store() {
    let mut env = EnvGuard::new();
    let tmp = tempfile::TempDir::new().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    assert_eq!(
        team_workspace("hornet"),
        tmp.path()
            .join(".hive")
            .join("teams")
            .join("hornet")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(
        std::path::Path::new(&team_workspace("hornet"))
            .join("team.json")
            .to_string_lossy()
            .as_ref(),
        crate::registry::entry_path("hornet")
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
}

#[test]
fn test_prepare_workspace_resets_the_team_dir_but_keeps_team_json() {
    let mut env = EnvGuard::new();
    let tmp = tempfile::TempDir::new().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    crate::registry::record_team("hornet", "", "1.0", &[], "").unwrap();
    let ws = team_workspace("hornet");
    let dir = std::path::Path::new(&ws);
    // a deleted predecessor's leftovers
    std::fs::create_dir_all(dir.join("artifacts")).unwrap();
    std::fs::write(dir.join("artifacts").join("old.md"), "x").unwrap();
    std::fs::write(dir.join("hive.db"), "stale").unwrap();

    prepare_workspace("hornet", &ws, false, &["k=v".to_string()]).unwrap();

    assert!(crate::registry::load("hornet").is_some());
    assert!(!dir.join("artifacts").join("old.md").exists());
    assert!(dir.join("hive.db").is_file());
    assert_ne!(std::fs::read(dir.join("hive.db")).unwrap(), b"stale");
    assert!(dir.join("run").is_dir());
    assert_eq!(
        std::fs::read_to_string(dir.join("state").join("k")).unwrap(),
        "v"
    );
}

#[test]
fn test_is_team_dir_reads_through_a_trailing_slash_and_a_tilde() {
    let mut env = EnvGuard::new();
    let tmp = tempfile::TempDir::new().unwrap();
    env.set("HOME", tmp.path());
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    let ws = team_workspace("hornet");
    assert!(is_team_dir("hornet", &ws));
    assert!(is_team_dir("hornet", &format!("{ws}/")));
    assert!(is_team_dir("hornet", &format!("{ws}/.")));
    assert!(is_team_dir("hornet", "~/.hive/teams/hornet/"));
    assert!(!is_team_dir("hornet", "~/.hive/teams/comb"));
    assert!(!is_team_dir("hornet", &format!("{ws}-2")));
}

#[test]
fn test_check_explicit_workspace_refuses_another_teams_dir_under_the_store() {
    let mut env = EnvGuard::new();
    let tmp = tempfile::TempDir::new().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    let store = tmp.path().join(".hive").join("teams");
    assert!(check_explicit_workspace("hornet", "").is_ok());
    assert!(check_explicit_workspace("hornet", &team_workspace("hornet")).is_ok());
    assert!(check_explicit_workspace("hornet", &format!("{}/", team_workspace("hornet"))).is_ok());
    assert!(
        check_explicit_workspace("hornet", tmp.path().join("elsewhere").to_str().unwrap()).is_ok()
    );
    for inside in [
        store.join("comb"),
        store.join("comb").join("artifacts"),
        store.clone(),
    ] {
        let error = check_explicit_workspace("hornet", inside.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains("registry store"), "{error}");
    }
}

#[test]
fn test_prepare_workspace_resets_the_team_dir_given_with_a_trailing_slash() {
    let mut env = EnvGuard::new();
    let tmp = tempfile::TempDir::new().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    crate::registry::record_team("hornet", "", "1.0", &[], "").unwrap();
    let ws = team_workspace("hornet");
    let dir = std::path::Path::new(&ws);
    std::fs::create_dir_all(dir.join("artifacts")).unwrap();
    std::fs::write(dir.join("artifacts").join("old.md"), "x").unwrap();

    prepare_workspace("hornet", &format!("{ws}/"), false, &[]).unwrap();

    assert!(crate::registry::load("hornet").is_some());
    assert!(!dir.join("artifacts").join("old.md").exists());
    assert!(dir.join("hive.db").is_file());
}

#[test]
fn test_prepare_workspace_keeps_an_explicit_dir_unless_reset() {
    let mut env = EnvGuard::new();
    let tmp = tempfile::TempDir::new().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(ws.join("artifacts")).unwrap();
    std::fs::write(ws.join("artifacts").join("keep.md"), "x").unwrap();
    let ws_str = ws.to_string_lossy().into_owned();

    prepare_workspace("hornet", &ws_str, false, &[]).unwrap();
    assert!(ws.join("artifacts").join("keep.md").is_file());
    assert!(ws.join("hive.db").is_file());

    prepare_workspace("hornet", &ws_str, true, &[]).unwrap();
    assert!(!ws.join("artifacts").join("keep.md").exists());
    assert!(ws.join("hive.db").is_file());
}

#[test]
fn test_reuse_existing_binding_refuses_another_name_for_a_paneless_session() {
    let mut bound = Map::new();
    bound.insert("team".to_string(), Value::String("honey".to_string()));
    bound.insert("agent".to_string(), Value::String("rex".to_string()));
    bound.insert("pane".to_string(), Value::String(String::new()));
    assert_eq!(reuse_existing_binding(&Map::new(), "wasp"), Ok(false));
    assert_eq!(reuse_existing_binding(&bound, ""), Ok(true));
    assert_eq!(reuse_existing_binding(&bound, "honey"), Ok(true));
    let refused = reuse_existing_binding(&bound, "wasp").unwrap_err();
    assert!(refused.contains("honey.rex") && refused.contains("wasp"));
    // a tagged pane is the team's display: idempotent whatever the name
    bound.insert("pane".to_string(), Value::String("%7".to_string()));
    assert_eq!(reuse_existing_binding(&bound, "wasp"), Ok(true));
}

#[test]
fn test_doctor_without_a_reachable_hived_reports_run_dir_and_logs() {
    let env = display_env();
    let ws = env._tmp.path().join("ws").to_string_lossy().into_owned();
    std::fs::create_dir_all(&ws).unwrap();
    let argv = fake_tmux("", &[]);
    let _hived = hived_answering_ping("honey");
    let mut t = Team {
        name: "honey".to_string(),
        workspace: ws.clone(),
        ..Default::default()
    };

    let (report, healthy) = doctor_report(&mut t, &ws, "orch");

    assert!(!healthy);
    let workspace = std::path::Path::new(&ws);
    assert_eq!(report["workspace"], Value::from(ws.as_str()));
    assert_eq!(
        report["runDir"],
        Value::from(
            crate::devlog::run_dir(workspace)
                .to_string_lossy()
                .into_owned()
        )
    );
    assert_eq!(
        report["logs"],
        Value::Object(crate::devlog::log_paths(workspace))
    );
    assert_eq!(report["hived"]["ok"], Value::Bool(false));
    assert_eq!(
        report["hived"]["error"],
        Value::from(crate::devlog::hived_unavailable_message(workspace))
    );
    assert!(report.get("duplicateTeams").is_none());
    // The hook answered the ping, so no hived was started, and the socket
    // the doctor request then looked for is still absent.
    assert!(!crate::hived::socket_path(&ws).exists());
    // Read-only on tmux: window identity and duplicate-binding lookups.
    assert!(argv
        .borrow()
        .iter()
        .all(|a| matches!(a[0].as_str(), "display-message" | "list-windows")));
}

#[test]
fn test_create_outside_tmux_builds_the_team_session_and_records_the_display() {
    let env = display_env_outside();
    let argv = fake_tmux_sessions("", &[], &[], &[]);

    create_detached_team("honey", "", "", false, &["k=v".to_string()]);

    assert!(has_row(
        &argv,
        &[
            "new-session",
            "-d",
            "-s",
            "honey",
            "-x",
            "220",
            "-y",
            "60",
            "-P",
            "-F",
            "#{pane_id}",
        ]
    ));
    assert!(has_row(&argv, &["rename-window", "-t", "honey:1", "honey"]));
    assert_eq!(count(&argv, "new-window"), 0);
    let entry = crate::registry::load("honey").expect("team recorded");
    assert_eq!(entry["display"], Value::from("@7"));
    // The default workspace is the team's own directory under the registry
    // store, beside its team.json — no /tmp slug, no session name in it.
    let ws = team_dir(&env, "honey");
    assert_eq!(
        entry["workspace"],
        Value::from(ws.to_string_lossy().as_ref())
    );
    assert!(ws.join("team.json").is_file());
    assert_eq!(entry["members"], Value::Array(Vec::new()));
    // The first pane is the team's dock, tagged as a shell-pane create
    // tags its pane, so a verb run from it finds the team through its own
    // tags (the window's `@hive-team` is display, not binding).
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-team", "honey"]
    ));
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-role", "terminal"]
    ));
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-agent", "orch"]
    ));
    // usable, not just recorded: the bus a dispatch rides exists
    assert!(ws.join("hive.db").is_file(), "{}", ws.display());
    assert!(ws.join("artifacts").is_dir(), "{}", ws.display());
    assert!(ws.join("run").is_dir(), "{}", ws.display());
    // --state lands on the default workspace too, not only an explicit one
    assert_eq!(
        std::fs::read_to_string(ws.join("state").join("k")).unwrap(),
        "v"
    );
}

#[test]
fn test_create_outside_tmux_resets_a_recycled_names_leftover_workspace() {
    let env = display_env_outside();
    // `hive delete honey` kept the predecessor's workspace files
    let ws = team_dir(&env, "honey");
    std::fs::create_dir_all(ws.join("artifacts")).unwrap();
    std::fs::write(ws.join("artifacts").join("old.md"), "stale").unwrap();
    std::fs::write(ws.join("hive.db"), "stale").unwrap();
    let _argv = fake_tmux_sessions("", &[], &[], &[]);

    create_detached_team("honey", "", "", false, &[]);

    assert!(crate::registry::load("honey").is_some());
    assert!(!ws.join("artifacts").join("old.md").exists());
    assert_ne!(std::fs::read(ws.join("hive.db")).unwrap(), b"stale");
}

#[test]
fn test_create_outside_tmux_honours_an_explicit_workspace_beside_the_team_dir() {
    let env = display_env_outside();
    let external = env._tmp.path().join("elsewhere");
    std::fs::create_dir_all(external.join("artifacts")).unwrap();
    std::fs::write(external.join("artifacts").join("keep.md"), "x").unwrap();
    let _argv = fake_tmux_sessions("", &[], &[], &[]);

    create_detached_team("honey", "", external.to_str().unwrap(), false, &[]);

    let entry = crate::registry::load("honey").expect("team recorded");
    assert_eq!(
        entry["workspace"],
        Value::from(external.to_string_lossy().as_ref())
    );
    // the entry still lives in the team dir; the workspace elsewhere is
    // initialized, not wiped
    assert!(team_dir(&env, "honey").join("team.json").is_file());
    assert!(!team_dir(&env, "honey").join("hive.db").exists());
    assert!(external.join("hive.db").is_file());
    assert!(external.join("artifacts").join("keep.md").is_file());
}

#[test]
fn test_create_inside_tmux_from_a_shell_pane_defaults_to_the_team_dir() {
    let env = display_env();
    let _argv = fake_tmux_sessions("", &[], &[], &["dev"]);

    create("honey", "", "", false, &["k=v".to_string()]);

    let entry = crate::registry::load("honey").expect("team recorded");
    // the team dir, not /tmp/hive-<session>-w<id>
    let ws = team_dir(&env, "honey");
    assert_eq!(
        entry["workspace"],
        Value::from(ws.to_string_lossy().as_ref())
    );
    assert!(ws.join("team.json").is_file());
    assert!(ws.join("hive.db").is_file());
    assert_eq!(
        std::fs::read_to_string(ws.join("state").join("k")).unwrap(),
        "v"
    );
}

#[test]
fn test_create_outside_tmux_seats_a_claude_session_creator_as_orch_on_a_mirror_pane() {
    let mut env = display_env_outside();
    env.env.set("HIVE_BIN", "/x/hive");
    let _claude = claude_session_me(&mut env);
    let _hived = hived_answering_ping("honey");
    let argv = fake_tmux_sessions("", &[], &[], &[]);

    create_detached_team("honey", "", "", false, &[]);

    let entry = crate::registry::load("honey").expect("team recorded");
    let mut orch = Map::new();
    orch.insert("name".to_string(), Value::from("orch"));
    orch.insert("cli".to_string(), Value::from("claude"));
    orch.insert("model".to_string(), Value::from(""));
    orch.insert("sessionId".to_string(), Value::from("s-me"));
    orch.insert("cwd".to_string(), Value::from(getcwd()));
    assert_eq!(entry["members"], Value::Array(vec![Value::Object(orch)]));
    // The first pane is the creator's read-only mirror: tagged as orch,
    // running `hive view` on the session — never a resume, which would mint
    // a forked job. No second pane.
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-agent", "orch"]
    ));
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-role", "mirror"]
    ));
    assert!(argv.borrow().iter().any(|a| a[0] == "send-keys"
        && a.contains(&"-l".to_string())
        && a.iter().any(|arg| arg.contains("hive view s-me"))));
    assert_eq!(count(&argv, "split-window"), 0);
    // The mirror on screen is what makes the orch chip appear.
    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "honey:1", "@hive-mirror", "on"]
    ));
    assert_status_bar_installed(&argv);
}

/// The team session's status bar, by session id, then the two bindings
/// naming this binary (`HIVE_BIN`).
fn assert_status_bar_installed(argv: &Argv) {
    for row in crate::tmux::team_status_argv("$1", crate::view_theme::active_theme_kind()) {
        let row: Vec<&str> = row.iter().map(String::as_str).collect();
        assert!(has_row(argv, &row), "{row:?}");
    }
    // The double answers `list-keys` with nothing: no prefix+m fallback.
    for row in [
        crate::tmux::status_click_binding("/x/hive"),
        crate::tmux::mirror_key_binding("/x/hive", ""),
    ] {
        let row: Vec<&str> = row.iter().map(String::as_str).collect();
        assert!(has_row(argv, &row), "{row:?}");
    }
}

#[test]
fn test_create_outside_tmux_without_a_session_installs_the_bar_but_no_mirror_chip() {
    let mut env = display_env_outside();
    env.env.set("HIVE_BIN", "/x/hive");
    let argv = fake_tmux_sessions("", &[], &[], &[]);

    create_detached_team("honey", "", "", false, &[]);

    assert_status_bar_installed(&argv);
    assert!(argv
        .borrow()
        .iter()
        .all(|a| !(a[0] == "set-window-option" && a[3] == "@hive-mirror")));
}

fn joined_session_row(team: &str) -> Map<String, Value> {
    crate::registry::load(team).unwrap()["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["sessionId"] == "s-me")
        .and_then(Value::as_object)
        .cloned()
        .expect("the joined session is on the roster")
}

#[test]
fn test_join_outside_tmux_adds_the_sessions_mirror_pane_to_the_team_window() {
    let mut env = display_env_outside();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "grok", "sid-orch")],
        "@7",
    )
    .unwrap();
    let argv = fake_tmux_sessions(
        "honey:1	@7	honey			
",
        &["%1	[orch]	grok	agent	orch	honey	grok	"],
        &[],
        &["honey"],
    );

    join_as_ccd("honey", "");

    let joined = joined_session_row("honey");
    assert_eq!(joined["cli"], Value::from("claude"));
    assert_ne!(joined["name"], Value::from("orch"));
    // One pane split into the existing window, running the session's
    // read-only mirror — never a resume, which would fork a bg job — and
    // tagged as the window's mirror.
    assert_eq!(count(&argv, "new-window"), 0);
    assert_eq!(count(&argv, "split-window"), 1);
    assert!(argv.borrow().iter().any(|a| a[0] == "send-keys"
        && a.contains(&"-l".to_string())
        && a.iter().any(|arg| arg.contains("hive view s-me"))));
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%2", "@hive-role", "mirror"]
    ));
    assert!(argv
        .borrow()
        .iter()
        .any(|a| a[0] == "set-window-option" && a[3] == "@hive-mirror" && a[4] == "on"));
}

#[test]
fn test_join_outside_tmux_rebuilds_a_missing_team_window_first() {
    let mut env = display_env_outside();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "grok", "sid-orch")],
        "",
    )
    .unwrap();
    let argv = fake_tmux_sessions("", &[], &[], &[]);

    join_as_ccd("honey", "");

    joined_session_row("honey");
    assert!(has_row(
        &argv,
        &[
            "new-session",
            "-d",
            "-s",
            "honey",
            "-x",
            "220",
            "-y",
            "60",
            "-P",
            "-F",
            "#{pane_id}",
        ]
    ));
    // orch rides the first pane; the joined session gets the split.
    assert_eq!(count(&argv, "split-window"), 1);
    assert!(argv
        .borrow()
        .iter()
        .any(|a| a[0] == "send-keys" && a.iter().any(|arg| arg.contains("hive view s-me"))));
}
