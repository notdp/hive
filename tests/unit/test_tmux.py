import subprocess

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


def test_context_helpers_use_environment_and_display_message(monkeypatch):
    monkeypatch.setenv("TMUX", "/tmp/tmux-1")
    monkeypatch.setenv("TMUX_PANE", "%7")
    monkeypatch.setattr(
        "hive.tmux._run",
        lambda args, check=False, timeout=5: subprocess.CompletedProcess(
            ["tmux", *args],
            0,
            "dev:2\n" if "#{session_name}:#{window_index}" in args else ("dev\n" if "#{session_name}" in args else "2\n"),
            "",
        ),
    )

    assert tmux.is_inside_tmux() is True
    assert tmux.get_current_pane_id() == "%7"
    assert tmux.get_current_window_target() == "dev:2"
    assert tmux.get_current_session_name() == "dev"
    assert tmux.get_current_window_index() == "2"


def test_current_window_helpers_return_none_without_tmux_pane(monkeypatch):
    monkeypatch.delenv("TMUX_PANE", raising=False)

    assert tmux.get_current_window_target() is None
    assert tmux.get_current_session_name() is None
    assert tmux.get_current_window_index() is None


def test_list_panes_with_titles_and_full_parse_rows(monkeypatch):
    outputs = {
        "#{pane_id}\t#{pane_title}": "%1\tmain\n%2\tworker\n",
        tmux._HIVE_FMT: "%1\tmain\tdroid\tagent\tclaude\tteam-a\n%2\tshell\tzsh\tterminal\tterm-1\tteam-a\n",
    }

    def _fake_run(args, check=False, timeout=5):
        fmt = args[-1]
        return subprocess.CompletedProcess(["tmux", *args], 0, outputs[fmt], "")

    monkeypatch.setattr("hive.tmux._run", _fake_run)

    titled = tmux.list_panes_with_titles("dev:0")
    full = tmux.list_panes_full("dev:0")

    assert titled == [tmux.PaneInfo("%1", "main"), tmux.PaneInfo("%2", "worker")]
    assert full[0] == tmux.PaneInfo("%1", "main", "droid", "agent", "claude", "team-a")
    assert full[1].role == "terminal"


def test_pane_option_helpers_and_tagging(monkeypatch):
    calls = []

    def _fake_run(args, check=False, timeout=5):
        calls.append(tuple(args))
        stdout = "value\n" if args[0] == "display-message" else ""
        return subprocess.CompletedProcess(["tmux", *args], 0, stdout, "")

    monkeypatch.setattr("hive.tmux._run", _fake_run)

    tmux.set_pane_option("%1", "hive-role", "agent")
    assert tmux.get_pane_option("%1", "hive-role") == "value"
    tmux.clear_pane_option("%1", "hive-role")
    tmux.tag_pane("%1", "agent", "claude", "team-a")
    tmux.clear_pane_tags("%1")

    assert calls[0] == ("set-option", "-p", "-t", "%1", "@hive-role", "agent")
    assert calls[1] == ("display-message", "-t", "%1", "-p", "#{@hive-role}")
    assert calls[2] == ("set-option", "-p", "-t", "%1", "-u", "@hive-role")
    assert ("set-option", "-p", "-t", "%1", "@hive-agent", "claude") in calls
    assert ("set-option", "-p", "-t", "%1", "-u", "@hive-team") in calls


def test_wait_for_text_success_and_timeout(monkeypatch):
    outputs = iter(["booting", "still booting", "ready for help"])
    monkeypatch.setattr("hive.tmux.capture_pane", lambda _pane, lines=50: next(outputs))
    monkeypatch.setattr("hive.tmux.time.sleep", lambda _interval: None)

    assert tmux.wait_for_text("%1", "for help", timeout=1, interval=0) is True

    times = iter([0.0, 0.3, 0.6])
    monkeypatch.setattr("hive.tmux.capture_pane", lambda _pane, lines=50: "no match")
    monkeypatch.setattr("hive.tmux.time.time", lambda: next(times))

    assert tmux.wait_for_text("%1", "for help", timeout=0.5, interval=0) is False
