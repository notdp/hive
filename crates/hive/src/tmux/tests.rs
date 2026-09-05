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

fn ok_run(returncode: i32, stdout: &str, stderr: &str) -> Run {
    Run {
        returncode,
        stdout: stdout.to_string(),
        stderr: stderr.to_string(),
    }
}

fn v(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

fn _timeout_run() {
    set_exec_override(|_argv, _timeout, _input| Err(TmuxError::Timeout));
}

fn _capture_run(rc: i32, out: &'static str) -> Calls {
    let calls: Calls = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    _set_run_override(move |args, check, timeout| {
        recorded.borrow_mut().push((args.to_vec(), check, timeout));
        Ok(ok_run(rc, out, ""))
    });
    calls
}

fn _raising_run() {
    _set_run_override(|_args, _check, _timeout| Err(TmuxError::Os("no tmux".to_string())));
}

fn is_timeout(err: &anyhow::Error) -> bool {
    matches!(err.downcast_ref::<TmuxError>(), Some(TmuxError::Timeout))
}

#[test]
fn test_run_probe_reads_timeout_as_unknown() {
    _timeout_run();

    let result = _run(&["list-panes"], false, 5).unwrap();

    assert_eq!(result.returncode, 1);
    assert_eq!(result.stderr, "timeout");
}

#[test]
fn test_run_timeout_raises_when_the_command_had_to_happen() {
    // check=true means the caller needs the command to have run: a busy tmux
    // server must not be able to fake a successful send-keys.
    _timeout_run();

    assert!(matches!(
        _run(&["list-panes"], true, 5),
        Err(TmuxError::Timeout)
    ));
    assert!(is_timeout(&send_keys("%1", "hello", true).unwrap_err()));
    assert!(is_timeout(&send_key("%1", "Escape").unwrap_err()));
}

#[test]
fn test_load_buffer_timeout_raises() {
    // A draft save that did not happen must not read as one — the caller
    // clears the pane's composer on the strength of this call.
    _timeout_run();

    assert!(is_timeout(
        &load_buffer("hive_draft_1", "unsent thought").unwrap_err()
    ));
}

#[test]
fn test_session_helpers_delegate_to_tmux() {
    let calls: Calls = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    _set_run_override(move |args, check, timeout| {
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
    assert_eq!(new_session("dev", 200, 50).unwrap(), "%9");
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
    let calls = _capture_run(0, "");

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
    let calls = _capture_run(0, "");

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
    _set_run_override(move |args, check, timeout| {
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
    _set_run_override(|args, _check, _timeout| {
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
    _set_run_override(|_args, _check, _timeout| Ok(ok_run(0, "%1 0\n%2 1\n", "")));

    assert!(is_pane_alive("%1"));
    assert!(!is_pane_alive("%2"));
    assert!(!is_pane_alive("%9"));
}

#[test]
fn test_is_pane_alive_treats_tmux_failure_as_alive() {
    _set_run_override(|_args, _check, _timeout| Ok(ok_run(1, "", "timeout")));

    assert!(is_pane_alive("%1"));
}

#[test]
fn test_context_helpers_use_environment_and_display_message() {
    let mut env = EnvGuard::new();
    env.set("TMUX", "/tmp/tmux-1");
    env.set("TMUX_PANE", "%7");
    env.remove("CODEX_THREAD_ID");
    env.remove("GROK_SESSION_ID");
    env.remove("CLAUDE_CODE_MESSAGING_SOCKET");
    _set_run_override(|args, _check, _timeout| {
        let stdout = if args.iter().any(|a| a == "#{session_name}:#{window_index}") {
            "dev:2\n"
        } else if args.iter().any(|a| a == "#{session_name}") {
            "dev\n"
        } else if args.iter().any(|a| a == "#{window_id}") {
            "@42\n"
        } else {
            "2\n"
        };
        Ok(ok_run(0, stdout, ""))
    });

    assert!(is_inside_tmux());
    assert_eq!(get_current_pane_id().as_deref(), Some("%7"));
    assert_eq!(get_current_window_target().as_deref(), Some("dev:2"));
    assert_eq!(get_current_session_name().as_deref(), Some("dev"));
    assert_eq!(get_current_window_id().as_deref(), Some("@42"));
    assert_eq!(get_window_id("dev:2").as_deref(), Some("@42"));
}

#[test]
fn test_client_mode_and_popup_support_helpers() {
    _set_run_override(|args, _check, _timeout| {
        let stdout = if args.iter().any(|a| a == "#{client_control_mode}") {
            "1\n"
        } else {
            ""
        };
        Ok(ok_run(0, stdout, ""))
    });

    assert_eq!(get_client_mode(Some("%7")), "control");
}

#[test]
fn test_client_mode_returns_terminal_or_unknown() {
    _set_run_override(|_args, _check, _timeout| Ok(ok_run(0, "0\n", "")));
    assert_eq!(get_client_mode(Some("%8")), "terminal");

    _set_run_override(|_args, _check, _timeout| Ok(ok_run(0, "", "")));
    assert_eq!(get_client_mode(Some("%8")), "unknown");
}

#[test]
fn test_client_window_helpers_resolve_most_recent_client() {
    _set_run_override(|args, _check, _timeout| {
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

#[test]
fn test_client_helpers_ignore_control_mode_clients() {
    _set_run_override(|args, _check, _timeout| {
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
fn test_current_window_helpers_return_none_without_tmux_pane() {
    let mut env = EnvGuard::new();
    env.remove("TMUX_PANE");
    env.remove("TMUX");
    env.remove("CODEX_THREAD_ID");
    env.remove("GROK_SESSION_ID");
    env.remove("CLAUDE_CODE_MESSAGING_SOCKET");

    assert_eq!(get_current_window_target(), None);
    assert_eq!(get_current_session_name(), None);
    assert_eq!(get_current_window_id(), None);
}

#[test]
fn test_grok_session_resolves_the_members_tagged_pane() {
    // A grok member's tools carry GROK_SESSION_ID and nothing else — no
    // $TMUX, no TMUX_PANE (the leader is minted by identity before any
    // pane exists). The id keys the member's roster row, and the pane
    // tagged with that team and name is its display.
    let tmp = tempfile::TempDir::new().unwrap();
    let mut env = EnvGuard::cleared(&crate::testenv::IDENTITY_VARS);
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    let mut member = serde_json::Map::new();
    member.insert(
        "name".to_string(),
        serde_json::Value::String("rex".to_string()),
    );
    member.insert(
        "cli".to_string(),
        serde_json::Value::String("grok".to_string()),
    );
    member.insert(
        "sessionId".to_string(),
        serde_json::Value::String("s-rex".to_string()),
    );
    assert_eq!(
        crate::registry::record_team("honey", "/tmp/ws-h", "1.0", &[member], "").unwrap(),
        "written"
    );
    env.set("GROK_SESSION_ID", "s-rex");
    let calls = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    _set_run_override(move |args, _check, _timeout| {
        recorded.borrow_mut().push(args.to_vec());
        let stdout = if args.first().map(String::as_str) == Some("list-panes") {
            concat!(
                "%3\t[orch]\tclaude\tagent\torch\thoney\tclaude\t\n",
                "%5\t[rex]\tgrok\tagent\trex\thoney\tgrok\t\n",
                "%8\t[rex]\tgrok\tagent\trex\twasp\tgrok\t\n"
            )
        } else if args.iter().any(|a| a == "#{window_id}") {
            "@4\n"
        } else {
            ""
        };
        Ok(ok_run(0, stdout, ""))
    });

    assert!(is_inside_tmux());
    assert_eq!(get_current_pane_id().as_deref(), Some("%5"));
    assert_eq!(get_current_window_id().as_deref(), Some("@4"));
    assert!(calls
        .borrow()
        .iter()
        .all(|args| args[0] == "list-panes" || args[..3] == v(&["display-message", "-t", "%5"])));

    // the same id on a claude row is a stranger, and no pane is anyone's
    let mut stranger = serde_json::Map::new();
    stranger.insert(
        "name".to_string(),
        serde_json::Value::String("rex".to_string()),
    );
    stranger.insert(
        "cli".to_string(),
        serde_json::Value::String("claude".to_string()),
    );
    stranger.insert(
        "sessionId".to_string(),
        serde_json::Value::String("s-rex".to_string()),
    );
    crate::registry::record_team("honey", "/tmp/ws-h", "1.0", &[stranger], "").unwrap();
    assert!(!is_inside_tmux());
    assert_eq!(get_current_pane_id(), None);
}

#[test]
fn test_grok_member_without_a_pane_keeps_its_identity_but_no_display() {
    // The member's window is gone: the roster still names it (the session
    // rung answers hive send), but there is no pane to act on.
    let tmp = tempfile::TempDir::new().unwrap();
    let mut env = EnvGuard::cleared(&crate::testenv::IDENTITY_VARS);
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    let mut member = serde_json::Map::new();
    member.insert(
        "name".to_string(),
        serde_json::Value::String("rex".to_string()),
    );
    member.insert(
        "cli".to_string(),
        serde_json::Value::String("grok".to_string()),
    );
    member.insert(
        "sessionId".to_string(),
        serde_json::Value::String("s-rex".to_string()),
    );
    crate::registry::record_team("honey", "/tmp/ws-h", "1.0", &[member], "").unwrap();
    env.set("GROK_SESSION_ID", "s-rex");
    _set_run_override(|_args, _check, _timeout| Ok(ok_run(0, "", "")));

    assert!(!is_inside_tmux());
    assert_eq!(get_current_pane_id(), None);
    assert_eq!(
        crate::registry::member_for_session("s-rex", Some("grok")),
        Some(("honey".to_string(), "rex".to_string()))
    );
}

#[test]
fn test_list_panes_full_parses_rows() {
    _set_run_override(|args, _check, _timeout| {
        let fmt = args.last().map(String::as_str).unwrap_or("");
        let stdout = if fmt == _PANE_BASE_FMT {
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
    _set_run_override(move |args, check, timeout| {
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
    _set_run_override(move |args, check, timeout| {
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
    let calls = _capture_run(0, "");

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
            _HIVE_PANE_BORDER_FORMAT,
        ])
    );
    assert!(!_HIVE_PANE_BORDER_FORMAT.contains("#[fg=colour220,bold]"));
    assert!(_HIVE_PANE_BORDER_FORMAT.contains("#[fg=colour220]#[bold][!]"));
}

#[test]
fn test_configure_hive_window_disables_native_tmux_alerts() {
    let calls = _capture_run(0, "");

    configure_hive_window("dev:1");

    let argvs: Vec<Vec<String>> = calls.borrow().iter().map(|c| c.0.clone()).collect();
    assert_eq!(
        argvs,
        vec![
            mirror_click_binding(),
            v(&[
                "set-window-option",
                "-t",
                "dev:1",
                "pane-border-status",
                "top"
            ]),
            v(&[
                "set-window-option",
                "-t",
                "dev:1",
                "pane-border-format",
                _HIVE_PANE_BORDER_FORMAT,
            ]),
            v(&[
                "set-window-option",
                "-t",
                "dev:1",
                "monitor-activity",
                "off"
            ]),
            v(&["set-window-option", "-t", "dev:1", "monitor-bell", "off"]),
        ]
    );
}

#[test]
fn test_mirror_click_binding_keeps_the_stock_click_as_its_else_branch() {
    let binding = mirror_click_binding();
    assert_eq!(
        binding[..4],
        v(&["bind-key", "-T", "root", "MouseDown1Pane"])[..]
    );
    assert_eq!(binding[4..8], v(&["if-shell", "-F", "-t", "="])[..]);
    assert!(binding[8].contains("@hive-role"), "{}", binding[8]);
    assert!(binding[8].contains("window_zoomed_flag"), "{}", binding[8]);
    // The rail branch is the toggle nested as one quoted command, selected
    // first, and never `send-keys`: the viewer must not receive the click
    // that resized it. Byte-exact — tmux parses the nested quoting.
    assert_eq!(
        binding[9],
        "select-pane -t = ; 'if-shell' '-F' '-t' '=' '#{e|>:#{pane_width},14}' \
         'resize-pane -t = -x 14' 'resize-pane -t = -x 45%'"
    );
    assert_eq!(binding[10], _STOCK_CLICK);
    assert_eq!(_STOCK_CLICK, "select-pane -t = ; send-keys -M");
    assert_eq!(binding.len(), 11);
}

#[test]
fn test_rail_toggle_argv_folds_above_the_rail_width() {
    assert_eq!(
        rail_toggle_argv("%9"),
        v(&[
            "if-shell",
            "-F",
            "-t",
            "%9",
            "#{e|>:#{pane_width},14}",
            "resize-pane -t %9 -x 14",
            "resize-pane -t %9 -x 45%",
        ])
    );
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
    let monitor = ControlModeOutputMonitor::new("613");
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

    assert!(!_control_mode_payload_has_activity(repaint));
}

#[test]
fn test_control_mode_payload_activity_accepts_visible_text_inside_styles() {
    assert!(_control_mode_payload_has_activity("\x1b[2mhello\x1b[0m"));
}

#[test]
fn test_control_mode_payload_activity_keeps_text_between_st_terminated_osc_sequences() {
    let payload = "\x1b]0;a\x1b\\hello\x1b]0;b\x1b\\";

    assert!(_control_mode_payload_has_activity(payload));
}

#[test]
fn test_control_mode_payload_activity_ignores_pure_dcs_sequence() {
    assert!(!_control_mode_payload_has_activity(
        "\x1bP1;2;3payload\x1b\\"
    ));
}

#[test]
fn test_control_mode_payload_activity_accepts_visible_text_between_dcs_and_osc() {
    let payload = "\x1bPignored\x1b\\hello\x1b]0;title\x1b\\";

    assert!(_control_mode_payload_has_activity(payload));
}

#[test]
fn test_control_mode_monitor_ignores_repaint_only_output() {
    // Repaint-only control sequences never mark a pane busy; the monitor
    // keeps no payload buffer (the pane-content msgId oracle is gone — delivery
    // confirmation is transcript-only).
    let monitor = ControlModeOutputMonitor::new("613");
    let payload = "\x1b[?2026h\x1b[49;2H\x1b[K\x1b[?2026l";

    monitor._record_control_mode_output("%9", payload);

    assert!(!monitor.is_busy("%9", 3.0));
}

#[test]
fn test_control_mode_monitor_marks_visible_text_busy() {
    let monitor = ControlModeOutputMonitor::new("613");

    monitor._record_control_mode_output("%9", "\x1b[2mhello\x1b[0m");

    assert!(monitor.is_busy("%9", 3.0));
}

#[test]
fn test_window_option_helpers() {
    let calls = _capture_run(0, "");

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
    let calls = _capture_run(0, "  #I #W  \n");

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
    _set_run_override(|_args, _check, _timeout| Ok(ok_run(0, "\n", "")));
    assert_eq!(get_global_window_option("window-status-format"), None);
}

#[test]
fn test_list_panes_full_or_none_is_status_aware() {
    _set_run_override(|_args, _check, _timeout| {
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

    _set_run_override(|_args, _check, _timeout| Ok(ok_run(1, "", "timeout")));
    assert_eq!(list_panes_full_or_none("dev:0"), None);
    assert_eq!(list_panes_full("dev:0"), Vec::new());

    _set_run_override(|_args, _check, _timeout| Ok(ok_run(0, "", "")));
    assert_eq!(list_panes_full_or_none("dev:0"), Some(Vec::new()));
}

#[test]
fn test_pane_scan_status_maps_no_server_variants() {
    for stderr in [
        "no server running on /tmp/tmux-501/default",
        "error connecting to /x/tmux-501/default (No such file or directory)",
    ] {
        _set_run_override(move |_args, _check, _timeout| Ok(ok_run(1, "", stderr)));
        assert_eq!(list_panes_all_status(), (None, "no-server"));
        assert_eq!(list_team_windows_status(), (None, "no-server"));
    }

    _set_run_override(|_args, _check, _timeout| Ok(ok_run(1, "", "timeout")));
    assert_eq!(list_panes_all_status(), (None, "unknown"));
    assert_eq!(list_team_windows_status(), (None, "unknown"));
}

#[test]
fn test_pane_scan_status_keeps_permission_denied_unknown() {
    _set_run_override(|_args, _check, _timeout| {
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
    _set_run_override(|_args, _check, _timeout| {
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
fn test_window_exists_requires_exact_id_echo() {
    let calls = _capture_run(0, "@7\n");
    assert!(window_exists("@7"));
    let calls = calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0],
        (
            v(&["display-message", "-t", "@7", "-p", "#{window_id}"]),
            false,
            5
        )
    );
}

#[test]
fn test_window_exists_false_paths() {
    let calls = _capture_run(0, "@8\n");
    assert!(!window_exists("")); // no subprocess for empty id
    assert!(calls.borrow().is_empty());
    assert!(!window_exists("@7")); // mismatched id
    _capture_run(1, "@7\n");
    assert!(!window_exists("@7")); // nonzero exit
    _raising_run();
    assert!(!window_exists("@7")); // missing binary never raises
}

#[test]
fn test_display_popup_preserves_argv_order_and_never_raises() {
    let calls = _capture_run(0, "");
    display_popup(
        "%5",
        "run-me",
        "/dev/ttys001",
        "#{popup_pane_left}",
        "#{popup_pane_top}",
        "40",
        "20",
        true,
        true,
        5,
    );
    assert_eq!(
        *calls.borrow(),
        vec![(
            v(&[
                "display-popup",
                "-c",
                "/dev/ttys001",
                "-t",
                "%5",
                "-B",
                "-x",
                "#{popup_pane_left}",
                "-y",
                "#{popup_pane_top}",
                "-w",
                "40",
                "-h",
                "20",
                "-E",
                "run-me",
            ]),
            false,
            5
        )]
    );
    _raising_run();
    display_popup("%5", "run-me", "", "", "", "", "", false, false, 5); // non-raising
}

#[test]
fn test_display_popup_omits_optional_flags() {
    let calls = _capture_run(0, "");
    display_popup("%5", "run-me", "", "", "", "", "", false, false, 5);
    assert_eq!(
        *calls.borrow(),
        vec![(v(&["display-popup", "-t", "%5", "run-me"]), false, 5)]
    );
}

#[test]
fn test_run_shell_detached_passes_command_byte_for_byte() {
    let calls = _capture_run(0, "");
    let cmd = "sleep 0.2 && tmux send-keys -t '%9' Escape";
    run_shell_detached(cmd);
    assert_eq!(
        *calls.borrow(),
        vec![(v(&["run-shell", "-b", cmd]), false, 5)]
    );
}

#[test]
fn test_source_file_bool_contract() {
    let calls = _capture_run(0, "");
    assert!(source_file("/x/enable.conf"));
    assert_eq!(
        *calls.borrow(),
        vec![(v(&["source-file", "/x/enable.conf"]), false, 5)]
    );
    _capture_run(1, "");
    assert!(!source_file("/x/enable.conf"));
    _raising_run();
    assert!(!source_file("/x/enable.conf"));
}

#[test]
fn test_display_value_none_on_failure() {
    _capture_run(1, "");
    assert_eq!(display_value("%5", "#{pane_left}"), None);
}
