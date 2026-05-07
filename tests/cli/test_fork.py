import json
import shlex

import pytest

from hive.cli import _choose_fork_split, _FORK_NEW_TASK_MARKER, _fork_boundary_prompt, cli


def test_fork_auto_registers_with_derived_name(runner, configure_hive_home, monkeypatch, tmp_path):
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

    result = runner.invoke(cli, ["fork", "--pane", "%99", "-s", "h"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert len(sent) == 1 and sent[0][0] == "%100"
    assert sent[0][1].startswith("claude -r sess-123 --fork-session \"$(cat ")
    assert sent[0][1].endswith(")\"")
    assert payload["pane"] == "%100"
    assert payload["team"] == "team-x"
    assert payload["registered"]
    assert prompted == []


def test_fork_join_as_registers_new_agent_in_current_team(runner, configure_hive_home, monkeypatch, tmp_path):
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

    result = runner.invoke(cli, ["fork", "--pane", "%99", "-s", "h", "--join-as", "claude-2"])

    assert result.exit_code == 0
    # Boundary text is static and cached under $HIVE_HOME; the resume command
    # shell-expands it via `$(cat <path>)` so the typed command stays short.
    assert len(sent) == 1 and sent[0][0] == "%100"
    assert sent[0][1].startswith("claude -r sess-123 --fork-session \"$(cat ")
    assert sent[0][1].endswith(")\"")
    assert prompted == []
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


def test_fork_join_as_prompt_embeds_in_resume_command(runner, configure_hive_home, monkeypatch, tmp_path):
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
    # With --prompt, the boundary text is inlined together with the user prompt
    # in the resume command (rather than expanded from the cached file). The
    # NEW TASK marker separates inherited transcript context from the new
    # prompt so the fork only acts on instructions after the marker.
    expected_prompt = (
        _fork_boundary_prompt()
        + "\n\n"
        + _FORK_NEW_TASK_MARKER
        + "\n"
        + "先跑 hive thread Veh9 看原始内容，处理完 reply-to lulu"
    )
    expected_cmd = f"claude -r sess-123 --fork-session {shlex.quote(expected_prompt)}"
    assert sent == [("%100", expected_cmd, True)]
    assert prompted == []


def test_fork_boundary_prompt_is_static_and_directs_to_hive_team():
    body = _fork_boundary_prompt()
    assert "FORK BOUNDARY" in body
    assert "hive team" in body
    # Inherited context must explicitly include the user's most recent
    # instruction, not just agent-side pending tool calls.
    assert "user's most recent instruction" in body
    assert "Do NOT continue, retry, or re-execute" in body
    # Marker pattern: forks without a NEW TASK section must stop and wait.
    assert _FORK_NEW_TASK_MARKER in body
    assert "stop after identifying yourself and wait for new input" in body
    # Boundary must be a single user message (no leading / trailing whitespace drift).
    assert body == body.strip()



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
