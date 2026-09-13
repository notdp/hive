use super::*;
use crate::testenv::EnvGuard;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

type Calls = Rc<RefCell<Vec<(Vec<String>, bool, u64)>>>;

fn set_exec_override(
    f: impl FnMut(&[String], u64, Option<&str>) -> Result<Run, TmuxError> + 'static,
) {
    EXEC_OVERRIDE.with(|o| *o.borrow_mut() = Some(Box::new(f)));
}

fn timeout_run() {
    set_exec_override(|_argv, _timeout, _input| Err(TmuxError::Timeout));
}

fn capture_run(rc: i32, out: &'static str) -> Calls {
    let calls: Calls = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    set_run_override(move |args, check, timeout| {
        recorded.borrow_mut().push((args.to_vec(), check, timeout));
        Ok(ok_run(rc, out, ""))
    });
    calls
}

fn raising_run() {
    set_run_override(|_args, _check, _timeout| Err(TmuxError::Os("no tmux".to_string())));
}

fn is_timeout(err: &anyhow::Error) -> bool {
    matches!(err.downcast_ref::<TmuxError>(), Some(TmuxError::Timeout))
}

#[test]
fn test_run_probe_reads_timeout_as_unknown() {
    timeout_run();

    let result = run(&["list-panes"], false, 5).unwrap();

    assert_eq!(result.returncode, 1);
    assert_eq!(result.stderr, "timeout");
}

#[test]
fn test_run_timeout_raises_when_the_command_had_to_happen() {
    // check=true means the caller needs the command to have run: a busy tmux
    // server must not be able to fake a successful send-keys.
    timeout_run();

    assert!(matches!(
        run(&["list-panes"], true, 5),
        Err(TmuxError::Timeout)
    ));
    assert!(is_timeout(&send_keys("%1", "hello", true).unwrap_err()));
    assert!(is_timeout(&send_key("%1", "Escape").unwrap_err()));
}

#[test]
fn test_load_buffer_timeout_raises() {
    // A draft save that did not happen must not read as one — the caller
    // clears the pane's composer on the strength of this call.
    timeout_run();

    assert!(is_timeout(
        &load_buffer("hive_draft_1", "unsent thought").unwrap_err()
    ));
}

#[test]
fn test_session_helpers_delegate_to_tmux() {
    let calls: Calls = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    set_run_override(move |args, check, timeout| {
        recorded.borrow_mut().push((args.to_vec(), check, timeout));
        if args[0] == "has-session" {
            return Ok(ok_run(0, "", ""));
        }
        if args[0] == "new-session" {
            return Ok(ok_run(0, "%9\n", ""));
        }
        Ok(ok_run(0, "", ""))
    });

    assert!(has_session("dev"));
    assert_eq!(new_session("dev", 200, 50, None, None).unwrap(), "%9");
    kill_session("dev");
    kill_window("@7");

    let calls = calls.borrow();
    assert_eq!(calls[0].0[..3], v(&["has-session", "-t", "dev"]));
    assert_eq!(calls[1].0[0], "new-session");
    assert_eq!(calls[2].0, v(&["kill-session", "-t", "dev"]));
    assert_eq!(calls[3].0, v(&["kill-window", "-t", "@7"]));
}

#[test]
fn test_window_jump_helpers_issue_expected_tmux_commands() {
    // `attach` jumps with switch-client: select-window would only
    // retarget the window's own session, leaving this client where it is.
    let calls = capture_run(0, "");

    select_window("dev:2");
    switch_client("dev:2");

    let calls = calls.borrow();
    let got: Vec<Vec<String>> = calls.iter().map(|c| c.0.clone()).collect();
    assert_eq!(
        got,
        vec![
            v(&["select-window", "-t", "dev:2"]),
            v(&["switch-client", "-t", "dev:2"]),
        ]
    );
}

#[test]
fn test_send_keys_and_send_key_issue_expected_tmux_commands() {
    let calls = capture_run(0, "");

    send_keys("%1", "hello", true).unwrap();
    send_keys("%2", "raw", false).unwrap();
    send_key("%3", "Escape").unwrap();

    let calls = calls.borrow();
    let got: Vec<(Vec<String>, bool)> = calls.iter().map(|c| (c.0.clone(), c.1)).collect();
    assert_eq!(
        got,
        vec![
            (v(&["send-keys", "-t", "%1", "-l", "hello"]), true),
            (v(&["send-keys", "-t", "%1", "Enter"]), true),
            (v(&["send-keys", "-t", "%2", "-l", "raw"]), true),
            (v(&["send-keys", "-t", "%3", "Escape"]), true),
        ]
    );
}

#[test]
fn test_pane_mode_helpers_use_tmux_display_and_copy_mode() {
    let calls: Calls = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    set_run_override(move |args, check, timeout| {
        recorded.borrow_mut().push((args.to_vec(), check, timeout));
        let stdout = if args.len() >= 3 && args[..3] == v(&["display-message", "-t", "%1"]) {
            "1\n"
        } else {
            ""
        };
        Ok(ok_run(0, stdout, ""))
    });

    assert!(is_pane_in_mode("%1"));
    cancel_pane_mode("%1");

    let calls = calls.borrow();
    let got: Vec<(Vec<String>, bool)> = calls.iter().map(|c| (c.0.clone(), c.1)).collect();
    assert_eq!(
        got,
        vec![
            (
                v(&["display-message", "-t", "%1", "-p", "#{pane_in_mode}"]),
                false
            ),
            (v(&["copy-mode", "-q", "-t", "%1"]), false),
        ]
    );
}

#[test]
fn test_capture_and_list_parsers() {
    set_run_override(|args, _check, _timeout| {
        if args[0] == "capture-pane" {
            return Ok(ok_run(0, "line1\nline2\n", ""));
        }
        if args.iter().any(|a| a == "#{pane_id}") {
            return Ok(ok_run(0, "%1\n%2\n", ""));
        }
        Ok(ok_run(0, "", ""))
    });

    assert_eq!(capture_pane("%1", 5, false).unwrap(), "line1\nline2");
    assert_eq!(list_panes("dev:0"), vec!["%1", "%2"]);
}

#[test]
fn test_is_pane_alive_parses_tmux_output() {
    set_run_override(|_args, _check, _timeout| Ok(ok_run(0, "%1 0\n%2 1\n", "")));

    assert!(is_pane_alive("%1"));
    assert!(!is_pane_alive("%2"));
    assert!(!is_pane_alive("%9"));
}

#[test]
fn test_is_pane_alive_treats_tmux_failure_as_alive() {
    set_run_override(|_args, _check, _timeout| Ok(ok_run(1, "", "timeout")));

    assert!(is_pane_alive("%1"));
}

#[test]
fn test_get_window_id_reads_display_message() {
    set_run_override(|args, _check, _timeout| {
        let stdout = if args.iter().any(|a| a == "#{window_id}") {
            "@42\n"
        } else {
            "2\n"
        };
        Ok(ok_run(0, stdout, ""))
    });

    assert_eq!(get_window_id("dev:2").as_deref(), Some("@42"));
}

#[test]
fn test_client_mode_and_popup_support_helpers() {
    set_run_override(|args, _check, _timeout| {
        let stdout = if args.iter().any(|a| a == "#{client_control_mode}") {
            "1\n"
        } else {
            ""
        };
        Ok(ok_run(0, stdout, ""))
    });

    assert_eq!(get_client_mode("%7"), "control");
}

#[test]
fn test_client_mode_returns_terminal_or_unknown() {
    set_run_override(|_args, _check, _timeout| Ok(ok_run(0, "0\n", "")));
    assert_eq!(get_client_mode("%8"), "terminal");

    set_run_override(|_args, _check, _timeout| Ok(ok_run(0, "", "")));
    assert_eq!(get_client_mode("%8"), "unknown");
}

#[test]
fn test_client_window_helpers_resolve_most_recent_client() {
    set_run_override(|args, _check, _timeout| {
        if args[0] == "list-clients" {
            return Ok(ok_run(
                0,
                "10\t0\t%1\t/dev/ttys010\n50\t0\t%5\t/dev/ttys050\n",
                "",
            ));
        }
        Ok(ok_run(0, "dev:5\n", ""))
    });

    assert_eq!(
        get_most_recent_client_tty(Some("dev")).as_deref(),
        Some("/dev/ttys050")
    );
    assert_eq!(
        get_client_window_target("/dev/ttys050").as_deref(),
        Some("dev:5")
    );
    assert_eq!(
        get_most_recent_client_window(Some("dev")).as_deref(),
        Some("dev:5")
    );
}

/// `-t <name>` falls back to prefix matching when no session has that
/// exact name: every client query pins the name, or names the id.
#[test]
fn test_client_queries_pin_the_session_target() {
    let calls: Calls = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    set_run_override(move |args, check, timeout| {
        recorded.borrow_mut().push((args.to_vec(), check, timeout));
        if args[0] == "list-clients" {
            return Ok(ok_run(0, "1\n1\n", ""));
        }
        Ok(ok_run(0, "dev:5\n", ""))
    });
    assert_eq!(watching_clients("fern"), Some(0), "control clients count 0");
    assert_eq!(watching_clients("$3"), Some(0));
    assert_eq!(watching_clients("=fern"), Some(0));
    assert_eq!(watching_clients(""), None);
    assert_eq!(get_most_recent_client_window(Some("fern")), None);
    assert_eq!(get_most_recent_client_tty(Some("$3")), None);
    let targets: Vec<String> = calls
        .borrow()
        .iter()
        .filter(|(args, _, _)| args[0] == "list-clients")
        .map(|(args, _, _)| args[2].clone())
        .collect();
    assert_eq!(targets, vec!["=fern", "$3", "=fern", "=fern", "$3"]);
    assert!(calls
        .borrow()
        .iter()
        .all(|(args, _, _)| args[0] != "list-clients" || args[1] == "-t"));
}

#[test]
fn test_client_helpers_ignore_control_mode_clients() {
    set_run_override(|args, _check, _timeout| {
        if args[0] == "list-clients" {
            return Ok(ok_run(
                0,
                "99\t1\t%control\t/dev/ttys999\n20\t0\t%term\t/dev/ttys020\n",
                "",
            ));
        }
        Ok(ok_run(0, "dev:2\n", ""))
    });

    assert_eq!(
        get_most_recent_client_tty(Some("dev")).as_deref(),
        Some("/dev/ttys020")
    );
    assert_eq!(
        get_most_recent_client_window(Some("dev")).as_deref(),
        Some("dev:2")
    );
}

#[test]
fn test_list_tty_processes_strips_dev_prefix_and_parses_output() {
    let calls: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    set_exec_override(move |argv, _timeout, _input| {
        recorded.borrow_mut().push(argv.to_vec());
        Ok(ok_run(
            0,
            "35214 -zsh -zsh\n35988 claude claude --verbose\n",
            "",
        ))
    });

    let processes = list_tty_processes("/dev/ttys012");
    assert_eq!(
        processes,
        vec![
            TTYProcessInfo {
                pid: "35214".to_string(),
                command: "-zsh".to_string(),
                argv: "-zsh".to_string(),
            },
            TTYProcessInfo {
                pid: "35988".to_string(),
                command: "claude".to_string(),
                argv: "claude --verbose".to_string(),
            },
        ]
    );
    let expected = v(&["ps", "-t", "ttys012", "-o", "pid=,comm=,command="]);
    assert_eq!(*calls.borrow(), vec![expected]);
}

#[test]
fn test_list_panes_full_parses_rows() {
    set_run_override(|args, _check, _timeout| {
        let fmt = args.last().map(String::as_str).unwrap_or("");
        let stdout = if fmt == PANE_BASE_FMT {
            "%1\tmain\tcodex\tagent\tclaude\tteam-a\t\n%2\tshell\tzsh\tterminal\tterm-1\tteam-a\t\n"
        } else {
            ""
        };
        Ok(ok_run(0, stdout, ""))
    });

    let full = list_panes_full("dev:0");

    assert_eq!(
        full[0],
        PaneInfo {
            pane_id: "%1".to_string(),
            title: "main".to_string(),
            command: "codex".to_string(),
            role: "agent".to_string(),
            agent: "claude".to_string(),
            team: "team-a".to_string(),
            ..Default::default()
        }
    );
    assert_eq!(full[1].role, "terminal");
}

#[test]
fn test_pane_option_helpers_and_tagging() {
    let calls: Calls = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    set_run_override(move |args, check, timeout| {
        recorded.borrow_mut().push((args.to_vec(), check, timeout));
        let stdout = if args[0] == "show-options" {
            "value\n"
        } else {
            ""
        };
        Ok(ok_run(0, stdout, ""))
    });

    set_pane_option("%1", "hive-role", "agent");
    assert_eq!(get_pane_option("%1", "hive-role").as_deref(), Some("value"));
    clear_pane_option("%1", "hive-role");
    tag_pane("%1", "agent", "claude", "team-a", "", "");
    clear_pane_tags("%1");

    let calls = calls.borrow();
    let argvs: Vec<Vec<String>> = calls.iter().map(|c| c.0.clone()).collect();
    assert_eq!(
        argvs[0],
        v(&["set-option", "-p", "-t", "%1", "@hive-role", "agent"])
    );
    assert_eq!(
        argvs[1],
        v(&["show-options", "-p", "-v", "-t", "%1", "@hive-role"])
    );
    assert_eq!(
        argvs[2],
        v(&["set-option", "-p", "-t", "%1", "-u", "@hive-role"])
    );
    assert!(argvs.contains(&v(&[
        "set-option",
        "-p",
        "-t",
        "%1",
        "@hive-agent",
        "claude"
    ])));
    assert!(argvs.contains(&v(&["set-option", "-p", "-t", "%1", "-u", "@hive-team"])));
    // `@hive-view` is derived from the claude view probe: release clears it
    // with the identity tags, or a reused pane keeps a dead border suffix.
    assert!(argvs.contains(&v(&["set-option", "-p", "-t", "%1", "-u", "@hive-view"])));
}

#[test]
fn test_tagging_a_pane_onto_another_cli_drops_the_claude_view() {
    // Only the claude view tick maintains @hive-view, and it skips non-claude
    // panes — an in-place retag must clear it or the suffix is stale forever.
    let calls: Calls = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    set_run_override(move |args, check, timeout| {
        recorded.borrow_mut().push((args.to_vec(), check, timeout));
        Ok(ok_run(0, "", ""))
    });

    tag_pane("%1", "agent", "blue", "team-a", "codex", "");
    let unset_view = v(&["set-option", "-p", "-t", "%1", "-u", "@hive-view"]);
    assert!(calls.borrow().iter().any(|c| c.0 == unset_view));

    calls.borrow_mut().clear();
    tag_pane("%1", "agent", "red", "team-a", "claude", "");
    assert!(!calls.borrow().iter().any(|c| c.0 == unset_view));
}

#[test]
fn test_enable_pane_border_status_uses_hive_member_format() {
    let calls = capture_run(0, "");

    enable_pane_border_status("dev:1");

    let calls = calls.borrow();
    assert_eq!(
        calls[0].0,
        v(&[
            "set-window-option",
            "-t",
            "dev:1",
            "pane-border-status",
            "top"
        ])
    );
    assert_eq!(
        calls[1].0,
        v(&[
            "set-window-option",
            "-t",
            "dev:1",
            "pane-border-format",
            HIVE_PANE_BORDER_FORMAT,
        ])
    );
    assert!(!HIVE_PANE_BORDER_FORMAT.contains("#[fg=colour220,bold]"));
    assert!(HIVE_PANE_BORDER_FORMAT.contains("#[fg=colour220]#[bold][!]"));
}

#[test]
fn test_configure_hive_window_disables_native_tmux_alerts_and_installs_layout_hooks() {
    let mut env = EnvGuard::new();
    env.set("HIVE_BIN", "/x/hive");
    let calls = capture_run(0, "");

    configure_hive_window("dev:1");

    let argvs: Vec<Vec<String>> = calls.borrow().iter().map(|c| c.0.clone()).collect();
    let mut expected = vec![
        v(&[
            "set-window-option",
            "-t",
            "dev:1",
            "pane-border-status",
            "top",
        ]),
        v(&[
            "set-window-option",
            "-t",
            "dev:1",
            "pane-border-format",
            HIVE_PANE_BORDER_FORMAT,
        ]),
        v(&[
            "set-window-option",
            "-t",
            "dev:1",
            "monitor-activity",
            "off",
        ]),
        v(&["set-window-option", "-t", "dev:1", "monitor-bell", "off"]),
    ];
    expected.extend(crate::layout::hook_argv("dev:1", "/x/hive"));
    assert_eq!(argvs, expected);
    assert_eq!(argvs.len(), 4 + crate::layout::LAYOUT_HOOKS.len());
}

const MIRROR_RUN_SHELL: &str =
    "run-shell -b \"/x/hive mirror --window '#{q:session_name}:#{window_index}' >/dev/null 2>&1 || true\"";

#[test]
fn test_team_status_argv_targets_the_session_id() {
    use crate::view_theme::ThemeKind;
    let rows = |option: &str, value: &str| v(&["set-option", "-t", "$3", option, value]);
    let p = status_palette(ThemeKind::Dark);
    assert_eq!(
        team_status_argv("$3", ThemeKind::Dark),
        vec![
            rows("mouse", "on"),
            rows("status", "2"),
            rows("status-style", p.bar),
            rows("status-left", ""),
            rows("status-right", ""),
            rows("status-format[0]", &team_status_format_0(p)),
            rows("status-format[1]", &team_status_format_1(p)),
        ]
    );
}

/// Light and dark differ in colours only: with every `#[…]` style stripped
/// the two bars are the same text, so a theme can never change what the
/// bar shows or which ranges it marks.
#[test]
fn test_team_status_palettes_differ_in_colours_only() {
    use crate::view_theme::ThemeKind;
    fn strip(format: &str) -> String {
        let mut out = String::new();
        let mut rest = format;
        while let Some(i) = rest.find("#[") {
            out.push_str(&rest[..i]);
            let j = rest[i..].find(']').expect("closed style") + i;
            rest = &rest[j + 1..];
        }
        out.push_str(rest);
        out
    }
    let dark = status_palette(ThemeKind::Dark);
    let light = status_palette(ThemeKind::Light);
    for (d, l) in [
        (team_status_format_0(dark), team_status_format_0(light)),
        (team_status_format_1(dark), team_status_format_1(light)),
    ] {
        assert_ne!(d, l);
        assert_eq!(strip(&d), strip(&l));
    }
    assert!(team_status_format_0(light).contains(&format!("bg={}", light.team_bg)));
}

#[test]
fn test_mirror_run_shell_escapes_a_dollar_in_the_binary_path_for_tmux() {
    assert_eq!(mirror_run_shell("/x/hive"), MIRROR_RUN_SHELL);
    assert_eq!(
        mirror_run_shell("'/tmp/we ird$x/hive'"),
        "run-shell -b \"'/tmp/we ird\\$x/hive' mirror --window '#{q:session_name}:#{window_index}' >/dev/null 2>&1 || true\""
    );
}

#[test]
fn test_status_click_binding_ends_in_the_stock_status_click() {
    let binding = status_click_binding("/x/hive", STOCK_STATUS_CLICK);
    assert_eq!(binding.len(), 9);
    assert_eq!(
        binding[..4],
        v(&["bind-key", "-T", "root", "MouseDown1Status"])[..]
    );
    assert_eq!(
        binding[4..7],
        v(&["if-shell", "-F", "#{==:#{mouse_status_range},hive-mirror}"])[..]
    );
    assert_eq!(binding[7], MIRROR_RUN_SHELL);
    assert_eq!(
        binding[8],
        "if-shell -F \"#{==:#{mouse_status_range},pane}\" \"select-pane -t =\" \"select-window -t =\""
    );
    assert!(binding[8]
        .trim_end_matches('"')
        .ends_with(STOCK_STATUS_CLICK));
    assert_eq!(STOCK_STATUS_CLICK, "select-window -t =");
}

#[test]
fn test_mirror_key_binding_is_gated_on_a_team_window_and_falls_back_elsewhere() {
    let head = [
        "bind-key",
        "-T",
        "prefix",
        "m",
        "if-shell",
        "-F",
        "#{@hive-team}",
        MIRROR_RUN_SHELL,
    ];
    assert_eq!(mirror_key_binding("/x/hive", ""), v(&head));
    let mut with_fallback = v(&head);
    with_fallback.push("select-pane -m".to_string());
    assert_eq!(
        mirror_key_binding("/x/hive", "select-pane -m"),
        with_fallback
    );
}

#[test]
fn test_bound_command_reads_the_list_keys_line() {
    assert_eq!(
        bound_command("bind-key -T prefix m select-pane -m\n"),
        Some("select-pane -m".to_string())
    );
    // Off a whole table (what 3.7 makes necessary): the `m` line, not `mm`.
    let table = "bind-key -T prefix M send-keys -X other\nbind-key -T prefix mm select-pane -M\nbind-key    -T prefix m       select-pane -m\n";
    assert_eq!(bound_command(table), Some("select-pane -m".to_string()));
    assert_eq!(
        bound_command_for(
            "bind-key    -T root MouseDown1Status          switch-client -t =\n",
            "root",
            "MouseDown1Status"
        ),
        Some("switch-client -t =".to_string())
    );
    // `-r` drops, tmux's `\;` separator becomes the ` ; ` an if-shell
    // branch string splits on, quoting stays verbatim.
    assert_eq!(
        bound_command(
            "bind-key -r -T prefix m swap-pane -s \"{top-left}\" \\; select-layout main-vertical"
        ),
        Some("swap-pane -s \"{top-left}\" ; select-layout main-vertical".to_string())
    );
    assert_eq!(bound_command(""), None);
    assert_eq!(bound_command("unknown key: m"), None);
}

/// A run override answering `list-keys -T prefix m` with *listed* and
/// `show-options -s -v @hive-prefix-m` with *stored*, recording every call.
fn prefix_m_server(listed: &'static str, stored: &'static str) -> Calls {
    let calls: Calls = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    set_run_override(move |args, check, timeout| {
        recorded.borrow_mut().push((args.to_vec(), check, timeout));
        let (rc, out) = match (args[0].as_str(), args.last().map(String::as_str)) {
            ("list-keys", _) if listed.is_empty() => (1, ""),
            ("list-keys", _) => (0, listed),
            ("show-options", Some(PREFIX_M_FALLBACK_OPTION)) if stored.is_empty() => (1, ""),
            ("show-options", Some(PREFIX_M_FALLBACK_OPTION)) => (0, stored),
            _ => (0, ""),
        };
        Ok(ok_run(rc, out, if rc == 0 { "" } else { "unknown key: m" }))
    });
    calls
}

fn argvs(calls: &Calls) -> Vec<Vec<String>> {
    calls.borrow().iter().map(|c| c.0.clone()).collect()
}

#[test]
fn test_prefix_m_fallback_remembers_the_command_the_key_had() {
    let calls = prefix_m_server("bind-key -T prefix m select-pane -m\n", "");

    assert_eq!(prefix_m_fallback(), "select-pane -m");

    assert_eq!(
        argvs(&calls),
        vec![
            v(&["list-keys", "-T", "prefix"]),
            v(&[
                "set-option",
                "-s",
                PREFIX_M_FALLBACK_OPTION,
                "select-pane -m"
            ]),
        ]
    );
}

#[test]
fn test_prefix_m_fallback_reads_the_remembered_command_behind_hives_own_binding() {
    let hive_binding = "bind-key -T prefix m if-shell -F \"#{@hive-team}\" \"run-shell -b \\\"/x/hive mirror --window '#{q:session_name}:#{window_index}' >/dev/null 2>&1 || true\\\"\" \"select-pane -m\"\n";
    let calls = prefix_m_server(hive_binding, "swap-pane -s \"{top-left}\"\n");

    assert_eq!(prefix_m_fallback(), "swap-pane -s \"{top-left}\"");

    // Nothing stored over the remembered command.
    assert_eq!(
        argvs(&calls),
        vec![
            v(&["list-keys", "-T", "prefix"]),
            v(&["show-options", "-s", "-v", PREFIX_M_FALLBACK_OPTION]),
        ]
    );
}

#[test]
fn test_prefix_m_fallback_is_empty_for_an_unbound_key() {
    let calls = prefix_m_server("", "");

    assert_eq!(prefix_m_fallback(), "");

    assert_eq!(argvs(&calls), vec![v(&["list-keys", "-T", "prefix"])]);
}

#[test]
fn test_install_team_status_runs_options_then_bindings() {
    let mut env = EnvGuard::new();
    env.set("HIVE_BIN", "/x/hive");
    let calls = prefix_m_server("bind-key -T prefix m select-pane -m\n", "");

    install_team_status("$3");

    // The two fallback probes come first (the rows are built before they
    // run) — the root table carries no status click here, so the stock
    // click stands in without a look at the option — then the session
    // options, then the two bindings.
    let mut expected = vec![
        v(&["list-keys", "-T", "root"]),
        v(&["list-keys", "-T", "prefix"]),
        v(&[
            "set-option",
            "-s",
            PREFIX_M_FALLBACK_OPTION,
            "select-pane -m",
        ]),
    ];
    expected.extend(team_status_argv(
        "$3",
        crate::view_theme::active_theme_kind(),
    ));
    expected.push(status_click_binding("/x/hive", STOCK_STATUS_CLICK));
    expected.push(mirror_key_binding("/x/hive", "select-pane -m"));
    // …then the two session hooks that wake a desk when a terminal
    // arrives: the array is read, and each hook gets one indexed entry.
    let wake = wake_run_shell(&wake_shell_line("/x/hive", &wake_environment()));
    expected.push(v(&["show-hooks", "-t", "$3"]));
    expected.push(v(&["set-hook", "-t", "$3", "client-attached[0]", &wake]));
    expected.push(v(&[
        "set-hook",
        "-t",
        "$3",
        "client-session-changed[0]",
        &wake,
    ]));
    assert_eq!(argvs(&calls), expected);
}

/// The bar reads options only — no `#(` shell-out — and every `@hive-`
/// key it names is one the CLI or the hived writes.
#[test]
fn test_team_status_format_reads_only_options() {
    let known = [
        "hive-team",
        "hive-mirror",
        "hive-role",
        "hive-agent",
        "hive-notify-active",
        "hive-unread",
        "hive-busy",
        "hive-pr",
        "hive-notify-text",
        "hive-ticker",
    ];
    let p = status_palette(crate::view_theme::ThemeKind::Dark);
    for format in [team_status_format_0(p), team_status_format_1(p)] {
        assert!(!format.contains("#("), "{format}");
        for (i, _) in format.match_indices("@hive-") {
            let key: String = format[i + 1..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            assert!(known.contains(&key.as_str()), "{key} is not a bar option");
        }
    }
}

/// Every team-window scan reads the tag through the hidden-window mask.
#[test]
fn test_team_window_scans_mask_the_hidden_window() {
    assert!(TEAM_WINDOW_FMT.contains(WINDOW_TEAM_FMT));
    assert!(!TEAM_WINDOW_FMT
        .replace(WINDOW_TEAM_FMT, "")
        .contains("@hive-team"));
}

#[test]
fn test_break_pane_targets_the_session_when_given() {
    let calls = capture_run(0, "honey:9\t%3\n");

    let (window, pane) = break_pane("%3", "honey·mirror", true, Some("=honey:")).unwrap();

    assert_eq!((window.as_str(), pane.as_str()), ("honey:9", "%3"));
    assert_eq!(
        calls.borrow()[0].0,
        v(&[
            "break-pane",
            "-s",
            "%3",
            "-d",
            "-t",
            "=honey:",
            "-n",
            "honey·mirror",
            "-P",
            "-F",
            "#{session_name}:#{window_index}\t#{pane_id}",
        ])
    );
    join_pane_before("%3", "%1");
    assert_eq!(
        calls.borrow()[1].0,
        v(&["join-pane", "-h", "-b", "-d", "-s", "%3", "-t", "%1"])
    );
}

#[test]
fn test_hidden_mirror_lookups_parse_the_listings() {
    // Server-wide (`-a`) listings: the parked window lives in the team
    // session, the caller may be anywhere.
    set_run_override(|args, _check, _timeout| {
        let out = if args == v(&["list-windows", "-a", "-F", "#{window_id}\t#{@hive-hidden}"]) {
            "@4\thoney\n@7\t\n"
        } else if args
            == v(&[
                "list-panes",
                "-a",
                "-F",
                "#{pane_id}\t#{window_id}\t#{@hive-role}",
            ])
        {
            "%9\t@4\tmirror\n%2\t@7\tagent\n"
        } else {
            ""
        };
        Ok(ok_run(0, out, ""))
    });

    assert_eq!(hidden_mirror_windows("honey"), vec!["@4".to_string()]);
    assert_eq!(hidden_mirror_pane("honey"), Some("%9".to_string()));
    assert_eq!(hidden_mirror_pane("comb"), None);
}

#[test]
fn test_parse_control_mode_output_matches_output_notifications() {
    assert_eq!(
        parse_control_mode_output("%output %2772 hello"),
        ("%2772".to_string(), "hello".to_string())
    );
    assert_eq!(
        parse_control_mode_output("%extended-output %2773 12 : world"),
        ("%2773".to_string(), "world".to_string())
    );
    assert_eq!(
        parse_control_mode_output("%session-changed $1 dev"),
        (String::new(), String::new())
    );
}

#[test]
fn test_control_mode_monitor_is_busy_uses_threshold() {
    let monitor = ControlModeOutputMonitor::new("613", "");
    monitor.inner.last_output_at.lock().unwrap().insert(
        "%9".to_string(),
        Instant::now() - Duration::from_secs_f64(1.0),
    );
    assert!(monitor.is_busy("%9", 3.0));
    monitor.inner.last_output_at.lock().unwrap().insert(
        "%9".to_string(),
        Instant::now() - Duration::from_secs_f64(4.0),
    );
    assert!(!monitor.is_busy("%9", 3.0));
}

#[test]
fn test_control_mode_payload_activity_ignores_pure_repaint_sequence() {
    let repaint = concat!(
        "\x1b[?2026h",
        "\x1b[49;2H\x1b[0m\x1b[49m\x1b[K",
        "\x1b[50;2H\x1b[0m\x1b[48;2;244;244;244m\x1b[K",
        "\x1b[51;28H\x1b[0m\x1b[48;2;244;244;244m\x1b[K",
        "\x1b[52;2H\x1b[0m\x1b[48;2;244;244;244m\x1b[K",
        "\x1b[53;52H\x1b[0m\x1b[49m\x1b[K",
        "\x1b[39m\x1b[49m\x1b[0m\x1b[?25h\x1b[51;3H\x1b[?2026l"
    );

    assert!(!control_mode_payload_has_activity(repaint));
}

#[test]
fn test_control_mode_payload_activity_accepts_visible_text_inside_styles() {
    assert!(control_mode_payload_has_activity("\x1b[2mhello\x1b[0m"));
}

#[test]
fn test_control_mode_payload_activity_keeps_text_between_st_terminated_osc_sequences() {
    let payload = "\x1b]0;a\x1b\\hello\x1b]0;b\x1b\\";

    assert!(control_mode_payload_has_activity(payload));
}

#[test]
fn test_control_mode_payload_activity_ignores_pure_dcs_sequence() {
    assert!(!control_mode_payload_has_activity(
        "\x1bP1;2;3payload\x1b\\"
    ));
}

#[test]
fn test_control_mode_payload_activity_accepts_visible_text_between_dcs_and_osc() {
    let payload = "\x1bPignored\x1b\\hello\x1b]0;title\x1b\\";

    assert!(control_mode_payload_has_activity(payload));
}

#[test]
fn test_control_mode_monitor_ignores_repaint_only_output() {
    // Repaint-only control sequences never mark a pane busy; the monitor
    // keeps no payload buffer (the pane-content delivery oracle is gone —
    // delivery confirmation is transcript-only).
    let monitor = ControlModeOutputMonitor::new("613", "");
    let payload = "\x1b[?2026h\x1b[49;2H\x1b[K\x1b[?2026l";

    monitor.record_control_mode_output("%9", payload);

    assert!(!monitor.is_busy("%9", 3.0));
}

#[test]
fn test_control_mode_monitor_marks_visible_text_busy() {
    let monitor = ControlModeOutputMonitor::new("613", "");

    monitor.record_control_mode_output("%9", "\x1b[2mhello\x1b[0m");

    assert!(monitor.is_busy("%9", 3.0));
}

#[test]
fn test_window_option_helpers() {
    let calls = capture_run(0, "");

    set_window_option("dev:1", "window-status-style", "fg=red");
    clear_window_option("dev:1", "window-status-style");

    let calls = calls.borrow();
    assert_eq!(
        calls[0].0,
        v(&[
            "set-window-option",
            "-t",
            "dev:1",
            "window-status-style",
            "fg=red"
        ])
    );
    assert_eq!(
        calls[1].0,
        v(&[
            "set-window-option",
            "-t",
            "dev:1",
            "-u",
            "window-status-style"
        ])
    );
}

#[test]
fn test_get_global_window_option_is_read_only_global_scope() {
    let calls = capture_run(0, "  #I #W  \n");

    let value = get_global_window_option("window-status-format");

    // Read-only `show-options -w -g -v`, no `-t` target — global scope only.
    let calls = calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (calls[0].0.clone(), calls[0].1),
        (
            v(&["show-options", "-w", "-g", "-v", "window-status-format"]),
            false
        )
    );
    // Meaningful leading/trailing padding survives; only the newline is stripped.
    assert_eq!(value.as_deref(), Some("  #I #W  "));
}

#[test]
fn test_get_global_window_option_returns_none_when_unset() {
    set_run_override(|_args, _check, _timeout| Ok(ok_run(0, "\n", "")));
    assert_eq!(get_global_window_option("window-status-format"), None);
}

#[test]
fn test_list_panes_full_or_none_is_status_aware() {
    set_run_override(|_args, _check, _timeout| {
        Ok(ok_run(
            0,
            "%1\t[w]\tzsh\tagent\tworker\tt1\tclaude\tduo\t\n",
            "",
        ))
    });
    let panes = list_panes_full_or_none("dev:0");
    assert!(panes.is_some());
    assert_eq!(panes.unwrap()[0].pane_id, "%1");
    assert_eq!(list_panes_full("dev:0")[0].pane_id, "%1");

    set_run_override(|_args, _check, _timeout| Ok(ok_run(1, "", "timeout")));
    assert_eq!(list_panes_full_or_none("dev:0"), None);
    assert_eq!(list_panes_full("dev:0"), Vec::new());

    set_run_override(|_args, _check, _timeout| Ok(ok_run(0, "", "")));
    assert_eq!(list_panes_full_or_none("dev:0"), Some(Vec::new()));
}

#[test]
fn test_pane_scan_status_maps_no_server_variants() {
    for stderr in [
        "no server running on /tmp/tmux-501/default",
        "error connecting to /x/tmux-501/default (No such file or directory)",
    ] {
        set_run_override(move |_args, _check, _timeout| Ok(ok_run(1, "", stderr)));
        assert_eq!(list_panes_all_status(), (None, "no-server"));
        assert_eq!(list_team_windows_status(), (None, "no-server"));
    }

    set_run_override(|_args, _check, _timeout| Ok(ok_run(1, "", "timeout")));
    assert_eq!(list_panes_all_status(), (None, "unknown"));
    assert_eq!(list_team_windows_status(), (None, "unknown"));
}

#[test]
fn test_pane_scan_status_reads_a_sanitized_listing_as_unknown() {
    // A client tmux does not treat as UTF-8 gets its tabs written as `_`:
    // the id column is then the whole line and names no pane. Such a
    // listing is unreadable, never an empty server or a server without
    // these panes.
    let sanitized = "%1_[worker]_node_agent_worker_t_codex_\n%2_zsh_zsh_____\n";
    set_run_override(move |_args, _check, _timeout| Ok(ok_run(0, sanitized, "")));
    assert_eq!(list_panes_all_status(), (None, "unknown"));
    assert_eq!(list_panes_full_or_none("t:1"), None);
    assert_eq!(list_panes_snapshot_status().1, "unknown");
    assert!(list_panes_snapshot_status().0.is_none());

    let readable = "%1\t[worker]\tnode\tagent\tworker\tt\tcodex\t\n";
    set_run_override(move |_args, _check, _timeout| Ok(ok_run(0, readable, "")));
    let (panes, status) = list_panes_all_status();
    assert_eq!(status, "ok");
    assert_eq!(panes.unwrap()[0].pane_id, "%1");
}

#[test]
fn test_tmux_client_gets_a_utf8_ctype_only_without_one() {
    let mut env = EnvGuard::cleared(&["LC_ALL", "LC_CTYPE", "LANG"]);
    let ctype_of = |cmd: &std::process::Command| -> (Option<String>, bool) {
        let envs: Vec<_> = cmd.get_envs().collect();
        let ctype = envs
            .iter()
            .find(|(k, _)| *k == "LC_CTYPE")
            .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()));
        let lc_all_removed = envs.iter().any(|(k, v)| *k == "LC_ALL" && v.is_none());
        (ctype, lc_all_removed)
    };

    // No locale at all: the C locale, so the client gets a UTF-8 one.
    let mut cmd = std::process::Command::new("tmux");
    utf8_client(&mut cmd);
    assert_eq!(ctype_of(&cmd), (Some("C.UTF-8".to_string()), true));

    // A UTF-8 locale in any of the three variables is left alone.
    for (key, value) in [
        ("LANG", "en_US.UTF-8"),
        ("LC_CTYPE", "C.utf8"),
        ("LC_ALL", "de_DE.UTF-8@euro"),
    ] {
        for other in ["LC_ALL", "LC_CTYPE", "LANG"] {
            env.remove(other);
        }
        env.set(key, value);
        let mut cmd = std::process::Command::new("tmux");
        utf8_client(&mut cmd);
        assert_eq!(ctype_of(&cmd), (None, false), "{key}={value}");
    }
    env.remove("LC_CTYPE");

    // LC_ALL wins over the others: a non-UTF-8 one is replaced.
    env.set("LANG", "en_US.UTF-8");
    env.set("LC_ALL", "C");
    let mut cmd = std::process::Command::new("tmux");
    utf8_client(&mut cmd);
    assert_eq!(ctype_of(&cmd), (Some("C.UTF-8".to_string()), true));

    assert!(locale_is_utf8("en_US.UTF-8"));
    assert!(locale_is_utf8("C.utf8"));
    assert!(!locale_is_utf8("C"));
    assert!(!locale_is_utf8(""));
    assert!(!locale_is_utf8("en_US.ISO8859-1"));
}

#[test]
fn test_pane_scan_status_keeps_permission_denied_unknown() {
    set_run_override(|_args, _check, _timeout| {
        Ok(ok_run(
            1,
            "",
            "error connecting to /private/tmp/tmux-501/default (Permission denied)",
        ))
    });
    assert_eq!(list_panes_all_status(), (None, "unknown"));
    assert_eq!(list_team_windows_status(), (None, "unknown"));
}

#[test]
fn test_team_window_scan_parses_pr_and_tolerates_short_lines() {
    set_run_override(|_args, _check, _timeout| {
        Ok(ok_run(
            0,
            // second line is an old 6-field line: pr backfills ""
            "dev:1\thive\t@1\t0-w2\t/tmp/ws\t100.0\t52\ndev:2\tother\t@2\t0-w9\t/tmp/w9\t50.0\n",
            "",
        ))
    });

    let (windows, status) = list_team_windows_status();
    assert_eq!(status, "ok");
    let windows = windows.unwrap();
    assert_eq!(windows[0].pr, "52");
    assert_eq!(windows[1].pr, "");
    assert_eq!(windows[1].team, "0-w9");
}

// --- facade-hygiene helpers (exact command contracts) ---------------------

#[test]
fn test_run_shell_detached_passes_command_byte_for_byte() {
    let calls = capture_run(0, "");
    let cmd = "sleep 0.2 && tmux send-keys -t '%9' Escape";
    run_shell_detached(cmd);
    assert_eq!(
        *calls.borrow(),
        vec![(v(&["run-shell", "-b", cmd]), false, 5)]
    );
}

#[test]
fn test_display_value_none_on_failure() {
    capture_run(1, "");
    assert_eq!(display_value("%5", "#{pane_left}"), None);
}

#[test]
fn test_own_socket_path_prefers_tmux_env_over_the_server() {
    let mut env = EnvGuard::new();
    env.set("TMUX", "/private/tmp/tmux-501/default,4242,0");
    set_run_override(|_args, _check, _timeout| panic!("must not ask the server"));
    assert_eq!(
        own_socket_path().as_deref(),
        Some("/private/tmp/tmux-501/default")
    );
}

#[test]
fn test_own_socket_path_asks_the_server_outside_tmux() {
    let _env = EnvGuard::cleared(&["TMUX"]);
    set_run_override(|args, _check, _timeout| {
        assert_eq!(args, v(&["display-message", "-p", "#{socket_path}"]));
        Ok(ok_run(0, "/x/tmux-501/e2e\n", ""))
    });
    assert_eq!(own_socket_path().as_deref(), Some("/x/tmux-501/e2e"));
    set_run_override(|_args, _check, _timeout| {
        Ok(ok_run(1, "", "no server running on /tmp/tmux-501/default"))
    });
    assert_eq!(own_socket_path(), None);
}

#[test]
fn test_same_socket_normalizes_private_tmp() {
    assert!(same_socket(
        "/private/tmp/tmux-501/default",
        "/tmp/tmux-501/default"
    ));
    assert!(same_socket("/x/tmux-501/e2e", "/x/tmux-501/e2e"));
    assert!(!same_socket("/x/tmux-501/e2e", "/tmp/tmux-501/default"));
    assert!(!same_socket("", ""));
    let default = default_socket_path();
    assert_eq!(default.file_name().unwrap(), "default");
    assert!(default.starts_with("/tmp"));
}

#[test]
fn test_pane_creation_passes_cwd_and_keeps_explicit_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_str().unwrap();
    let calls = capture_run(0, "%9");
    split_window("%1", true, None, true, Some(cwd)).unwrap();
    new_window("dev", "worker", Some(cwd), true, None).unwrap();
    new_session("dev", 100, 30, Some(cwd), None).unwrap();
    new_window("dev", "viewer", Some(cwd), true, Some("hive view session")).unwrap();
    new_session("dev", 100, 30, Some(cwd), Some("claude attach job")).unwrap();
    let calls = calls.borrow();
    for (index, (args, _, _)) in calls.iter().enumerate() {
        assert!(args.windows(2).any(|pair| pair == ["-c", cwd]));
        let last = args.last().map(String::as_str);
        match index {
            3 => assert_eq!(last, Some("hive view session")),
            4 => assert_eq!(last, Some("claude attach job")),
            _ => assert!(last == Some(cwd) || last.is_some_and(|arg| arg.starts_with("#{"))),
        }
    }
}

#[test]
fn test_pane_creation_rejects_unavailable_cwd_before_tmux() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("missing");
    let file = tmp.path().join("file");
    std::fs::write(&file, "").unwrap();
    let calls = capture_run(0, "%9");
    for cwd in ["", missing.to_str().unwrap(), file.to_str().unwrap()] {
        let mut results = vec![split_window("%1", true, None, true, Some(cwd))];
        for command in [None, Some("hive view session")] {
            results.push(new_session("dev", 100, 30, Some(cwd), command));
            results
                .push(new_window("dev", "worker", Some(cwd), true, command).map(|(_, pane)| pane));
        }
        for result in results {
            let error = result.unwrap_err();
            assert!(error.to_string().contains(&format!("{cwd:?}")));
            assert!(calls.borrow().is_empty());
        }
    }
}

#[test]
fn test_parse_panes_snapshot_reads_the_pane_info_and_its_extra_columns() {
    let (panes, extras) = parse_panes_snapshot(
        "%1\tt\tclaude\tagent\trex\tteam-a\tclaude\tg\tdev:1\t0\t/dev/ttys003\t501\t/tmp/a\n\
         %2\t\tzsh\t\t\t\t\t\tdev:2\t1\t\t\t\n",
    );
    assert_eq!(panes.len(), 2);
    assert_eq!(
        panes[0],
        PaneInfo {
            pane_id: "%1".into(),
            title: "t".into(),
            command: "claude".into(),
            role: "agent".into(),
            agent: "rex".into(),
            team: "team-a".into(),
            cli: "claude".into(),
            group: "g".into(),
        }
    );
    assert_eq!(
        extras["%1"],
        PaneExtra {
            window: "dev:1".into(),
            dead: false,
            tty: "/dev/ttys003".into(),
            pid: Some(501),
            cwd: "/tmp/a".into(),
        }
    );
    assert_eq!(
        extras["%2"],
        PaneExtra {
            window: "dev:2".into(),
            dead: true,
            tty: String::new(),
            pid: None,
            cwd: String::new(),
        }
    );
}

#[test]
fn test_parse_all_tty_processes_groups_by_tty_and_drops_ttyless_rows() {
    let by_tty = parse_all_tty_processes(
        "  501 ttys003 /usr/local/bin/claude claude --resume abc\n  502 ttys003 -zsh -zsh\n  700 ??   /sbin/launchd /sbin/launchd\n bad line\n",
    );
    assert_eq!(by_tty.len(), 1);
    let rows = &by_tty["ttys003"];
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].pid, "501");
    assert_eq!(rows[0].command, "/usr/local/bin/claude");
    assert_eq!(rows[0].argv, "claude --resume abc");
    assert_eq!(rows[1].argv, "-zsh");
    assert_eq!(tty_key("/dev/ttys003"), "ttys003");
    assert_eq!(tty_key("ttys003"), "ttys003");
}

#[test]
fn test_parse_windows_snapshot_reads_every_window_with_its_instance_tags() {
    let windows = parse_windows_snapshot(
        "dev:1\t@3\t$0\tdev\tfern\t/ws/fern\t1700000000.5\ttok-1\n\
         lane:0\t@7\t$4\tlane\t\t\t\t\n\
         \t@9\t$4\tlane\tfern\t/ws\t1\t\n",
    );
    assert_eq!(windows.len(), 2, "{windows:?}");
    let fern = &windows["dev:1"];
    assert_eq!(fern.window_id, "@3");
    assert_eq!(fern.session_id, "$0");
    assert_eq!(fern.session_name, "dev");
    assert_eq!(fern.team, "fern");
    assert_eq!(fern.workspace, "/ws/fern");
    assert_eq!(fern.created, "1700000000.5");
    assert_eq!(fern.token, "tok-1");
    let plain = &windows["lane:0"];
    assert_eq!(plain.window_id, "@7");
    assert_eq!(plain.team, "");
    assert_eq!(plain.token, "");
}

#[test]
fn test_windows_snapshot_lists_the_instance_tags_and_the_token_in_one_read() {
    let calls = capture_run(0, "dev:1\t@3\t$0\tdev\tfern\t/ws\t1\ttok\n");
    let (windows, status) = list_windows_snapshot_status("hive-notify-token");
    assert_eq!(status, "ok");
    assert_eq!(windows.unwrap()["dev:1"].token, "tok");
    let calls = calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0[..3], v(&["list-windows", "-a", "-F"])[..]);
    let fmt = &calls[0].0[3];
    assert!(fmt.contains(WINDOW_TEAM_FMT), "{fmt}");
    assert!(fmt.contains("#{session_id}"), "{fmt}");
    assert!(fmt.ends_with("#{@hive-notify-token}"), "{fmt}");
    capture_run(1, "");
    assert_eq!(
        list_windows_snapshot_status("hive-notify-token").1,
        "unknown"
    );
    raising_run();
    assert_eq!(
        list_windows_snapshot_status("hive-notify-token").1,
        "unknown"
    );
}

// --- wake hooks -----------------------------------------------------------

/// The hook environment a wake-hook test installs under: a fixed binary,
/// hive home, `HOME` and one engine home, no other engine home.
fn wake_env() -> EnvGuard {
    let mut env = EnvGuard::cleared(&[
        "CLAUDE_HOME",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "GROK_HOME",
    ]);
    env.set("HIVE_BIN", "/x/hive");
    env.set("HIVE_HOME", "/h/one");
    env.set("HOME", "/home/dp");
    env.set("CODEX_HOME", "/home/dp/.codex");
    env
}

/// The wake entry `wake_env`'s installer bakes for *home*, as tmux lists it.
fn wake_entry(home: &str) -> String {
    format!("run-shell -b \"HIVE_HOME={home} HOME=/home/dp CODEX_HOME=/home/dp/.codex /x/hive wake --session #{{q:session_id}} >/dev/null 2>&1 || true\"")
}

const OWN_WAKE: &str = "run-shell -b \"HIVE_HOME=/h/one HOME=/home/dp CODEX_HOME=/home/dp/.codex /x/hive wake --session #{q:session_id} >/dev/null 2>&1 || true\"";
const FOREIGN_WAKE: &str = "run-shell -b \"HIVE_HOME=/h/two HOME=/home/dp /y/hive wake --session #{q:session_id} >/dev/null 2>&1 || true\"";
const USER_ATTACH: &str = "run-shell \"touch /tmp/attached\"";
const USER_CHANGED: &str = "display-message hi";
/// Wake entries from before homes were baked in: this binary's and
/// another's. Neither names a home, so neither is any home's.
const LEGACY_THIS_BIN: &str = "run-shell -b \"/x/hive wake --window '#{q:session_name}:#{window_index}' >/dev/null 2>&1 || true\"";
const LEGACY_OTHER_BIN: &str = "run-shell -b \"/y/hive wake --window '#{q:session_name}:#{window_index}' >/dev/null 2>&1 || true\"";

type HookStore = Rc<RefCell<std::collections::BTreeMap<String, String>>>;

/// A tmux answering `show-hooks -t $3` from *entries* (`name[idx]` →
/// command, listed as tmux prints them) and applying every `set-hook` to
/// them, or refusing each `set-hook` when *refuse_set*; every other
/// command answers empty. Returns the store and the recorded calls.
fn hook_server(entries: &[(&str, &str)], refuse_set: bool) -> (HookStore, Calls) {
    let store: HookStore = Rc::new(RefCell::new(
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    ));
    let calls: Calls = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    let hooks = Rc::clone(&store);
    set_run_override(move |args, check, timeout| {
        recorded.borrow_mut().push((args.to_vec(), check, timeout));
        match args[0].as_str() {
            "show-hooks" => {
                assert_eq!(args[1..], ["-t", "$3"]);
                let listing: String = hooks
                    .borrow()
                    .iter()
                    .map(|(k, v)| format!("{k} {v}\n"))
                    .collect();
                Ok(ok_run(0, &listing, ""))
            }
            "set-hook" if refuse_set => Err(TmuxError::CalledProcess {
                returncode: 1,
                stderr: "refused".to_string(),
            }),
            "set-hook" if args[1] == "-u" => {
                assert_eq!(args[2..4], ["-t", "$3"]);
                hooks.borrow_mut().remove(&args[4]);
                Ok(ok_run(0, "", ""))
            }
            "set-hook" => {
                assert_eq!(args[1..3], ["-t", "$3"]);
                hooks.borrow_mut().insert(args[3].clone(), args[4].clone());
                Ok(ok_run(0, "", ""))
            }
            _ => Ok(ok_run(0, "", "")),
        }
    });
    (store, calls)
}

fn entries(store: &HookStore) -> Vec<(String, String)> {
    store
        .borrow()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[test]
fn test_wake_shell_line_quotes_argv_then_escapes_the_line_for_tmux() {
    let env = vec![
        ("HIVE_HOME".to_string(), "/h \"q\" $x".to_string()),
        ("HOME".to_string(), "/home/dp".to_string()),
    ];
    let line = wake_shell_line("/tmp/we ird$x/hi've", &env);
    // Each argv piece is shell-quoted on its own; the session id is tmux
    // format quoting, never shell quoting.
    assert_eq!(
        line,
        "HIVE_HOME='/h \"q\" $x' HOME=/home/dp '/tmp/we ird$x/hi'\"'\"'ve' wake --session #{q:session_id} >/dev/null 2>&1 || true"
    );
    // The whole line then gets tmux's double-quote escaping: `$` and `"`
    // in the path reach sh intact.
    let run = wake_run_shell(&line);
    assert_eq!(
        run,
        "run-shell -b \"HIVE_HOME='/h \\\"q\\\" \\$x' HOME=/home/dp '/tmp/we ird\\$x/hi'\\\"'\\\"'ve' wake --session #{q:session_id} >/dev/null 2>&1 || true\""
    );
    // tmux prints a stored entry back with the same escapes: the body
    // read off a listing is the line that was installed.
    assert_eq!(run_shell_body(&run), Some(line.clone()));
    assert_eq!(
        run_shell_body(&format!("\"{}\"", crate::shell::tmux_dquote_escape(&run))),
        Some(line)
    );
    assert_eq!(run_shell_body("display-message hi"), None);
    assert_eq!(run_shell_body("run-shell \"touch /tmp/x\""), None);
}

#[test]
fn test_wake_environment_bakes_the_absolute_hive_home_and_set_engine_homes() {
    let mut env = wake_env();
    assert_eq!(
        wake_environment(),
        vec![
            ("HIVE_HOME".to_string(), "/h/one".to_string()),
            ("HOME".to_string(), "/home/dp".to_string()),
            ("CODEX_HOME".to_string(), "/home/dp/.codex".to_string()),
        ]
    );
    // A relative or default home is baked resolved: the hook must name the
    // home it was installed for, not read one from the server's env.
    env.remove("HIVE_HOME");
    env.set("HOME", "/home/dp");
    assert_eq!(wake_environment()[0].1, "/home/dp/.hive");
    env.set("HIVE_HOME", "rel/home");
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(
        wake_environment()[0].1,
        cwd.join("rel/home").to_string_lossy()
    );
}

#[test]
fn test_parse_hook_entries_reads_indexed_and_plain_wake_hooks_only() {
    let listing = "client-attached[0] run-shell x\nclient-attached[3] display-message hi\nclient-session-changed run-shell y\nsession-created run-shell z\nbogus\n";
    assert_eq!(
        parse_hook_entries(listing),
        vec![
            HookEntry {
                hook: "client-attached".to_string(),
                index: 0,
                command: "run-shell x".to_string()
            },
            HookEntry {
                hook: "client-attached".to_string(),
                index: 3,
                command: "display-message hi".to_string()
            },
            HookEntry {
                hook: "client-session-changed".to_string(),
                index: 0,
                command: "run-shell y".to_string()
            },
        ]
    );
}

#[test]
fn test_install_wake_hooks_preserves_user_and_foreign_home_entries() {
    let _env = wake_env();
    let (store, calls) = hook_server(
        &[
            ("client-attached[0]", USER_ATTACH),
            ("client-attached[1]", FOREIGN_WAKE),
            ("client-session-changed[0]", USER_CHANGED),
        ],
        false,
    );

    // Three installs: one entry of this home's per hook, at the lowest
    // free index, the user's and the other home's bytes untouched.
    for _ in 0..3 {
        install_wake_hooks("$3").unwrap();
        assert_eq!(
            entries(&store),
            vec![
                ("client-attached[0]".to_string(), USER_ATTACH.to_string()),
                ("client-attached[1]".to_string(), FOREIGN_WAKE.to_string()),
                ("client-attached[2]".to_string(), OWN_WAKE.to_string()),
                (
                    "client-session-changed[0]".to_string(),
                    USER_CHANGED.to_string()
                ),
                (
                    "client-session-changed[1]".to_string(),
                    OWN_WAKE.to_string()
                ),
            ]
        );
    }
    // Every write is checked, and only this home's index is ever written.
    let writes: Vec<Vec<String>> = argvs(&calls)
        .into_iter()
        .filter(|a| a[0] == "set-hook")
        .collect();
    assert_eq!(writes.len(), 6);
    assert!(writes
        .iter()
        .all(|a| a[1] == "-t"
            && (a[3] == "client-attached[2]" || a[3] == "client-session-changed[1]")));
    assert!(calls.borrow().iter().all(|(_, check, _)| *check));
}

#[test]
fn test_install_and_remove_wake_hooks_recognise_their_entry_under_a_relative_hive_home() {
    let mut env = wake_env();
    env.set("HIVE_HOME", "rel/home");
    let (store, _calls) = hook_server(&[("client-attached[0]", USER_ATTACH)], false);

    // The entry is baked with the resolved home, and a second install
    // under the same relative spelling finds it instead of growing.
    install_wake_hooks("$3").unwrap();
    let after_first = entries(&store);
    assert_eq!(after_first.len(), 3);
    let baked = &after_first[1].1;
    let cwd = std::env::current_dir().unwrap();
    assert!(baked.contains(&cwd.join("rel/home").to_string_lossy().to_string()));
    install_wake_hooks("$3").unwrap();
    assert_eq!(entries(&store), after_first);

    // The remover recognises the same entry and leaves the user's alone.
    remove_wake_hooks("$3").unwrap();
    assert_eq!(
        entries(&store),
        vec![("client-attached[0]".to_string(), USER_ATTACH.to_string())]
    );
}

#[test]
fn test_install_wake_hooks_leaves_home_less_legacy_hooks_and_drops_its_own_duplicates() {
    let _env = wake_env();
    // Older installs (no home baked in, the whole array clobbered to
    // index 0) of this binary and of another are nobody's: both stay as
    // they are and this home takes the next free index. A second entry of
    // this home's is a leftover to drop.
    let (store, _calls) = hook_server(
        &[
            ("client-attached[0]", LEGACY_THIS_BIN),
            ("client-attached[1]", LEGACY_OTHER_BIN),
            ("client-session-changed[0]", OWN_WAKE),
            ("client-session-changed[2]", USER_CHANGED),
            ("client-session-changed[3]", OWN_WAKE),
        ],
        false,
    );

    install_wake_hooks("$3").unwrap();

    assert_eq!(
        entries(&store),
        vec![
            (
                "client-attached[0]".to_string(),
                LEGACY_THIS_BIN.to_string()
            ),
            (
                "client-attached[1]".to_string(),
                LEGACY_OTHER_BIN.to_string()
            ),
            ("client-attached[2]".to_string(), OWN_WAKE.to_string()),
            (
                "client-session-changed[0]".to_string(),
                OWN_WAKE.to_string()
            ),
            (
                "client-session-changed[2]".to_string(),
                USER_CHANGED.to_string()
            ),
        ]
    );
}

/// Two hive homes installed from one binary: the home-less legacy entry
/// that binary left names no home, so a second home neither claims it as
/// its own index nor removes it — its entry is created beside it, updated
/// in place, and removed alone.
#[test]
fn test_a_second_home_of_the_same_binary_leaves_the_binarys_legacy_hook_alone() {
    let mut env = wake_env();
    env.set("HIVE_HOME", "/h/two");
    let (store, calls) = hook_server(
        &[
            ("client-attached[0]", LEGACY_THIS_BIN),
            ("client-session-changed[0]", LEGACY_THIS_BIN),
        ],
        false,
    );
    let two = wake_entry("/h/two");

    for _ in 0..2 {
        install_wake_hooks("$3").unwrap();
        assert_eq!(
            entries(&store),
            vec![
                (
                    "client-attached[0]".to_string(),
                    LEGACY_THIS_BIN.to_string()
                ),
                ("client-attached[1]".to_string(), two.clone()),
                (
                    "client-session-changed[0]".to_string(),
                    LEGACY_THIS_BIN.to_string()
                ),
                ("client-session-changed[1]".to_string(), two.clone()),
            ]
        );
    }
    // Index 0 was never written: not to migrate it, not to unset it.
    assert!(argvs(&calls)
        .iter()
        .filter(|a| a[0] == "set-hook")
        .all(|a| a.iter().all(|arg| !arg.ends_with("[0]"))));

    remove_wake_hooks("$3").unwrap();
    assert_eq!(
        entries(&store),
        vec![
            (
                "client-attached[0]".to_string(),
                LEGACY_THIS_BIN.to_string()
            ),
            (
                "client-session-changed[0]".to_string(),
                LEGACY_THIS_BIN.to_string()
            ),
        ]
    );
}

#[test]
fn test_install_wake_hooks_reports_a_refused_write_and_an_unanswered_session() {
    let _env = wake_env();
    let (store, _calls) = hook_server(&[("client-attached[0]", USER_ATTACH)], true);
    let err = install_wake_hooks("$3").unwrap_err().to_string();
    assert!(err.contains("refused"), "{err}");
    assert_eq!(
        entries(&store),
        vec![("client-attached[0]".to_string(), USER_ATTACH.to_string())]
    );

    set_run_override(|args, _check, _timeout| {
        assert_eq!(args[0], "show-hooks");
        Err(TmuxError::CalledProcess {
            returncode: 1,
            stderr: "can't find session: $3".to_string(),
        })
    });
    assert!(install_wake_hooks("$3").is_err());
    assert!(install_wake_hooks("").is_err());
}

#[test]
fn test_remove_wake_hooks_unsets_only_this_homes_entries() {
    let _env = wake_env();
    let (store, calls) = hook_server(
        &[
            ("client-attached[0]", USER_ATTACH),
            ("client-attached[1]", FOREIGN_WAKE),
            ("client-attached[2]", OWN_WAKE),
            ("client-session-changed[0]", LEGACY_THIS_BIN),
            ("client-session-changed[1]", LEGACY_OTHER_BIN),
            ("client-session-changed[2]", OWN_WAKE),
        ],
        false,
    );

    remove_wake_hooks("$3").unwrap();

    assert_eq!(
        entries(&store),
        vec![
            ("client-attached[0]".to_string(), USER_ATTACH.to_string()),
            ("client-attached[1]".to_string(), FOREIGN_WAKE.to_string()),
            (
                "client-session-changed[0]".to_string(),
                LEGACY_THIS_BIN.to_string()
            ),
            (
                "client-session-changed[1]".to_string(),
                LEGACY_OTHER_BIN.to_string()
            ),
        ]
    );
    let unsets: Vec<Vec<String>> = argvs(&calls)
        .into_iter()
        .filter(|a| a[0] == "set-hook")
        .collect();
    assert_eq!(
        unsets,
        vec![
            v(&["set-hook", "-u", "-t", "$3", "client-attached[2]"]),
            v(&["set-hook", "-u", "-t", "$3", "client-session-changed[2]"]),
        ]
    );

    // A session that is gone has nothing to remove: no error, no write.
    let calls = capture_run(1, "");
    remove_wake_hooks("$3").unwrap();
    assert_eq!(argvs(&calls), vec![v(&["show-hooks", "-t", "$3"])]);
    remove_wake_hooks("").unwrap();
}
