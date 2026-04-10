from hive.cli import cli


def test_fork_uses_claude_profile_from_runtime_session(runner, configure_hive_home, monkeypatch):
    configure_hive_home(current_pane="%99", session_name="dev")

    sent: list[tuple[str, str, bool]] = []
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%99")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane: type("P", (), {"name": "claude", "resume_cmd": "claude -r {session_id} --fork-session"})())
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane, profile=None: "sess-123")
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp/work")
    monkeypatch.setattr("hive.cli.tmux.split_window", lambda _pane, horizontal=True, cwd=None, detach=False: "%100")
    monkeypatch.setattr("hive.cli.tmux.send_keys", lambda pane, text, enter=True: sent.append((pane, text, enter)))

    result = runner.invoke(cli, ["fork", "--pane", "%99", "-s", "h"])

    assert result.exit_code == 0
    assert sent == [("%100", "claude -r sess-123 --fork-session", True)]


def test_fork_falls_back_to_codex_resume_command(runner, configure_hive_home, monkeypatch):
    configure_hive_home(current_pane="%141", session_name="dev")

    sent: list[tuple[str, str, bool]] = []
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%141")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane: type("P", (), {"name": "codex", "resume_cmd": "codex fork {session_id}"})())
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane, profile=None: "sess-codex")
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp/work")
    monkeypatch.setattr("hive.cli.tmux.split_window", lambda _pane, horizontal=True, cwd=None, detach=False: "%145")
    monkeypatch.setattr("hive.cli.tmux.send_keys", lambda pane, text, enter=True: sent.append((pane, text, enter)))

    result = runner.invoke(cli, ["fork", "--pane", "%141", "-s", "v"])

    assert result.exit_code == 0
    assert sent == [("%145", "codex fork sess-codex", True)]
