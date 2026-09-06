use serde_json::Value;

use super::*;
use crate::testkit::{
    claude_session_me, count, display_env, display_env_outside, fake_tmux, fake_tmux_sessions,
    fake_tmux_tagged, has_row, member_row,
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

    // One window — in the caller's own session, since the caller is inside
    // tmux — and one split for the second attachable member; the member
    // with no engine identity gets no pane.
    assert_eq!(count(&argv, "new-window"), 1);
    assert!(has_row(
        &argv,
        &[
            "new-window",
            "-t",
            "dev:",
            "-d",
            "-n",
            "honey",
            "-c",
            "/tmp",
            "-P",
            "-F",
            "#{session_name}:#{window_index}\t#{pane_id}",
        ]
    ));
    assert_eq!(count(&argv, "split-window"), 1);
    assert!(has_row(&argv, &["switch-client", "-t", "dev:2"]));
    // The freshly built window id lands in the registry's display cache.
    assert_eq!(
        crate::registry::load("honey").unwrap()["display"],
        Value::from("@7")
    );
    // A window in the caller's own session gets no status bar and no
    // binding: their status line is theirs.
    assert_eq!(count(&argv, "bind-key"), 0);
    assert!(argv
        .borrow()
        .iter()
        .all(|a| !(a[0] == "set-option" && a.get(3).map(String::as_str) == Some("status"))));
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
    // The window records nothing; the orch's mirror is parked from an
    // earlier `hive mirror off` on a window since killed by hand.
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
    // A mirror on screen makes the orch chip appear.
    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "on"]
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
    let argv = fake_tmux_tagged("", &[], &[("dev:2", "hive-mirror", "off")]);

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
fn test_attach_heal_builds_the_mirror_when_not_suppressed() {
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

    assert_eq!(count(&argv, "split-window"), 1);
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-role", "mirror"]
    ));
    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "on"]
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
    assert_eq!(planned.key, "landscape/m2/no-mirror/no-dock/2x1");
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
    // The flow-rig shape: a team whose roster has no session member and
    // whose rig mirror is gone for good.
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
fn test_mirror_on_joins_a_parked_rig_mirror_that_names_no_member() {
    let _env = display_env();
    crate::registry::record_team("honey", "", "100.0", &[], "@7").unwrap();
    let argv = fake_tmux_tagged(
        MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[
            ("dev:1", "hive-team", "honey"),
            ("dev:1", "hive-mirror", "off"),
            ("%1", "hive-hidden", "honey"),
            ("%1", "hive-role", "mirror"),
        ],
    );

    assert_eq!(mirror("on", ""), Ok("mirror on (honey)".to_string()));

    assert!(has_row(
        &argv,
        &["join-pane", "-h", "-b", "-d", "-s", "%1", "-t", "%0"]
    ));
    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "on"]
    ));
    assert_eq!(count(&argv, "split-window"), 0);
    assert_eq!(count(&argv, "select-layout"), 1);
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
