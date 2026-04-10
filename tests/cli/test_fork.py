import pytest

from hive.cli import _choose_fork_split, cli


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


@pytest.mark.parametrize("width,height,expected_horizontal", [
    (161, 41, True),    # both ok, wide enough for bias
    (160, 40, True),    # neither ok; h_score(79/80=0.99) > v_score(19/20=0.95)
    (100, 38, False),   # neither ok; v_score(100/80=1.25, 18/20=0.9 -> 0.9) > h_score(49/80=0.6, 38/20=1.9 -> 0.6)
    (170, 30, True),    # only horizontal works (v_half=14 < 20)
    (100, 41, False),   # only vertical works (h_half=49 < 80)
    (200, 50, True),    # both ok, 200 >= 50*2.5=125
    (120, 50, False),   # h_half=59 < 80, only vertical
    (80, 24, False),    # neither ok; v_score better than h_score
])
def test_choose_fork_split(width, height, expected_horizontal):
    assert _choose_fork_split(width, height) == expected_horizontal
