use serde_json::Value;

use super::*;
use crate::testkit::{
    claude_session_me, count, display_env, display_env_outside, fake_tmux, fake_tmux_sessions,
    fake_tmux_tagged, has_row, member_row, team_dir, Argv, DisplayEnv,
};

#[test]
fn test_attach_with_a_window_switches_the_client() {
    let _env = display_env();
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "grok", "sid-orch")],
        "@7",
    )
    .unwrap();
    let argv = fake_tmux(
        "dev:2\t@7\thoney\t\t\t\n",
        &["%1\t[orch]\tgrok\tagent\torch\thoney\tgrok\t"],
    );

    attach_cmd("honey");

    let recorded = argv.borrow();
    // switch-client moves *this* client; select-window would only retarget
    // the window's own session and leave the caller where it was.
    assert!(recorded
        .iter()
        .any(|a| a[..] == ["switch-client", "-t", "dev:2"]));
    assert!(recorded.iter().all(|a| a[0] != "select-window"));
    // Every member has its pane: nothing to build.
    assert!(recorded
        .iter()
        .all(|a| !matches!(a[0].as_str(), "new-window" | "split-window" | "send-keys")));
}

#[test]
fn test_attach_without_a_window_rebuilds_it_and_records_the_display() {
    let _env = display_env();
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[
            member_row("orch", "grok", "sid-orch"),
            member_row("sage", "grok", "sid-sage"),
            member_row("ghost", "grok", ""),
        ],
        "",
    )
    .unwrap();
    let argv = fake_tmux("", &[]);

    attach_cmd("honey");

    // Rebuild in the team's session even when the caller is inside tmux.
    assert_eq!(count(&argv, "new-session"), 1);
    assert_eq!(count(&argv, "new-window"), 0);
    assert_eq!(count(&argv, "split-window"), 1);
    assert!(has_row(&argv, &["switch-client", "-t", "honey:1"]));
    assert_eq!(
        crate::registry::load("honey").unwrap()["display"],
        Value::from("@7")
    );
    assert_eq!(count(&argv, "bind-key"), 2);
    assert!(has_row(&argv, &["set-option", "-t", "$1", "status", "2"]));
}

#[test]
fn test_attach_with_a_window_adds_a_pane_for_a_member_spawned_after_it() {
    let _env = display_env();
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[
            member_row("orch", "grok", "sid-orch"),
            member_row("sage", "grok", "sid-sage"),
        ],
        "@7",
    )
    .unwrap();
    // The window shows `orch` only — `sage` was spawned after it was built.
    let argv = fake_tmux(
        "dev:2\t@7\thoney\t\t\t\n",
        &["%1\t[orch]\tgrok\tagent\torch\thoney\tgrok\t"],
    );

    attach_cmd("honey");

    assert_eq!(count(&argv, "new-window"), 0);
    assert_eq!(count(&argv, "split-window"), 1);
    // The new pane runs sage's own viewer, not orch's.
    assert!(argv
        .borrow()
        .iter()
        .any(|a| a[0] == "send-keys" && a.last().is_some_and(|text| text.contains("sid-sage"))));
    assert!(has_row(&argv, &["switch-client", "-t", "dev:2"]));
}

#[test]
fn test_attach_heal_respects_hive_mirror_off() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "claude", "s-me")],
        "@7",
    )
    .unwrap();
    let argv = fake_tmux_tagged(
        "dev:2\t@7\thoney\t\t\t\n",
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[("dev:2", "hive-mirror", "off")],
    );

    attach_cmd("honey");

    assert_eq!(count(&argv, "split-window"), 0);
    assert_eq!(count(&argv, "send-keys"), 0);
    assert!(has_row(&argv, &["switch-client", "-t", "dev:2"]));
}

#[test]
fn test_attach_heal_keeps_the_mirror_the_window_already_shows() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "claude", "s-me")],
        "@7",
    )
    .unwrap();
    let argv = fake_tmux(
        "dev:2\t@7\thoney\t\t\t\n",
        &[
            "%0\t\tzsh\tterminal\t\thoney\t\t",
            "%1\t[orch]\thive\tmirror\torch\thoney\tclaude\t",
        ],
    );

    attach_cmd("honey");

    // The mirror counts as the member's pane: no second one, nothing moved.
    assert_eq!(count(&argv, "split-window"), 0);
    assert_eq!(count(&argv, "send-keys"), 0);
    assert_eq!(count(&argv, "kill-pane"), 0);
    assert_eq!(count(&argv, "break-pane"), 0);
    assert!(argv.borrow().iter().all(|a| a[0] != "set-window-option"));
    assert!(has_row(&argv, &["switch-client", "-t", "dev:2"]));
}

#[test]
fn test_attach_heal_joins_the_hidden_mirror_instead_of_splitting() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "claude", "s-me")],
        "@7",
    )
    .unwrap();
    // The window records `on`; the orch's mirror is parked from an earlier
    // `hive mirror off` on a window since killed by hand.
    let argv = fake_tmux_tagged(
        MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[
            ("dev:1", "hive-mirror", "on"),
            ("%1", "hive-hidden", "honey"),
            ("%1", "hive-role", "mirror"),
            ("%1", "hive-agent", "orch"),
        ],
    );

    attach_cmd("honey");

    // The parked pane comes back — its viewer intact, never a second one —
    // without the notify mark a fire while parked left on it.
    assert!(has_row(
        &argv,
        &["join-pane", "-h", "-b", "-d", "-s", "%1", "-t", "%0"]
    ));
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "-u", "@hive-notify-active"]
    ));
    assert_eq!(count(&argv, "split-window"), 0);
    assert!(argv
        .borrow()
        .iter()
        .all(|a| !(a[0] == "send-keys" && a.iter().any(|arg| arg.contains("hive view")))));
    assert_eq!(count(&argv, "select-layout"), 1);
}

#[test]
fn test_attach_heal_leaves_the_parked_mirror_and_records_off_when_nothing_is_recorded() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "claude", "s-me")],
        "@7",
    )
    .unwrap();
    // Nothing recorded on this window: the mirror stays collapsed, parked
    // pane and all, and the window records the default so the chip can
    // open it.
    let argv = fake_tmux_tagged(
        MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[
            ("%1", "hive-hidden", "honey"),
            ("%1", "hive-role", "mirror"),
            ("%1", "hive-agent", "orch"),
        ],
    );

    attach_cmd("honey");

    assert_eq!(count(&argv, "join-pane"), 0);
    assert_eq!(count(&argv, "split-window"), 0);
    assert!(argv
        .borrow()
        .iter()
        .all(|a| !(a[0] == "send-keys" && a.iter().any(|arg| arg.contains("hive view")))));
    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "off"]
    ));
}

#[test]
fn test_attach_heal_splits_a_fresh_viewer_when_the_parked_pane_is_another_members() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "claude", "s-me")],
        "@7",
    )
    .unwrap();
    let argv = fake_tmux_tagged(
        MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[
            ("dev:1", "hive-mirror", "on"),
            ("%1", "hive-hidden", "honey"),
            ("%1", "hive-role", "mirror"),
            ("%1", "hive-agent", "scout"),
        ],
    );

    attach_cmd("honey");

    // scout's parked pane stays parked; the orch gets its own viewer.
    assert_eq!(count(&argv, "join-pane"), 0);
    assert_eq!(count(&argv, "split-window"), 1);
    assert!(argv
        .borrow()
        .iter()
        .any(|a| a[0] == "send-keys" && a.iter().any(|arg| arg.contains("hive view s-me"))));
}

#[test]
fn test_attach_rebuild_hands_the_first_pane_to_the_next_member_when_the_mirror_is_withheld() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[
            member_row("orch", "claude", "s-me"),
            member_row("sage", "grok", "sid-sage"),
        ],
        "",
    )
    .unwrap();
    let argv = fake_tmux_tagged("", &[], &[("honey:1", "hive-mirror", "off")]);

    attach_cmd("honey");

    // The withheld mirror consumes no pane: sage takes the window's own.
    assert_eq!(count(&argv, "split-window"), 0);
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-role", "agent"]
    ));
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-agent", "sage"]
    ));
    assert!(argv
        .borrow()
        .iter()
        .all(|a| !(a[0] == "send-keys" && a.iter().any(|arg| arg.contains("hive view")))));
}

#[test]
fn test_attach_heal_withholds_the_mirror_and_records_off_by_default() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "claude", "s-me")],
        "@7",
    )
    .unwrap();
    let argv = fake_tmux(MIRROR_WINDOW, &["%0\t\tzsh\tterminal\t\thoney\t\t"]);

    attach_cmd("honey");

    // Nothing recorded: no mirror pane, no viewer; the window records the
    // collapsed default so the orch chip appears and can open it.
    assert_eq!(count(&argv, "split-window"), 0);
    assert!(argv
        .borrow()
        .iter()
        .all(|a| !(a[0] == "send-keys" && a.iter().any(|arg| arg.contains("hive view")))));
    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "off"]
    ));
}

#[test]
fn test_attach_heal_builds_the_mirror_when_the_window_records_on() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "claude", "s-me")],
        "@7",
    )
    .unwrap();
    let argv = fake_tmux_tagged(
        MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[("dev:1", "hive-mirror", "on")],
    );

    attach_cmd("honey");

    assert_eq!(count(&argv, "split-window"), 1);
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-role", "mirror"]
    ));
    // The mirror beside the shell pane: the plan for a 200x50 window with
    // a mirror and one member, its key recorded on the window.
    let planned = planned_layout((200, 50), &[("%1", "mirror"), ("%0", "")]);
    assert!(has_row(
        &argv,
        &["select-layout", "-t", "dev:1", &planned.layout]
    ));
    assert!(has_row(
        &argv,
        &[
            "set-window-option",
            "-t",
            "dev:1",
            "@hive-layout",
            &planned.key
        ]
    ));
}

/// The real planner's answer for `panes` (`(id, role)`, window order).
fn planned_layout(size: (i64, i64), panes: &[(&str, &str)]) -> crate::layout::Plan {
    let panes: Vec<crate::tmux::PaneInfo> = panes
        .iter()
        .map(|(id, role)| crate::tmux::PaneInfo {
            pane_id: id.to_string(),
            role: role.to_string(),
            ..Default::default()
        })
        .collect();
    crate::layout::plan(size, &panes).expect("a plan for two panes")
}

const MIRROR_WINDOW: &str = "dev:1\t@7\thoney\t\t\t\n";

fn honey_with_a_session_orch() {
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "claude", "s-me")],
        "@7",
    )
    .unwrap();
}

const BREAK_PANE_TAIL: [&str; 5] = [
    "-n",
    "honey·mirror",
    "-P",
    "-F",
    "#{session_name}:#{window_index}\t#{pane_id}",
];

#[test]
fn test_mirror_off_breaks_the_pane_into_the_team_session_records_off_and_retiles() {
    let _env = display_env();
    honey_with_a_session_orch();
    let argv = fake_tmux_sessions(
        MIRROR_WINDOW,
        &[
            "%0\t\tzsh\tterminal\torch\thoney\t\t",
            "%1\t[orch]\thive\tmirror\torch\thoney\tclaude\t",
            "%2\t[sage]\tgrok\tagent\tsage\thoney\tgrok\t",
        ],
        &[("dev:1", "hive-team", "honey")],
        &["dev", "honey"],
    );

    assert_eq!(mirror("off", ""), Ok("mirror off (honey)".to_string()));

    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "off"]
    ));
    let mut row = vec!["break-pane", "-s", "%1", "-d", "-t", "=honey:"];
    row.extend(&BREAK_PANE_TAIL);
    assert!(has_row(&argv, &row));
    assert!(has_row(
        &argv,
        &[
            "set-window-option",
            "-t",
            "honey:9",
            "@hive-hidden",
            "honey"
        ]
    ));
    assert_eq!(count(&argv, "kill-pane"), 0);
    // The two survivors are planned side by side (200x50 is landscape).
    let planned = planned_layout((200, 50), &[("%0", ""), ("%2", "agent")]);
    assert_eq!(planned.key, "landscape/m2/no-mirror/2x1");
    assert!(has_row(
        &argv,
        &["select-layout", "-t", "dev:1", &planned.layout]
    ));
    assert_eq!(count(&argv, "select-layout"), 1);
}

#[test]
fn test_mirror_off_without_a_team_session_parks_the_pane_in_the_callers_session() {
    let _env = display_env();
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        MIRROR_WINDOW,
        &[
            "%0\t\tzsh\tterminal\torch\thoney\t\t",
            "%1\t[orch]\thive\tmirror\torch\thoney\tclaude\t",
        ],
        &[("dev:1", "hive-team", "honey")],
    );

    assert_eq!(mirror("off", ""), Ok("mirror off (honey)".to_string()));

    let mut row = vec!["break-pane", "-s", "%1", "-d"];
    row.extend(&BREAK_PANE_TAIL);
    assert!(has_row(&argv, &row));
    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:9", "@hive-hidden", "honey"]
    ));
}

#[test]
fn test_mirror_off_refuses_when_the_mirror_is_the_only_pane() {
    let _env = display_env();
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        MIRROR_WINDOW,
        &["%1\t[orch]\thive\tmirror\torch\thoney\tclaude\t"],
        &[("dev:1", "hive-team", "honey")],
    );

    let err = mirror("off", "").unwrap_err();

    assert!(err.contains("only pane"), "{err}");
    // A refusal records nothing: the mirror is still on screen.
    assert_eq!(count(&argv, "set-window-option"), 0);
    assert_eq!(count(&argv, "break-pane"), 0);
}

#[test]
fn test_mirror_off_without_a_mirror_records_off_and_leaves_the_window_alone() {
    let _env = display_env();
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\torch\thoney\t\t"],
        &[("dev:1", "hive-team", "honey")],
    );

    assert_eq!(
        mirror("off", ""),
        Ok("mirror off (honey): no mirror".to_string())
    );

    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "off"]
    ));
    assert_eq!(count(&argv, "break-pane"), 0);
    assert_eq!(count(&argv, "select-layout"), 0);
}

#[test]
fn test_mirror_off_refuses_from_the_mirror_pane_but_not_with_window() {
    let mut env = display_env();
    env.env.set("TMUX_PANE", "%1");
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        MIRROR_WINDOW,
        &[
            "%0\t\tzsh\tterminal\t\thoney\t\t",
            "%1\t[orch]\thive\tmirror\torch\thoney\tclaude\t",
        ],
        &[("dev:1", "hive-team", "honey")],
    );

    let err = mirror("off", "").unwrap_err();
    assert!(err.contains("mirror"), "{err}");
    assert_eq!(count(&argv, "break-pane"), 0);
    assert!(argv.borrow().iter().all(|a| a[0] != "set-window-option"));

    // The bindings name the window; a click is never "from" a pane.
    assert_eq!(mirror("off", "dev:1"), Ok("mirror off (honey)".to_string()));
    assert_eq!(count(&argv, "break-pane"), 1);
}

#[test]
fn test_mirror_on_joins_the_hidden_pane_first_and_retiles() {
    let _env = display_env();
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[
            ("dev:1", "hive-team", "honey"),
            ("dev:1", "hive-mirror", "off"),
            ("%1", "hive-hidden", "honey"),
            ("%1", "hive-role", "mirror"),
            ("%1", "hive-agent", "orch"),
        ],
    );

    assert_eq!(mirror("on", ""), Ok("mirror on (honey)".to_string()));

    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "on"]
    ));
    assert!(has_row(
        &argv,
        &["join-pane", "-h", "-b", "-d", "-s", "%1", "-t", "%0"]
    ));
    assert_eq!(count(&argv, "split-window"), 0);
    assert_eq!(count(&argv, "select-layout"), 1);
}

#[test]
fn test_mirror_on_with_the_mirror_shown_says_so_and_leaves_the_window_alone() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        MIRROR_WINDOW,
        &[
            "%0\t\tzsh\tterminal\t\thoney\t\t",
            "%1\t[orch]\thive\tmirror\torch\thoney\tclaude\t",
        ],
        &[("dev:1", "hive-team", "honey")],
    );

    assert_eq!(
        mirror("on", ""),
        Ok("mirror on (honey): already shown".to_string())
    );

    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "on"]
    ));
    assert_eq!(count(&argv, "join-pane"), 0);
    assert_eq!(count(&argv, "split-window"), 0);
    assert_eq!(count(&argv, "select-layout"), 0);
}

#[test]
fn test_mirror_on_rebuilds_the_mirror_when_no_hidden_pane_exists() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[
            ("dev:1", "hive-team", "honey"),
            ("dev:1", "hive-mirror", "off"),
        ],
    );

    assert_eq!(mirror("on", ""), Ok("mirror on (honey)".to_string()));

    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "on"]
    ));
    assert_eq!(count(&argv, "join-pane"), 0);
    assert_eq!(count(&argv, "split-window"), 1);
    assert!(argv
        .borrow()
        .iter()
        .any(|a| a[0] == "send-keys" && a.iter().any(|arg| arg.contains("hive view s-me"))));
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-role", "mirror"]
    ));
}

#[test]
fn test_mirror_on_with_nothing_to_show_says_so() {
    let _env = display_env();
    // A team whose roster has no session member: nothing to mirror.
    crate::registry::record_team("honey", "", "100.0", &[], "@7").unwrap();
    let argv = fake_tmux_tagged(
        MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[("dev:1", "hive-team", "honey")],
    );

    assert_eq!(
        mirror("on", ""),
        Ok("mirror on (honey): no session mirror to show".to_string())
    );

    assert_eq!(count(&argv, "join-pane"), 0);
    assert_eq!(count(&argv, "split-window"), 0);
    // Nothing shown, nothing recorded: no orch chip that toggles nothing.
    assert_eq!(count(&argv, "set-window-option"), 0);
}

#[test]
fn test_mirror_on_leaves_another_members_parked_pane_alone() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    honey_with_a_session_orch();
    // scout's parked mirror is scout's: the orch gets a fresh viewer.
    let argv = fake_tmux_tagged(
        MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[
            ("dev:1", "hive-team", "honey"),
            ("dev:1", "hive-mirror", "off"),
            ("%1", "hive-hidden", "honey"),
            ("%1", "hive-role", "mirror"),
            ("%1", "hive-agent", "scout"),
        ],
    );

    assert_eq!(mirror("on", ""), Ok("mirror on (honey)".to_string()));

    assert_eq!(count(&argv, "join-pane"), 0);
    assert_eq!(count(&argv, "split-window"), 1);
    assert!(argv
        .borrow()
        .iter()
        .any(|a| a[0] == "send-keys" && a.iter().any(|arg| arg.contains("hive view s-me"))));
}

#[test]
fn test_mirror_toggles_by_presence() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[("dev:1", "hive-team", "honey")],
    );

    // No mirror: the toggle shows one…
    assert_eq!(mirror("", ""), Ok("mirror on (honey)".to_string()));
    assert_eq!(count(&argv, "split-window"), 1);
    // …and with the mirror on screen the next toggle parks it.
    assert_eq!(mirror("", ""), Ok("mirror off (honey)".to_string()));
    assert_eq!(count(&argv, "break-pane"), 1);
    assert!(argv
        .borrow()
        .iter()
        .any(|a| a[0] == "break-pane" && a[2] == "%1"));
}

#[test]
fn test_mirror_window_flag_names_the_window() {
    // A run-shell job (the status click, prefix+m): TMUX but no TMUX_PANE.
    let mut env = display_env_outside();
    env.env.set("TMUX", "/tmp/hive-test-tmux,1,0");
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        MIRROR_WINDOW,
        &[
            "%0\t\tzsh\tterminal\t\thoney\t\t",
            "%1\t[orch]\thive\tmirror\torch\thoney\tclaude\t",
        ],
        &[("dev:1", "hive-team", "honey")],
    );

    assert!(mirror("on", "").is_err());
    assert_eq!(
        mirror("on", "dev:1"),
        Ok("mirror on (honey): already shown".to_string())
    );
    assert_eq!(mirror("off", "dev:1"), Ok("mirror off (honey)".to_string()));
    assert_eq!(count(&argv, "break-pane"), 1);
}

#[test]
fn test_mirror_outside_a_team_window_fails() {
    let _env = display_env();
    let _argv = fake_tmux("dev:1\t@7\t\t\t\t\n", &[]);

    let err = mirror("on", "").unwrap_err();

    assert!(err.contains("hive ls"), "{err}");
}

// --- wake -----------------------------------------------------------------

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Map};

use crate::tmux::WindowExtra;

fn window(
    target: &str,
    id: &str,
    session: &str,
    team: &str,
    ws: &str,
    created: &str,
) -> WindowExtra {
    WindowExtra {
        window: target.to_string(),
        window_id: id.to_string(),
        session_id: session.to_string(),
        session_name: target.split(':').next().unwrap_or_default().to_string(),
        team: team.to_string(),
        workspace: ws.to_string(),
        created: created.to_string(),
        token: String::new(),
    }
}

/// A tmux answering `list-windows -t <session>` with the fixture windows
/// of that session in the snapshot format, and a window's `#{session_id}`
/// from the same fixture; every call is recorded.
fn wake_tmux(windows: Vec<WindowExtra>) -> Argv {
    let argv: Argv = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let recorded = std::rc::Rc::clone(&argv);
    crate::tmux::set_run_override(move |args, _check, _timeout| {
        recorded.borrow_mut().push(args.to_vec());
        let out = match args[0].as_str() {
            "list-windows" => {
                assert_eq!(args[1], "-t");
                let session = args[2].as_str();
                assert!(
                    session.starts_with('$'),
                    "a session id, never a name: {session}"
                );
                windows
                    .iter()
                    .filter(|w| w.session_id == session)
                    .map(|w| {
                        format!(
                            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                            w.window,
                            w.window_id,
                            w.session_id,
                            w.session_name,
                            w.team,
                            w.workspace,
                            w.created,
                            w.token
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            "display-message" => {
                assert_eq!(args.last().map(String::as_str), Some("#{session_id}"));
                windows
                    .iter()
                    .find(|w| w.window == args[2])
                    .map(|w| w.session_id.clone())
                    .unwrap_or_default()
            }
            _ => String::new(),
        };
        Ok(crate::tmux::Run {
            returncode: 0,
            stdout: format!("{out}\n"),
            stderr: String::new(),
        })
    });
    argv
}

type Spawns = Arc<Mutex<Vec<Vec<String>>>>;

/// The hived seams under a wake: no desk answers until one is spawned on
/// a workspace, after which that workspace's ping matches; *up* names
/// the workspaces whose desk is up from the start.
fn hived_seams(up: &[&str]) -> (crate::hived::testhook::Guard, Spawns) {
    let spawns: Spawns = Arc::new(Mutex::new(Vec::new()));
    let serving: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(
        up.iter()
            .map(|ws| (ws.to_string(), String::new()))
            .collect(),
    ));
    let identity = |team: &str| {
        let mut m = Map::new();
        m.insert("ok".to_string(), Value::Bool(true));
        m.insert(
            "apiVersion".to_string(),
            Value::from(crate::hived::HIVED_API_VERSION),
        );
        m.insert(
            "buildHash".to_string(),
            Value::from(crate::hived::hived_build_hash()),
        );
        m.insert("team".to_string(), Value::from(team));
        m
    };
    let served = Arc::clone(&serving);
    let spawned = Arc::clone(&spawns);
    let guard = crate::hived::testhook::install(crate::hived::testhook::Hook {
        request_ping: Some(Arc::new(move |ws, _timeout| {
            let serving = served.lock().unwrap();
            let team = serving.get(ws)?;
            // A desk that was up from the start answers for whatever team
            // asks; a spawned one, for the team it was spawned for.
            let team = if team.is_empty() {
                std::fs::read_to_string(std::path::Path::new(ws).join("team-name"))
                    .unwrap_or_default()
            } else {
                team.clone()
            };
            Some(identity(&team))
        })),
        cleanup_socket: Some(Arc::new(|_ws| {})),
        popen: Some(Arc::new(move |argv, _stderr| {
            spawned.lock().unwrap().push(argv.to_vec());
            serving
                .lock()
                .unwrap()
                .insert(argv[2].clone(), argv[3].clone());
            4242
        })),
        ..Default::default()
    });
    (guard, spawns)
}

fn workspace(env: &DisplayEnv, name: &str) -> String {
    let ws = env._tmp.path().join(name);
    std::fs::create_dir_all(&ws).unwrap();
    ws.to_string_lossy().into_owned()
}

fn write_marker(ws: &str, text: &str) {
    let path = crate::hived::asleep_marker_path(ws);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn read_marker(ws: &str) -> Option<String> {
    std::fs::read_to_string(crate::hived::asleep_marker_path(ws)).ok()
}

fn entry_bytes(env: &DisplayEnv, team: &str) -> Option<Vec<u8>> {
    std::fs::read(team_dir(env, team).join("team.json")).ok()
}

/// The workspaces the spawns so far were for, in order.
fn spawned_workspaces(spawns: &Spawns) -> Vec<String> {
    spawns
        .lock()
        .unwrap()
        .iter()
        .map(|argv| {
            assert_eq!(argv[1], "--hived");
            argv[2].clone()
        })
        .collect()
}

#[test]
fn test_wake_requires_matching_instance_and_unwatched_marker() {
    let env = display_env_outside();
    let ws = workspace(&env, "ws");
    crate::registry::record_team("honey", &ws, "100", &[], "@7").unwrap();
    let (_hived, spawns) = hived_seams(&[]);
    let honey = |target: &str, id: &str, ws: &str, created: &str| {
        window(target, id, "$1", "honey", ws, created)
    };

    // The full match behind an unwatched marker: one desk, told its window.
    let _tmux = wake_tmux(vec![honey("honey:1", "@7", &ws, "100")]);
    write_marker(&ws, "{\"reason\":\"unwatched\",\"at\":1}\n");
    wake_cmd("$1", "");
    let argv = spawns.lock().unwrap().clone();
    assert_eq!(argv.len(), 1);
    assert_eq!(
        argv[0][1..],
        ["--hived", &ws, "honey", "honey:1", "@7"].map(str::to_string)
    );
    // The same instance shown twice (a linked or duplicated window) is
    // woken once, and the desk that is now up is left alone thereafter.
    let _tmux = wake_tmux(vec![
        honey("honey:1", "@7", &ws, "100"),
        honey("main:4", "@7", &ws, "100.0"),
    ]);
    wake_cmd("$1", "");
    assert_eq!(spawns.lock().unwrap().len(), 1);

    // Everything short of the full match starts nothing and writes
    // nothing: the marker and the registry entry keep their bytes.
    let reset = |marker: &str| {
        write_marker(&ws, marker);
        (read_marker(&ws), entry_bytes(&env, "honey"))
    };
    let unwatched = "{\"reason\":\"unwatched\",\"at\":2}\n";
    let (_hived, spawns) = hived_seams(&[]);
    let cases: Vec<(&str, &str, Vec<WindowExtra>)> = vec![
        (
            "window-gone marker",
            "{\"reason\":\"window-gone\"}\n",
            vec![honey("honey:1", "@7", &ws, "100")],
        ),
        (
            "display-unreachable marker",
            "{\"reason\":\"display-unreachable\"}\n",
            vec![honey("honey:1", "@7", &ws, "100")],
        ),
        (
            "bad marker",
            "not json\n",
            vec![honey("honey:1", "@7", &ws, "100")],
        ),
        (
            "no team tag",
            unwatched,
            vec![honey("honey:1", "@7", "", "")]
                .into_iter()
                .map(|mut w| {
                    w.team.clear();
                    w
                })
                .collect(),
        ),
        (
            "other workspace",
            unwatched,
            vec![honey("honey:1", "@7", "/ws/elsewhere", "100")],
        ),
        (
            "no workspace tag",
            unwatched,
            vec![honey("honey:1", "@7", "", "100")],
        ),
        (
            "other createdAt",
            unwatched,
            vec![honey("honey:1", "@7", &ws, "99")],
        ),
        (
            "no createdAt tag",
            unwatched,
            vec![honey("honey:1", "@7", &ws, "")],
        ),
        (
            "no entry for the tag",
            unwatched,
            vec![window("honey:1", "@7", "$1", "ghost", &ws, "100")],
        ),
        (
            "another session's window",
            unwatched,
            vec![window("other:1", "@9", "$2", "honey", &ws, "100")],
        ),
    ];
    for (what, marker, windows) in cases {
        let before = reset(marker);
        let _tmux = wake_tmux(windows);
        wake_cmd("$1", "");
        assert!(spawns.lock().unwrap().is_empty(), "{what} spawned a hived");
        assert_eq!(
            (read_marker(&ws), entry_bytes(&env, "honey")),
            before,
            "{what}"
        );
    }
    // No marker at all: a desk that never ran, or left for good.
    std::fs::remove_file(crate::hived::asleep_marker_path(&ws)).unwrap();
    let _tmux = wake_tmux(vec![honey("honey:1", "@7", &ws, "100")]);
    wake_cmd("$1", "");
    assert!(
        spawns.lock().unwrap().is_empty(),
        "no marker spawned a hived"
    );
    assert_eq!(read_marker(&ws), None);

    // A team the collector is archiving is not admitted into either.
    let before = reset(unwatched);
    crate::registry::update_entry("honey", |entry| {
        entry.insert(
            "gc".to_string(),
            json!({"closing": {"at": crate::gc::epoch_now(), "by": "test"}}),
        );
        true
    })
    .unwrap();
    let closing = entry_bytes(&env, "honey");
    assert_ne!(closing, before.1);
    let _tmux = wake_tmux(vec![honey("honey:1", "@7", &ws, "100")]);
    wake_cmd("$1", "");
    assert!(
        spawns.lock().unwrap().is_empty(),
        "a closing team spawned a hived"
    );
    assert_eq!(read_marker(&ws).as_deref(), Some(unwatched));
    assert_eq!(entry_bytes(&env, "honey"), closing);
    // No registry entry: nothing to wake, nothing written.
    crate::registry::delete_team("honey").unwrap();
    let _tmux = wake_tmux(vec![honey("honey:1", "@7", &ws, "100")]);
    wake_cmd("$1", "");
    assert!(spawns.lock().unwrap().is_empty());
    assert_eq!(entry_bytes(&env, "honey"), None);

    // A desk that is up answers the ping: no new generation.
    crate::registry::record_team("honey", &ws, "100", &[], "@7").unwrap();
    std::fs::write(std::path::Path::new(&ws).join("team-name"), "honey").unwrap();
    let (_hived, spawns) = hived_seams(&[&ws]);
    write_marker(&ws, unwatched);
    let _tmux = wake_tmux(vec![honey("honey:1", "@7", &ws, "100")]);
    wake_cmd("$1", "");
    assert!(
        spawns.lock().unwrap().is_empty(),
        "a matching desk was replaced"
    );
}

#[test]
fn test_session_wake_finds_team_behind_plain_current_window() {
    let env = display_env_outside();
    let honey_ws = workspace(&env, "honey-ws");
    let comb_ws = workspace(&env, "comb-ws");
    crate::registry::record_team("honey", &honey_ws, "100", &[], "@7").unwrap();
    crate::registry::record_team("comb", &comb_ws, "200", &[], "@8").unwrap();
    let unwatched = "{\"reason\":\"unwatched\"}\n";
    write_marker(&honey_ws, unwatched);
    write_marker(&comb_ws, unwatched);
    // The session's current window is a plain shell; behind it two teams
    // of this home, a window of a team another home registered under the
    // same name (its workspace is not this home's), an earlier instance
    // of honey, and a window of another session.
    let windows = || {
        vec![
            window("dev:1", "@1", "$1", "", "", ""),
            window("dev:2", "@7", "$1", "honey", &honey_ws, "100"),
            window("dev:3", "@8", "$1", "comb", &comb_ws, "200"),
            window("dev:4", "@9", "$1", "comb", "/other/home/comb", "200"),
            window("dev:5", "@10", "$1", "honey", &honey_ws, "50"),
            window("far:1", "@11", "$2", "honey", &honey_ws, "100"),
        ]
    };

    let (_hived, spawns) = hived_seams(&[]);
    let _tmux = wake_tmux(windows());
    wake_cmd("$1", "");
    assert_eq!(
        spawned_workspaces(&spawns),
        vec![honey_ws.clone(), comb_ws.clone()]
    );
    let argv = spawns.lock().unwrap().clone();
    assert_eq!(argv[0][3..], ["honey", "dev:2", "@7"].map(str::to_string));
    assert_eq!(argv[1][3..], ["comb", "dev:3", "@8"].map(str::to_string));

    // `--window` on the shell window: its session, the same scan.
    let (_hived, spawns) = hived_seams(&[]);
    let tmux = wake_tmux(windows());
    wake_cmd("", "dev:1");
    assert_eq!(
        spawned_workspaces(&spawns),
        vec![honey_ws.clone(), comb_ws.clone()]
    );
    assert!(has_row(
        &tmux,
        &["display-message", "-t", "dev:1", "-p", "#{session_id}"]
    ));
    assert!(tmux
        .borrow()
        .iter()
        .any(|a| a[..3] == ["list-windows", "-t", "$1"].map(str::to_string)));

    // A window tmux does not know: nothing scanned, nothing started.
    let (_hived, spawns) = hived_seams(&[]);
    let tmux = wake_tmux(windows());
    wake_cmd("", "gone:9");
    assert!(spawns.lock().unwrap().is_empty());
    assert!(tmux.borrow().iter().all(|a| a[0] != "list-windows"));

    // The verb takes exactly one of the two, and has its help text.
    let cli = || crate::cli::build_cli();
    assert!(cli().try_get_matches_from(["hive", "wake"]).is_err());
    assert!(cli()
        .try_get_matches_from(["hive", "wake", "--session", "$1", "--window", "dev:1"])
        .is_err());
    assert!(cli()
        .try_get_matches_from(["hive", "wake", "--session", "$1"])
        .is_ok());
    assert!(cli()
        .try_get_matches_from(["hive", "wake", "--window", "dev:1"])
        .is_ok());
    let help = crate::cli::help_text::help_for(&["wake"]).expect("help for the hidden verb");
    assert!(
        help.contains("--session") && help.contains("--window"),
        "{help}"
    );
}
