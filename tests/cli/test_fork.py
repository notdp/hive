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
            "P", (), {"name": "claude", "fork_cmd": "claude -r {session_id} --fork-session", "ready_text": "Claude Code"},
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


def test_fork_in_squad_prefixes_agent_name(runner, configure_hive_home, monkeypatch, tmp_path):
    """Forking in a squad pane auto-prefixes the derived name with the squad
    namespace (e.g. 'coco' → 'peaky.coco') so return routing works."""
    configure_hive_home(current_pane="%99", session_name="dev")

    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-x", "--workspace", str(workspace)]).exit_code == 0

    sent: list[tuple[str, str, bool]] = []
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%99")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _pane: type(
            "P", (), {"name": "claude", "fork_cmd": "claude -r {session_id} --fork-session", "ready_text": "Claude Code"},
        )(),
    )
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane, profile=None: "sess-123")
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp/work")
    monkeypatch.setattr("hive.cli.tmux.split_window", lambda _pane, horizontal=True, cwd=None, detach=False: "%100")
    monkeypatch.setattr("hive.cli.tmux.send_keys", lambda pane, text, enter=True: sent.append((pane, text, enter)))
    monkeypatch.setattr("hive.cli.tmux.wait_for_text", lambda _pane, _text, timeout=0, interval=1: True)
    monkeypatch.setattr("hive.cli.time.sleep", lambda _s: None)
    monkeypatch.setattr("hive.agent.Agent.send", lambda self, text: None)

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%99", "orch", command="claude", role="agent", agent="peaky.orch", team="team-x", cli="claude", group="peaky")],
    )
    # Source pane is in squad "peaky" — must also return hive-team so the
    # pane is recognised as team-bound by _resolve_pane_target.
    pane_options = {"hive-group": "peaky", "hive-team": "team-x", "hive-cli": "claude", "hive-agent": "peaky.orch"}
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option",
        lambda pane, key: pane_options.get(key, ""),
    )

    result = runner.invoke(cli, ["fork", "--pane", "%99", "-s", "h"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["registered"].startswith("peaky."), (
        f"expected squad-prefixed name, got {payload['registered']!r}"
    )


def test_fork_in_duo_does_not_prefix_agent_name(runner, configure_hive_home, monkeypatch, tmp_path):
    """Forking in a duo pane (group='duo') should NOT add a prefix."""
    configure_hive_home(current_pane="%99", session_name="dev")

    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-x", "--workspace", str(workspace)]).exit_code == 0

    sent: list[tuple[str, str, bool]] = []
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%99")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _pane: type(
            "P", (), {"name": "claude", "fork_cmd": "claude -r {session_id} --fork-session", "ready_text": "Claude Code"},
        )(),
    )
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane, profile=None: "sess-123")
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp/work")
    monkeypatch.setattr("hive.cli.tmux.split_window", lambda _pane, horizontal=True, cwd=None, detach=False: "%100")
    monkeypatch.setattr("hive.cli.tmux.send_keys", lambda pane, text, enter=True: sent.append((pane, text, enter)))
    monkeypatch.setattr("hive.cli.tmux.wait_for_text", lambda _pane, _text, timeout=0, interval=1: True)
    monkeypatch.setattr("hive.cli.time.sleep", lambda _s: None)
    monkeypatch.setattr("hive.agent.Agent.send", lambda self, text: None)

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%99", "worker", command="claude", role="agent", agent="worker", team="team-x", cli="claude", group="duo")],
    )
    pane_options = {"hive-group": "duo", "hive-team": "team-x", "hive-cli": "claude", "hive-agent": "worker"}
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option",
        lambda pane, key: pane_options.get(key, ""),
    )

    result = runner.invoke(cli, ["fork", "--pane", "%99", "-s", "h"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert "." not in payload["registered"], (
        f"duo fork should not prefix, got {payload['registered']!r}"
    )


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
            "P", (), {"name": "claude", "fork_cmd": "claude -r {session_id} --fork-session", "ready_text": "Claude Code"},
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
            "P", (), {"name": "claude", "fork_cmd": "claude -r {session_id} --fork-session", "ready_text": "Claude Code"},
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
        lambda _pane: type("P", (), {"name": "claude", "fork_cmd": "claude -r {session_id} --fork-session"})(),
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


def test_fork_codex_resolves_session_via_daemon(runner, configure_hive_home, monkeypatch, tmp_path):
    """Regression: fork on a born-connected codex pane.

    The rollout jsonl is held by the per-pane app-server daemon, not by any
    process in the pane's tty tree, so the lsof adapter returns None. fork must
    fall back to the daemon session id instead of dying with 'cannot determine
    session id'. Unlike the other fork tests this does NOT mock
    resolve_session_id_for_pane — it exercises the real resolver so the daemon
    fallback is covered end-to-end. (Only codex regressed; claude was fine.)
    """
    configure_hive_home(current_pane="%99", session_name="dev")

    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-x", "--workspace", str(workspace)]).exit_code == 0

    sent: list[tuple[str, str, bool]] = []
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%99")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _pane: type(
            "P", (), {"name": "codex", "fork_cmd": "codex fork {session_id}", "ready_text": "codex"},
        )(),
    )

    # The real CodexAdapter runs (NOT mocked): it asks the daemon first, so we
    # only stub the daemon lookup — no real socket needed. This exercises the
    # actual daemon-first path that fork depends on.
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.session_id_for_pane",
        lambda pane: "sess-daemon" if pane == "%99" else None,
    )
    # No sidecar in tests -> the snapshot path yields nothing, forcing the resolver.
    monkeypatch.setattr("hive.sidecar.request_runtime_snapshot", lambda *a, **k: None)
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp/work")
    monkeypatch.setattr("hive.cli.tmux.split_window", lambda _pane, horizontal=True, cwd=None, detach=False: "%100")
    monkeypatch.setattr("hive.cli.tmux.send_keys", lambda pane, text, enter=True: sent.append((pane, text, enter)))
    monkeypatch.setattr("hive.cli.time.sleep", lambda _s: None)

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%99", "orch", command="node", role="lead", agent="orch", team="team-x", cli="codex")],
    )

    result = runner.invoke(cli, ["fork", "--pane", "%99", "-s", "h"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["pane"] == "%100"
    assert payload["team"] == "team-x"
    # The resume command carries the daemon-resolved session id: fork survived.
    assert len(sent) == 1 and sent[0][0] == "%100"
    assert sent[0][1].startswith("codex fork sess-daemon")


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
            "P", (), {"name": "claude", "fork_cmd": "claude -r {session_id} --fork-session", "ready_text": "Claude Code"},
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
    assert sent[0][1].startswith("claude -r sess-123 --fork-session \"$(cat ")
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
            "P", (), {"name": "claude", "fork_cmd": "claude -r {session_id} --fork-session", "ready_text": "Claude Code"},
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
        lambda _pane: type("P", (), {"name": "claude", "fork_cmd": "claude -r {session_id} --fork-session"})(),
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
            "P", (), {"name": "codex", "fork_cmd": "codex fork {session_id}", "ready_text": "codex"},
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
    assert sent[0][1].startswith("codex fork sess-daemon")


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
