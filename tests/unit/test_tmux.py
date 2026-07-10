import subprocess
import time

from hive import tmux


def test_run_returns_timeout_completed_process(monkeypatch):
    def _boom(*_args, **_kwargs):
        raise subprocess.TimeoutExpired(cmd=["tmux"], timeout=5)

    monkeypatch.setattr("hive.tmux.subprocess.run", _boom)

    result = tmux._run(["list-panes"])

    assert result.returncode == 1
    assert result.stderr == "timeout"


def test_session_helpers_delegate_to_tmux(monkeypatch):
    calls = []

    def _fake_run(args, check=True, timeout=5):
        calls.append((tuple(args), check, timeout))
        if args[0] == "has-session":
            return subprocess.CompletedProcess(["tmux", *args], 0, "", "")
        if args[0] == "new-session":
            return subprocess.CompletedProcess(["tmux", *args], 0, "%9\n", "")
        return subprocess.CompletedProcess(["tmux", *args], 0, "", "")

    monkeypatch.setattr("hive.tmux._run", _fake_run)

    assert tmux.has_session("dev") is True
    assert tmux.new_session("dev") == "%9"
    tmux.kill_session("dev")

    assert calls[0][0][:3] == ("has-session", "-t", "dev")
    assert calls[1][0][0] == "new-session"
    assert calls[2][0] == ("kill-session", "-t", "dev")


def test_send_keys_and_send_key_issue_expected_tmux_commands(monkeypatch):
    calls = []
    monkeypatch.setattr("hive.tmux._run", lambda args, check=True, timeout=5: calls.append((tuple(args), check)) or subprocess.CompletedProcess(["tmux", *args], 0, "", ""))

    tmux.send_keys("%1", "hello")
    tmux.send_keys("%2", "raw", enter=False)
    tmux.send_key("%3", "Escape")

    assert calls == [
        (("send-keys", "-t", "%1", "-l", "hello"), True),
        (("send-keys", "-t", "%1", "Enter"), True),
        (("send-keys", "-t", "%2", "-l", "raw"), True),
        (("send-keys", "-t", "%3", "Escape"), True),
    ]


def test_pane_mode_helpers_use_tmux_display_and_copy_mode(monkeypatch):
    calls = []

    def _fake_run(args, check=True, timeout=5):
        calls.append((tuple(args), check))
        stdout = "1\n" if args[:3] == ["display-message", "-t", "%1"] else ""
        return subprocess.CompletedProcess(["tmux", *args], 0, stdout, "")

    monkeypatch.setattr("hive.tmux._run", _fake_run)

    assert tmux.is_pane_in_mode("%1") is True
    tmux.cancel_pane_mode("%1")

    assert calls == [
        (("display-message", "-t", "%1", "-p", "#{pane_in_mode}"), False),
        (("copy-mode", "-q", "-t", "%1"), False),
    ]


def test_capture_and_list_parsers(monkeypatch):
    monkeypatch.setattr(
        "hive.tmux._run",
        lambda args, check=True, timeout=5: subprocess.CompletedProcess(["tmux", *args], 0, "%1\n%2\n" if "#{pane_id}" in args else "", ""),
    )
    monkeypatch.setattr("hive.tmux._run_output", lambda args: "line1\nline2")

    assert tmux.capture_pane("%1", 5) == "line1\nline2"
    assert tmux.list_panes("dev:0") == ["%1", "%2"]


def test_is_pane_alive_parses_tmux_output(monkeypatch):
    monkeypatch.setattr(
        "hive.tmux._run",
        lambda args, check=False, timeout=5: subprocess.CompletedProcess(["tmux", *args], 0, "%1 0\n%2 1\n", ""),
    )

    assert tmux.is_pane_alive("%1") is True
    assert tmux.is_pane_alive("%2") is False
    assert tmux.is_pane_alive("%9") is False


def test_is_pane_alive_treats_tmux_failure_as_alive(monkeypatch):
    monkeypatch.setattr(
        "hive.tmux._run",
        lambda args, check=False, timeout=5: subprocess.CompletedProcess(["tmux", *args], 1, "", "timeout"),
    )

    assert tmux.is_pane_alive("%1") is True


def test_context_helpers_use_environment_and_display_message(monkeypatch):
    monkeypatch.setenv("TMUX", "/tmp/tmux-1")
    monkeypatch.setenv("TMUX_PANE", "%7")
    monkeypatch.setattr(
        "hive.tmux._run",
        lambda args, check=False, timeout=5: subprocess.CompletedProcess(
            ["tmux", *args],
            0,
            (
                "dev:2\n" if "#{session_name}:#{window_index}" in args else (
                    "dev\n" if "#{session_name}" in args else (
                        "@42\n" if "#{window_id}" in args else "2\n"
                    )
                )
            ),
            "",
        ),
    )

    assert tmux.is_inside_tmux() is True
    assert tmux.get_current_pane_id() == "%7"
    assert tmux.get_current_window_target() == "dev:2"
    assert tmux.get_current_session_name() == "dev"
    assert tmux.get_current_window_index() == "2"
    assert tmux.get_current_window_id() == "@42"
    assert tmux.get_window_id("dev:2") == "@42"


def test_client_mode_and_popup_support_helpers(monkeypatch):
    monkeypatch.setattr(
        "hive.tmux._run",
        lambda args, check=False, timeout=5: subprocess.CompletedProcess(
            ["tmux", *args],
            0,
            "display-popup\n" if args[0] == "list-commands" else ("1\n" if "#{client_control_mode}" in args else ""),
            "",
        ),
    )
    monkeypatch.setenv("TMUX_PANE", "%7")

    assert tmux.get_client_mode("%7") == "control"
    assert tmux.is_control_mode_client("%7") is True


def test_client_mode_returns_terminal_or_unknown(monkeypatch):
    monkeypatch.setattr("hive.tmux.display_value", lambda _target, _fmt: "0")
    assert tmux.get_client_mode("%8") == "terminal"
    assert tmux.is_control_mode_client("%8") is False

    monkeypatch.setattr("hive.tmux.display_value", lambda _target, _fmt: None)
    assert tmux.get_client_mode("%8") == "unknown"


def test_client_window_helpers_resolve_most_recent_client(monkeypatch):
    def _fake_run(args, check=False, timeout=5):
        if args[0] == "list-clients":
            return subprocess.CompletedProcess(
                ["tmux", *args],
                0,
                "10\t0\t%1\t/dev/ttys010\n50\t0\t%5\t/dev/ttys050\n",
                "",
            )
        return subprocess.CompletedProcess(["tmux", *args], 0, "dev:5\n", "")

    monkeypatch.setattr("hive.tmux._run", _fake_run)

    assert tmux.get_most_recent_client_tty("dev") == "/dev/ttys050"
    assert tmux.get_most_recent_terminal_client_pane("dev") == "%5"
    assert tmux.get_client_window_target("/dev/ttys050") == "dev:5"
    assert tmux.get_most_recent_client_window("dev") == "dev:5"


def test_client_helpers_ignore_control_mode_clients(monkeypatch):
    def _fake_run(args, check=False, timeout=5):
        if args[0] == "list-clients":
            return subprocess.CompletedProcess(
                ["tmux", *args],
                0,
                "99\t1\t%control\t/dev/ttys999\n20\t0\t%term\t/dev/ttys020\n",
                "",
            )
        return subprocess.CompletedProcess(["tmux", *args], 0, "dev:2\n", "")

    monkeypatch.setattr("hive.tmux._run", _fake_run)

    assert tmux.get_most_recent_client_tty("dev") == "/dev/ttys020"
    assert tmux.get_most_recent_terminal_client_pane("dev") == "%term"
    assert tmux.get_most_recent_client_window("dev") == "dev:2"


def test_list_tty_processes_and_commands_strip_dev_prefix_and_parse_output(monkeypatch):
    calls = []

    def _fake_run(args, capture_output=True, text=True, check=False, timeout=5):
        calls.append(tuple(args))
        return subprocess.CompletedProcess(args, 0, "35214 -zsh -zsh\n35988 claude claude --verbose\n", "")

    monkeypatch.setattr("hive.tmux.subprocess.run", _fake_run)

    processes = tmux.list_tty_processes("/dev/ttys012")
    assert processes == [
        tmux.TTYProcessInfo(pid="35214", command="-zsh", argv="-zsh"),
        tmux.TTYProcessInfo(pid="35988", command="claude", argv="claude --verbose"),
    ]
    assert tmux.list_tty_commands("/dev/ttys012") == ["-zsh", "claude"]
    assert calls == [
        ("ps", "-t", "ttys012", "-o", "pid=,comm=,command="),
        ("ps", "-t", "ttys012", "-o", "pid=,comm=,command="),
    ]


def test_list_open_files_uses_native_proc_info(monkeypatch):
    monkeypatch.setattr("hive.proc_info.list_open_files", lambda _pid: ["/tmp/native.jsonl"])
    monkeypatch.setattr(
        "hive.tmux.subprocess.run",
        lambda *_a, **_kw: (_ for _ in ()).throw(AssertionError("lsof should not run")),
    )

    assert tmux.list_open_files("123") == ["/tmp/native.jsonl"]


def test_list_open_files_falls_back_to_lsof(monkeypatch):
    from hive import proc_info

    def _native(_pid):
        raise proc_info.ProcInfoUnavailable("no native backend")

    monkeypatch.setattr("hive.proc_info.list_open_files", _native)
    monkeypatch.setattr(
        "hive.tmux.subprocess.run",
        lambda args, capture_output=True, text=True, check=False, timeout=5: subprocess.CompletedProcess(
            args,
            0,
            "p123\nn/tmp/fallback.jsonl\nn/dev/null\n",
            "",
        ),
    )

    assert tmux.list_open_files("123") == ["/tmp/fallback.jsonl", "/dev/null"]


def test_current_window_helpers_return_none_without_tmux_pane(monkeypatch):
    monkeypatch.delenv("TMUX_PANE", raising=False)

    assert tmux.get_current_window_target() is None
    assert tmux.get_current_session_name() is None
    assert tmux.get_current_window_index() is None
    assert tmux.get_current_window_id() is None


def test_list_panes_with_titles_and_full_parse_rows(monkeypatch):
    outputs = {
        "#{pane_id}\t#{pane_title}": "%1\tmain\n%2\tworker\n",
        tmux._PANE_BASE_FMT: (
            "%1\tmain\tcodex\tagent\tclaude\tteam-a\t\n"
            "%2\tshell\tzsh\tterminal\tterm-1\tteam-a\t\n"
        ),
    }

    def _fake_run(args, check=False, timeout=5):
        fmt = args[-1]
        return subprocess.CompletedProcess(["tmux", *args], 0, outputs[fmt], "")

    monkeypatch.setattr("hive.tmux._run", _fake_run)

    titled = tmux.list_panes_with_titles("dev:0")
    full = tmux.list_panes_full("dev:0")

    assert titled == [tmux.PaneInfo("%1", "main"), tmux.PaneInfo("%2", "worker")]
    assert full[0] == tmux.PaneInfo("%1", "main", "codex", "agent", "claude", "team-a")
    assert full[1].role == "terminal"


def test_pane_option_helpers_and_tagging(monkeypatch):
    calls = []

    def _fake_run(args, check=False, timeout=5):
        calls.append(tuple(args))
        stdout = "value\n" if args[0] == "show-options" else ""
        return subprocess.CompletedProcess(["tmux", *args], 0, stdout, "")

    monkeypatch.setattr("hive.tmux._run", _fake_run)

    tmux.set_pane_option("%1", "hive-role", "agent")
    assert tmux.get_pane_option("%1", "hive-role") == "value"
    tmux.clear_pane_option("%1", "hive-role")
    tmux.tag_pane("%1", "agent", "claude", "team-a")
    tmux.clear_pane_tags("%1")

    assert calls[0] == ("set-option", "-p", "-t", "%1", "@hive-role", "agent")
    assert calls[1] == ("show-options", "-p", "-v", "-t", "%1", "@hive-role")
    assert calls[2] == ("set-option", "-p", "-t", "%1", "-u", "@hive-role")
    assert ("set-option", "-p", "-t", "%1", "@hive-agent", "claude") in calls
    assert ("set-option", "-p", "-t", "%1", "-u", "@hive-team") in calls


def test_enable_pane_border_status_uses_hive_member_format(monkeypatch):
    calls = []

    def _fake_run(args, check=False, timeout=5):
        calls.append(tuple(args))
        return subprocess.CompletedProcess(["tmux", *args], 0, "", "")

    monkeypatch.setattr("hive.tmux._run", _fake_run)

    tmux.enable_pane_border_status("dev:1")

    assert calls[0] == ("set-window-option", "-t", "dev:1", "pane-border-status", "top")
    assert calls[1] == (
        "set-window-option", "-t", "dev:1", "pane-border-format", tmux._HIVE_PANE_BORDER_FORMAT
    )
    assert "#[fg=colour220,bold]" not in tmux._HIVE_PANE_BORDER_FORMAT
    assert "#[fg=colour220]#[bold][!]" in tmux._HIVE_PANE_BORDER_FORMAT


def test_configure_hive_window_disables_native_tmux_alerts(monkeypatch):
    calls = []

    def _fake_run(args, check=False, timeout=5):
        calls.append(tuple(args))
        return subprocess.CompletedProcess(["tmux", *args], 0, "", "")

    monkeypatch.setattr("hive.tmux._run", _fake_run)

    tmux.configure_hive_window("dev:1")

    assert calls == [
        ("set-window-option", "-t", "dev:1", "pane-border-status", "top"),
        ("set-window-option", "-t", "dev:1", "pane-border-format", tmux._HIVE_PANE_BORDER_FORMAT),
        ("set-window-option", "-t", "dev:1", "monitor-activity", "off"),
        ("set-window-option", "-t", "dev:1", "monitor-bell", "off"),
    ]


def test_parse_control_mode_output_pane_matches_output_notifications():
    assert tmux.parse_control_mode_output_pane("%output %2772 hello") == "%2772"
    assert tmux.parse_control_mode_output_pane("%extended-output %2773 12 : world") == "%2773"
    assert tmux.parse_control_mode_output_pane("%session-changed $1 dev") is None


def test_control_mode_monitor_is_busy_uses_threshold():
    monitor = tmux.ControlModeOutputMonitor("613")
    monitor._last_output_at["%9"] = time.monotonic() - 1.0
    assert monitor.is_busy("%9", threshold_seconds=3.0) is True
    monitor._last_output_at["%9"] = time.monotonic() - 4.0
    assert monitor.is_busy("%9", threshold_seconds=3.0) is False


def test_control_mode_payload_activity_ignores_pure_repaint_sequence():
    repaint = (
        "\033[?2026h"
        "\033[49;2H\033[0m\033[49m\033[K"
        "\033[50;2H\033[0m\033[48;2;244;244;244m\033[K"
        "\033[51;28H\033[0m\033[48;2;244;244;244m\033[K"
        "\033[52;2H\033[0m\033[48;2;244;244;244m\033[K"
        "\033[53;52H\033[0m\033[49m\033[K"
        "\033[39m\033[49m\033[0m\033[?25h\033[51;3H\033[?2026l"
    )

    assert tmux._control_mode_payload_has_activity(repaint) is False


def test_control_mode_payload_activity_accepts_visible_text_inside_styles():
    assert tmux._control_mode_payload_has_activity("\033[2mhello\033[0m") is True


def test_control_mode_payload_activity_keeps_text_between_st_terminated_osc_sequences():
    payload = "\033]0;a\033\\hello\033]0;b\033\\"

    assert tmux._control_mode_payload_has_activity(payload) is True


def test_control_mode_payload_activity_ignores_pure_dcs_sequence():
    assert tmux._control_mode_payload_has_activity("\033P1;2;3payload\033\\") is False


def test_control_mode_payload_activity_accepts_visible_text_between_dcs_and_osc():
    payload = "\033Pignored\033\\hello\033]0;title\033\\"

    assert tmux._control_mode_payload_has_activity(payload) is True


def test_control_mode_monitor_keeps_buffer_but_does_not_mark_repaint_busy():
    monitor = tmux.ControlModeOutputMonitor("613")
    payload = "\033[?2026h\033[49;2H\033[K\033[?2026l"

    monitor._record_control_mode_output("%9", payload)

    assert monitor.is_busy("%9", threshold_seconds=3.0) is False
    assert monitor._output_buffer["%9"] == payload


def test_control_mode_monitor_marks_visible_text_busy():
    monitor = tmux.ControlModeOutputMonitor("613")

    monitor._record_control_mode_output("%9", "\033[2mhello\033[0m")

    assert monitor.is_busy("%9", threshold_seconds=3.0) is True


def test_window_option_helpers_and_flash(monkeypatch):
    calls = []

    def _fake_run(args, check=False, timeout=5):
        calls.append(tuple(args))
        return subprocess.CompletedProcess(["tmux", *args], 0, "", "")

    monkeypatch.setattr("hive.tmux._run", _fake_run)

    tmux.set_window_option("dev:1", "window-status-style", "fg=red")
    tmux.clear_window_option("dev:1", "window-status-style")
    tmux.flash_window_status("dev:1", seconds=3)

    assert calls[0] == ("set-window-option", "-t", "dev:1", "window-status-style", "fg=red")
    assert calls[1] == ("set-window-option", "-t", "dev:1", "-u", "window-status-style")
    assert calls[2][0:2] == ("run-shell", "-b")
    assert "window-status-style" in calls[2][2]
    assert "dev:1" in calls[2][2]
    assert calls[2][2].count("sleep 0.5") == 6


def test_get_global_window_option_is_read_only_global_scope(monkeypatch):
    calls = []

    def _fake_run(args, check=True, timeout=5):
        calls.append((tuple(args), check))
        return subprocess.CompletedProcess(["tmux", *args], 0, "  #I #W  \n", "")

    monkeypatch.setattr("hive.tmux._run", _fake_run)

    value = tmux.get_global_window_option("window-status-format")

    # Read-only `show-options -w -g -v`, no `-t` target — global scope only.
    assert calls == [(("show-options", "-w", "-g", "-v", "window-status-format"), False)]
    # Meaningful leading/trailing padding survives; only the newline is stripped.
    assert value == "  #I #W  "


def test_get_global_window_option_returns_none_when_unset(monkeypatch):
    monkeypatch.setattr(
        "hive.tmux._run",
        lambda args, check=True, timeout=5: subprocess.CompletedProcess(
            ["tmux", *args], 0, "\n", ""
        ),
    )
    assert tmux.get_global_window_option("window-status-format") is None


def test_wait_for_text_success_and_timeout(monkeypatch):
    outputs = iter(["booting", "still booting", "ready for help"])
    monkeypatch.setattr("hive.tmux.capture_pane", lambda _pane, lines=50: next(outputs))
    monkeypatch.setattr("hive.tmux.time.sleep", lambda _interval: None)

    assert tmux.wait_for_text("%1", "for help", timeout=1, interval=0) is True

    times = iter([0.0, 0.3, 0.6])
    monkeypatch.setattr("hive.tmux.capture_pane", lambda _pane, lines=50: "no match")
    monkeypatch.setattr("hive.tmux.time.time", lambda: next(times))

    assert tmux.wait_for_text("%1", "for help", timeout=0.5, interval=0) is False


def test_list_panes_full_or_none_is_status_aware(monkeypatch):
    ok = subprocess.CompletedProcess(["tmux"], 0, "%1\t[w]\tzsh\tagent\tworker\tt1\tclaude\tduo\t\n", "")
    fail = subprocess.CompletedProcess(["tmux"], 1, "", "timeout")

    monkeypatch.setattr("hive.tmux._run", lambda args, check=False, timeout=5: ok)
    panes = tmux.list_panes_full_or_none("dev:0")
    assert panes is not None and panes[0].pane_id == "%1"
    assert tmux.list_panes_full("dev:0")[0].pane_id == "%1"

    monkeypatch.setattr("hive.tmux._run", lambda args, check=False, timeout=5: fail)
    assert tmux.list_panes_full_or_none("dev:0") is None
    assert tmux.list_panes_full("dev:0") == []

    empty = subprocess.CompletedProcess(["tmux"], 0, "", "")
    monkeypatch.setattr("hive.tmux._run", lambda args, check=False, timeout=5: empty)
    assert tmux.list_panes_full_or_none("dev:0") == []


def test_pane_scan_status_maps_no_server_variants(monkeypatch):
    for stderr in ("no server running on /tmp/tmux-501/default", "error connecting to /x/tmux-501/default (No such file or directory)"):
        monkeypatch.setattr(
            "hive.tmux._run",
            lambda args, check=False, timeout=5, _e=stderr: subprocess.CompletedProcess(["tmux", *args], 1, "", _e),
        )
        assert tmux.list_panes_all_status() == (None, "no-server")
        assert tmux.list_team_windows_status() == (None, "no-server")

    monkeypatch.setattr(
        "hive.tmux._run",
        lambda args, check=False, timeout=5: subprocess.CompletedProcess(["tmux", *args], 1, "", "timeout"),
    )
    assert tmux.list_panes_all_status() == (None, "unknown")
    assert tmux.list_team_windows_status() == (None, "unknown")
