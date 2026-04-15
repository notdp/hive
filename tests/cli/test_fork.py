import json

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


def test_fork_join_as_registers_new_agent_in_current_team(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%99", session_name="dev")

    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-x", "--workspace", str(workspace)]).exit_code == 0

    sent: list[tuple[str, str, bool]] = []
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%99")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _pane: type("P", (), {"name": "claude", "resume_cmd": "claude -r {session_id} --fork-session"})(),
    )
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane, profile=None: "sess-123")
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp/work")
    monkeypatch.setattr("hive.cli.tmux.split_window", lambda _pane, horizontal=True, cwd=None, detach=False: "%100")
    monkeypatch.setattr("hive.cli.tmux.send_keys", lambda pane, text, enter=True: sent.append((pane, text, enter)))

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%99", "orch", command="claude", role="lead", agent="orch", team="team-x", cli="claude")],
    )

    result = runner.invoke(cli, ["fork", "--pane", "%99", "-s", "h", "--join-as", "claude-2"])

    assert result.exit_code == 0
    assert sent == [("%100", "claude -r sess-123 --fork-session", True)]
    payload = json.loads(result.output)
    assert payload == {"pane": "%100", "registered": "claude-2", "team": "team-x"}

    from hive import tmux

    assert tmux.get_pane_option("%100", "hive-agent") == "claude-2"
    assert tmux.get_pane_option("%100", "hive-team") == "team-x"
    assert tmux.get_pane_option("%100", "hive-cli") == "claude"

    ctx = json.loads((tmp_path / ".hive" / "contexts" / "pane-100.json").read_text())
    assert ctx["team"] == "team-x"
    assert ctx["workspace"] == str(workspace)
    assert ctx["agent"] == "claude-2"


def test_fork_join_as_prompt_waits_for_ready_then_sends_prompt(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%99", session_name="dev")

    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-x", "--workspace", str(workspace)]).exit_code == 0

    sent: list[tuple[str, str, bool]] = []
    prompted: list[tuple[str, str, str]] = []
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%99")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _pane: type(
            "P", (), {"name": "claude", "resume_cmd": "claude -r {session_id} --fork-session", "ready_text": "Claude Code"},
        )(),
    )
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane, profile=None: "sess-123")
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp/work")
    monkeypatch.setattr("hive.cli.tmux.split_window", lambda _pane, horizontal=True, cwd=None, detach=False: "%100")
    monkeypatch.setattr("hive.cli.tmux.send_keys", lambda pane, text, enter=True: sent.append((pane, text, enter)))
    monkeypatch.setattr("hive.cli.tmux.wait_for_text", lambda _pane, _text, timeout=0, interval=1: True)
    monkeypatch.setattr("hive.cli.time.sleep", lambda _s: None)
    monkeypatch.setattr("hive.agent.Agent.send", lambda self, text: prompted.append((self.name, self.pane_id, text)))

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%99", "orch", command="claude", role="lead", agent="orch", team="team-x", cli="claude")],
    )

    result = runner.invoke(
        cli,
        [
            "fork",
            "--pane",
            "%99",
            "-s",
            "h",
            "--join-as",
            "claude-2",
            "--prompt",
            "先跑 hive thread Veh9 看原始内容，处理完 reply-to lulu",
        ],
    )

    assert result.exit_code == 0
    assert sent == [("%100", "claude -r sess-123 --fork-session", True)]
    assert prompted == [("claude-2", "%100", "先跑 hive thread Veh9 看原始内容，处理完 reply-to lulu")]


def test_fork_prompt_requires_join_as(runner, configure_hive_home, monkeypatch):
    configure_hive_home(current_pane="%99", session_name="dev")

    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%99")

    result = runner.invoke(cli, ["fork", "--pane", "%99", "--prompt", "do work"])

    assert result.exit_code != 0
    assert "--prompt requires --join-as" in result.output


def test_fork_join_as_rejects_taken_name_before_split(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%99", session_name="dev")

    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-x", "--workspace", str(workspace)]).exit_code == 0

    split_called = False

    def _split_window(_pane, horizontal=True, cwd=None, detach=False):
        nonlocal split_called
        split_called = True
        return "%100"

    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%99")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _pane: type("P", (), {"name": "claude", "resume_cmd": "claude -r {session_id} --fork-session"})(),
    )
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane, profile=None: "sess-123")
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp/work")
    monkeypatch.setattr("hive.cli.tmux.split_window", _split_window)

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [
            PaneInfo("%99", "orch", command="claude", role="lead", agent="orch", team="team-x", cli="claude"),
            PaneInfo("%88", "claude-2", command="claude", role="agent", agent="claude-2", team="team-x", cli="claude"),
        ],
    )

    result = runner.invoke(cli, ["fork", "--pane", "%99", "-s", "h", "--join-as", "claude-2"])

    assert result.exit_code != 0
    assert "already taken" in result.output
    assert split_called is False


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
