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
            "P", (), {"name": "claude", "fork_cmd": "hive claude -r {session_id} --fork-session", "ready_text": "Claude Code"},
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
    assert sent[0][1].startswith("hive claude -r sess-123 --fork-session \"$(cat ")
    assert sent[0][1].endswith(")\"")
    assert payload["pane"] == "%100"
    assert payload["team"] == "team-x"
    assert payload["registered"]
    assert prompted == []


def _fork_snapshot_team(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%99", session_name="dev")
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-x", "--workspace", str(workspace)]).exit_code == 0

    sent: list[tuple[str, str, bool]] = []
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%99")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _pane: type(
            "P", (), {"name": "claude", "fork_cmd": "hive claude -r {session_id} --fork-session", "ready_text": "Claude Code"},
        )(),
    )
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp/work")
    monkeypatch.setattr("hive.cli.tmux.split_window", lambda _pane, horizontal=True, cwd=None, detach=False: "%100")
    monkeypatch.setattr("hive.cli.tmux.send_keys", lambda pane, text, enter=True: sent.append((pane, text, enter)))
    monkeypatch.setattr("hive.cli.tmux.wait_for_text", lambda _pane, _text, timeout=0, interval=1: True)
    monkeypatch.setattr("hive.cli.time.sleep", lambda _s: None)
    monkeypatch.setattr("hive.agent.Agent.send", lambda self, text: None)

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%99", "orch", command="claude", role="lead", agent="orch", team="team-x", cli="claude")],
    )
    return sent


def test_fork_uses_fresh_runtime_snapshot_without_resolver(runner, configure_hive_home, monkeypatch, tmp_path):
    """A fresh sidecar snapshot is authoritative: fork must unwrap the nested
    `snapshot` field of the response and never fall back to pidfile/lsof
    guessing via resolve_session_id_for_pane."""
    sent = _fork_snapshot_team(runner, configure_hive_home, monkeypatch, tmp_path)
    monkeypatch.setattr(
        "hive.sidecar.request_runtime_snapshot",
        lambda *a, **k: {
            "ok": True,
            "pane": "%99",
            "snapshot": {"sessionId": "sid-runtime", "_sessionIdFresh": True},
        },
    )

    def _resolver_must_not_run(_pane, profile=None):
        raise AssertionError("resolve_session_id_for_pane called despite fresh snapshot")

    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", _resolver_must_not_run)

    result = runner.invoke(cli, ["fork", "--pane", "%99", "-s", "h"])

    assert result.exit_code == 0
    assert len(sent) == 1
    assert sent[0][1].startswith("hive claude -r sid-runtime --fork-session")


def test_fork_stale_runtime_snapshot_falls_back_to_resolver(runner, configure_hive_home, monkeypatch, tmp_path):
    sent = _fork_snapshot_team(runner, configure_hive_home, monkeypatch, tmp_path)
    monkeypatch.setattr(
        "hive.sidecar.request_runtime_snapshot",
        lambda *a, **k: {
            "ok": True,
            "pane": "%99",
            "snapshot": {"sessionId": "sid-old", "_sessionIdFresh": False},
        },
    )
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane, profile=None: "sess-fallback")

    result = runner.invoke(cli, ["fork", "--pane", "%99", "-s", "h"])

    assert result.exit_code == 0
    assert len(sent) == 1
    assert sent[0][1].startswith("hive claude -r sess-fallback --fork-session")


def test_fork_in_squad_prefixes_agent_name(runner, configure_hive_home, monkeypatch, tmp_path):
    """Forking in a squad pane auto-prefixes the derived name with the squad
    namespace (e.g. 'coco' → 'peaky.coco') and sets @hive-group on the new pane
    so qualified routing works."""
    configure_hive_home(current_pane="%99", session_name="dev")

    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-x", "--workspace", str(workspace)]).exit_code == 0

    sent: list[tuple[str, str, bool]] = []
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%99")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _pane: type(
            "P", (), {"name": "claude", "fork_cmd": "hive claude -r {session_id} --fork-session", "ready_text": "Claude Code"},
        )(),
    )
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane, profile=None: "sess-123")
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp/work")
    monkeypatch.setattr("hive.cli.tmux.split_window", lambda _pane, horizontal=True, cwd=None, detach=False: "%100")
    monkeypatch.setattr("hive.cli.tmux.send_keys", lambda pane, text, enter=True: sent.append((pane, text, enter)))
    monkeypatch.setattr("hive.cli.tmux.wait_for_text", lambda _pane, _text, timeout=0, interval=1: True)
    monkeypatch.setattr("hive.cli.time.sleep", lambda _s: None)
    monkeypatch.setattr("hive.agent.Agent.send", lambda self, text: None)

    from hive import tmux
    from hive.tmux import PaneInfo

    tmux.tag_pane("%99", "agent", "peaky.orch", "team-x", cli="claude", group="peaky")
    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%99", "orch", command="claude", role="agent", agent="peaky.orch", team="team-x", cli="claude", group="peaky")],
    )

    result = runner.invoke(cli, ["fork", "--pane", "%99", "-s", "h"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["registered"].startswith("peaky."), (
        f"expected squad-prefixed name, got {payload['registered']!r}"
    )
    assert tmux.get_pane_option("%100", "hive-group") == "peaky"


def test_fork_in_duo_does_not_prefix_agent_name(runner, configure_hive_home, monkeypatch, tmp_path):
    """Forking in a duo pane (group='duo') should NOT add a prefix and should
    NOT set @hive-group on the new pane."""
    configure_hive_home(current_pane="%99", session_name="dev")

    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-x", "--workspace", str(workspace)]).exit_code == 0

    sent: list[tuple[str, str, bool]] = []
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%99")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _pane: type(
            "P", (), {"name": "claude", "fork_cmd": "hive claude -r {session_id} --fork-session", "ready_text": "Claude Code"},
        )(),
    )
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane, profile=None: "sess-123")
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp/work")
    monkeypatch.setattr("hive.cli.tmux.split_window", lambda _pane, horizontal=True, cwd=None, detach=False: "%100")
    monkeypatch.setattr("hive.cli.tmux.send_keys", lambda pane, text, enter=True: sent.append((pane, text, enter)))
    monkeypatch.setattr("hive.cli.tmux.wait_for_text", lambda _pane, _text, timeout=0, interval=1: True)
    monkeypatch.setattr("hive.cli.time.sleep", lambda _s: None)
    monkeypatch.setattr("hive.agent.Agent.send", lambda self, text: None)

    from hive import tmux
    from hive.tmux import PaneInfo

    tmux.tag_pane("%99", "agent", "worker", "team-x", cli="claude", group="duo")
    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%99", "worker", command="claude", role="agent", agent="worker", team="team-x", cli="claude", group="duo")],
    )

    result = runner.invoke(cli, ["fork", "--pane", "%99", "-s", "h"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert "." not in payload["registered"], (
        f"duo fork should not prefix, got {payload['registered']!r}"
    )
    assert tmux.get_pane_option("%100", "hive-group") is None


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
            "P", (), {"name": "claude", "fork_cmd": "hive claude -r {session_id} --fork-session", "ready_text": "Claude Code"},
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
    assert sent[0][1].startswith("hive claude -r sess-123 --fork-session \"$(cat ")
    assert sent[0][1].endswith(")\"")
    assert prompted == []
    payload = json.loads(result.output)
    assert payload == {"pane": "%100", "registered": "claude-2", "team": "team-x"}

    from hive import tmux

    assert tmux.get_pane_option("%100", "hive-agent") == "claude-2"
    assert tmux.get_pane_option("%100", "hive-team") == "team-x"
    assert tmux.get_pane_option("%100", "hive-cli") == "claude"
    assert tmux.get_pane_option("%100", "hive-group") is None

    ctx = json.loads((tmp_path / ".hive" / "contexts" / "pane-100.json").read_text())
    assert ctx["team"] == "team-x"
    assert ctx["workspace"] == str(workspace)
    assert ctx["agent"] == "claude-2"


def test_fork_join_as_qualified_sets_group_tag(runner, configure_hive_home, monkeypatch, tmp_path):
    """Explicit --join-as with a qualified name (peaky.coco) must set both
    @hive-agent and @hive-group on the new pane."""
    configure_hive_home(current_pane="%99", session_name="dev")

    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-x", "--workspace", str(workspace)]).exit_code == 0

    sent: list[tuple[str, str, bool]] = []
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%99")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _pane: type(
            "P", (), {"name": "claude", "fork_cmd": "hive claude -r {session_id} --fork-session", "ready_text": "Claude Code"},
        )(),
    )
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane, profile=None: "sess-123")
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp/work")
    monkeypatch.setattr("hive.cli.tmux.split_window", lambda _pane, horizontal=True, cwd=None, detach=False: "%100")
    monkeypatch.setattr("hive.cli.tmux.send_keys", lambda pane, text, enter=True: sent.append((pane, text, enter)))
    monkeypatch.setattr("hive.cli.tmux.wait_for_text", lambda _pane, _text, timeout=0, interval=1: True)
    monkeypatch.setattr("hive.cli.time.sleep", lambda _s: None)
    monkeypatch.setattr("hive.agent.Agent.send", lambda self, text: None)

    from hive import tmux
    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%99", "orch", command="claude", role="lead", agent="orch", team="team-x", cli="claude")],
    )

    result = runner.invoke(cli, ["fork", "--pane", "%99", "-s", "h", "--join-as", "peaky.coco"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["registered"] == "peaky.coco"
    assert tmux.get_pane_option("%100", "hive-agent") == "peaky.coco"
    assert tmux.get_pane_option("%100", "hive-group") == "peaky"


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
            "P", (), {"name": "claude", "fork_cmd": "hive claude -r {session_id} --fork-session", "ready_text": "Claude Code"},
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
    expected_cmd = f"hive claude -r sess-123 --fork-session {shlex.quote(expected_prompt)}"
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
        lambda _pane: type("P", (), {"name": "claude", "fork_cmd": "hive claude -r {session_id} --fork-session"})(),
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


def test_fork_codex_team_bound_launches_through_hive_codex(runner, configure_hive_home, monkeypatch, tmp_path):
    """A team-bound codex fork is no longer refused: the clone launches through
    `hive codex fork <sid>`, which binds the clone's own per-pane daemon, so it
    joins daemon-backed like a spawned member. The source session id still
    comes from the source pane's daemon (resolve_session_id_for_pane is NOT
    mocked here)."""
    configure_hive_home(current_pane="%99", session_name="dev")

    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-x", "--workspace", str(workspace)]).exit_code == 0

    sent: list[tuple[str, str, bool]] = []
    splits: list[str] = []
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%99")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _pane: type(
            "P", (), {"name": "codex", "fork_cmd": "hive codex fork {session_id}", "ready_text": "codex"},
        )(),
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.session_id_for_pane",
        lambda pane: "sess-daemon" if pane == "%99" else None,
    )
    monkeypatch.setattr("hive.sidecar.request_runtime_snapshot", lambda *a, **k: None)
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp/work")
    monkeypatch.setattr(
        "hive.cli.tmux.split_window",
        lambda _pane, horizontal=True, cwd=None, detach=False: splits.append(_pane) or "%100",
    )
    monkeypatch.setattr("hive.cli.tmux.send_keys", lambda pane, text, enter=True: sent.append((pane, text, enter)))
    monkeypatch.setattr("hive.cli.tmux.wait_for_text", lambda _pane, _text, timeout=0, interval=1: True)
    monkeypatch.setattr("hive.cli.time.sleep", lambda _s: None)

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%99", "orch", command="node", role="lead", agent="orch", team="team-x", cli="codex")],
    )

    result = runner.invoke(cli, ["fork", "--pane", "%99", "-s", "h"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert splits == ["%99"]
    assert len(sent) == 1 and sent[0][0] == "%100"
    assert sent[0][1].startswith("hive codex fork sess-daemon \"$(cat ")
    assert payload["pane"] == "%100"
    assert payload["team"] == "team-x"
    assert payload["registered"]


def test_fork_non_team_pane_bare_clone(runner, configure_hive_home, monkeypatch, tmp_path):
    # A pane bound to no Hive team forks into a bare clone: split + resume sent,
    # but NO member registration and NO @hive-* tags on the new pane. Output is
    # registered: null, team: null, and the resume uses the orphan boundary.
    configure_hive_home(current_pane="%99", session_name="dev")

    sent: list[tuple[str, str, bool]] = []
    registered: list = []
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%99")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _pane: type(
            "P", (), {"name": "claude", "fork_cmd": "hive claude -r {session_id} --fork-session", "ready_text": "Claude Code"},
        )(),
    )
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane, profile=None: "sess-123")
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp/work")
    monkeypatch.setattr("hive.cli.tmux.split_window", lambda _pane, horizontal=True, cwd=None, detach=False: "%100")
    monkeypatch.setattr("hive.cli.tmux.send_keys", lambda pane, text, enter=True: sent.append((pane, text, enter)))
    monkeypatch.setattr("hive.cli._register_agent_member", lambda *a, **k: registered.append((a, k)))

    result = runner.invoke(cli, ["fork", "--pane", "%99", "-s", "h"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload == {"pane": "%100", "registered": None, "team": None}
    # split happened and a resume command was sent to the new pane.
    assert len(sent) == 1 and sent[0][0] == "%100"
    assert sent[0][1].startswith("hive claude -r sess-123 --fork-session \"$(cat ")
    assert sent[0][1].endswith(")\"")
    # the orphan boundary file is used, not the team one.
    assert "fork-boundary-orphan.txt" in sent[0][1]
    # bare clone: no registration, no @hive-* tags on the new pane.
    assert registered == []
    from hive import tmux

    assert tmux.get_pane_option("%100", "hive-team") is None
    assert tmux.get_pane_option("%100", "hive-agent") is None


def test_fork_non_team_current_pane_bare_clone(runner, configure_hive_home, monkeypatch, tmp_path):
    # no --pane: the current pane is non-team, so fork clones it bare and never
    # falls into _resolve_scoped_team (which would fail "no Hive team is bound").
    configure_hive_home(current_pane="%99", session_name="dev")

    sent: list[tuple[str, str, bool]] = []
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%99")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _pane: type(
            "P", (), {"name": "claude", "fork_cmd": "hive claude -r {session_id} --fork-session", "ready_text": "Claude Code"},
        )(),
    )
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane, profile=None: "sess-123")
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp/work")
    monkeypatch.setattr("hive.cli.tmux.split_window", lambda _pane, horizontal=True, cwd=None, detach=False: "%100")
    monkeypatch.setattr("hive.cli.tmux.send_keys", lambda pane, text, enter=True: sent.append((pane, text, enter)))

    def _no_scoped(*_a, **_kw):
        raise AssertionError("non-team fork must not resolve a scoped team")

    monkeypatch.setattr("hive.cli._resolve_scoped_team", _no_scoped)

    result = runner.invoke(cli, ["fork", "-s", "h"])  # no --pane: uses current pane %99

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload == {"pane": "%100", "registered": None, "team": None}
    assert len(sent) == 1 and sent[0][0] == "%100"


def test_fork_join_as_on_non_team_fails_before_split(runner, configure_hive_home, monkeypatch, tmp_path):
    # --join-as needs a team to register into; on a non-team pane it fails BEFORE
    # any split and never auto-creates a team.
    configure_hive_home(current_pane="%99", session_name="dev")
    split_called = False

    def _split_window(_pane, horizontal=True, cwd=None, detach=False):
        nonlocal split_called
        split_called = True
        return "%100"

    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%99")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _pane: type("P", (), {"name": "claude", "fork_cmd": "hive claude -r {session_id} --fork-session"})(),
    )
    monkeypatch.setattr("hive.cli.tmux.split_window", _split_window)

    result = runner.invoke(cli, ["fork", "--pane", "%99", "-s", "h", "--join-as", "dodo"])

    assert result.exit_code != 0
    assert "--join-as requires a team-bound pane" in result.output
    assert split_called is False


def test_fork_non_team_codex_resolves_session_via_daemon(runner, configure_hive_home, monkeypatch, tmp_path):
    # Non-team codex fork still resolves the session via the per-pane daemon
    # (daemon-first), so the bare clone's resume command carries the right id.
    # Like the team-bound codex test, this does NOT mock resolve_session_id_for_pane.
    configure_hive_home(current_pane="%99", session_name="dev")

    sent: list[tuple[str, str, bool]] = []
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%99")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _pane: type(
            "P", (), {"name": "codex", "fork_cmd": "hive codex fork {session_id}", "ready_text": "codex"},
        )(),
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.session_id_for_pane",
        lambda pane: "sess-daemon" if pane == "%99" else None,
    )
    monkeypatch.setattr("hive.sidecar.request_runtime_snapshot", lambda *a, **k: None)
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp/work")
    monkeypatch.setattr("hive.cli.tmux.split_window", lambda _pane, horizontal=True, cwd=None, detach=False: "%100")
    monkeypatch.setattr("hive.cli.tmux.send_keys", lambda pane, text, enter=True: sent.append((pane, text, enter)))

    result = runner.invoke(cli, ["fork", "--pane", "%99", "-s", "h"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload == {"pane": "%100", "registered": None, "team": None}
    assert len(sent) == 1 and sent[0][0] == "%100"
    assert sent[0][1].startswith("hive codex fork sess-daemon")


def test_fork_orphan_boundary_prompt_has_no_self_lookup():
    body = _fork_boundary_prompt(team_bound=False)
    assert "FORK BOUNDARY" in body
    # orphan has no team and no `self` — it must NOT be told to find an identity.
    assert "bound to any Hive team" in body
    assert "find your own identity" not in body
    # anti-re-execution core is preserved verbatim.
    assert "Do NOT continue, retry, or re-execute" in body
    assert _FORK_NEW_TASK_MARKER in body
    # Single user message: no leading / trailing whitespace drift.
    assert body == body.strip()


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


def test_claude_profile_forks_through_the_managed_launcher():
    # a forked team pane must register a channel like a spawned one: plain
    # `claude` is never hive-managed, `hive claude` is
    from hive.agent_cli import PROFILES

    assert PROFILES["claude"].fork_cmd.startswith("hive claude -r ")
    assert PROFILES["codex"].fork_cmd.startswith("hive codex fork ")
