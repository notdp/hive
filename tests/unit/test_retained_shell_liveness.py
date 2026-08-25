"""spawn-pane-survives: a retained shell is not an agent runtime.

The pane (and its shell) survive the CLI exiting now, so liveness comes from
process evidence only (`cliAlive`), delivery fails closed before any native
transport, and the sidecar consumers ignore retained shells.
"""
from types import SimpleNamespace

import pytest

from hive import bus, sidecar
from hive.agent import Agent
from hive.runtime_snapshot import RuntimeSnapshotStore

pytestmark = pytest.mark.unit


def _proc(command, argv="", pid=1):
    return SimpleNamespace(pid=pid, command=command, argv=argv or command)


def _pane_env(monkeypatch, *, alive=True, command="zsh", title="", procs=()):
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda _p: alive)
    monkeypatch.setattr("hive.tmux.get_pane_current_command", lambda _p: command)
    monkeypatch.setattr("hive.tmux.get_pane_title", lambda _p: title)
    monkeypatch.setattr("hive.tmux.get_pane_tty", lambda _p: "/dev/ttys099")
    monkeypatch.setattr("hive.tmux.list_tty_processes", lambda _t: list(procs))
    # output-based busy would say True: the contract must force it off for
    # anything that is not a live CLI
    monkeypatch.setattr(sidecar, "_busy_output_payload", lambda _p: {"busy": True})


def _forbid(monkeypatch, target, message):
    monkeypatch.setattr(target, lambda *_a, **_k: (_ for _ in ()).throw(AssertionError(message)))


# --- V2: liveness three-state matrix -----------------------------------------


def test_payload_pane_dead_is_fully_offline(monkeypatch):
    _pane_env(monkeypatch, alive=False)
    rt = sidecar._agent_runtime_payload("%9")
    assert rt["alive"] is False
    assert rt["cliAlive"] is False
    assert rt["busy"] is False
    assert rt["inputState"] == "offline"
    assert rt["inputReason"] == "pane_dead"


def test_payload_retained_shell_with_stale_codex_title(monkeypatch):
    # deception sample 1: the title still says "OpenAI Codex" but the TTY has
    # only the shell — title text is not liveness evidence
    _pane_env(monkeypatch, command="zsh", title="OpenAI Codex", procs=[_proc("-zsh")])
    _forbid(monkeypatch, "hive.sidecar._codex_app_server_runtime",
            "daemon runtime must not be consulted for a retained shell")
    rt = sidecar._agent_runtime_payload("%9")
    assert rt["alive"] is True
    assert rt["cliAlive"] is False
    assert rt["busy"] is False
    assert rt["inputState"] == "offline"
    assert rt["inputReason"] == "cli_exited"


def test_payload_retained_shell_ignores_surviving_daemon(monkeypatch):
    # deception sample 2: the per-pane daemon (and its thread) outlive the
    # TUI — a reachable daemon must not make the member look alive
    _pane_env(monkeypatch, command="zsh", title="codex", procs=[_proc("-zsh")])
    _forbid(monkeypatch, "hive.sidecar._codex_app_server_runtime",
            "daemon runtime must not be consulted for a retained shell")
    rt = sidecar._agent_runtime_payload("%9")
    assert rt["cliAlive"] is False
    assert rt["inputState"] == "offline"
    assert rt["inputReason"] == "cli_exited"
    assert rt["busy"] is False


def test_payload_live_codex_process_reaches_daemon_runtime(monkeypatch):
    _pane_env(
        monkeypatch,
        command="node",
        procs=[_proc("node", "node /opt/homebrew/bin/codex --remote unix:///s")],
    )
    monkeypatch.setattr("hive.agent_cli.resolve_model_for_pane", lambda *_a, **_k: "")
    monkeypatch.setattr(
        sidecar, "_codex_app_server_runtime",
        lambda _p: {"busy": True, "inputState": "ready", "inputReason": ""},
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.session_id_for_pane", lambda _p: "sid-1",
    )
    rt = sidecar._agent_runtime_payload("%9")
    assert rt["cliAlive"] is True
    assert rt["busy"] is True
    assert rt["sessionId"] == "sid-1"


def test_payload_live_claude_process_is_cli_alive(monkeypatch):
    _pane_env(monkeypatch, command="claude", procs=[])
    monkeypatch.setattr("hive.agent_cli.resolve_model_for_pane", lambda *_a, **_k: "")
    monkeypatch.setattr("hive.adapters.get", lambda _n: None)
    rt = sidecar._agent_runtime_payload("%9")
    assert rt["cliAlive"] is True
    # flow passed the liveness gate and stopped at the adapter, not at offline
    assert rt["inputState"] == "unknown"
    assert rt["inputReason"] == "no_session"


# --- V3: delivery fails closed before any native transport -------------------


def _wire_send(monkeypatch, workspace, agent):
    team = SimpleNamespace(
        name="team-x", workspace=str(workspace), tmux_session="dev", tmux_window="dev:0"
    )
    monkeypatch.setattr(sidecar, "_resolve_live_agent", lambda _t, _a: (team, agent))
    monkeypatch.setattr(sidecar, "_resolve_ack_baseline", lambda _t: (None, 0))
    monkeypatch.setattr(sidecar, "_check_send_gate", lambda _p: None)


def test_send_to_retained_shell_fails_closed_with_durable_bus_event(tmp_path, monkeypatch):
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    monkeypatch.setattr("hive.agent_cli.detect_cli_process_for_pane", lambda _p: None)
    _forbid(monkeypatch, "hive.adapters.codex_app_server.send_to_pane",
            "native codex transport must not be called for a retained shell")
    _forbid(monkeypatch, "hive.adapters.claude_sessions.send",
            "native claude transport must not be called for a retained shell")
    _forbid(monkeypatch, "hive.agent._submit_interactive_text",
            "keystroke fallback is forbidden")
    agent = Agent(name="v", team_name="team-x", pane_id="%9", cli="codex")
    _wire_send(monkeypatch, workspace, agent)

    payload = sidecar._send_payload(
        workspace=str(workspace), team_name="team-x", sender_agent="w",
        sender_pane="%1", target_agent="v", body="hi", artifact="", reply_to="",
    )

    assert payload["ok"] is False
    assert "transport refused" in payload["error"]
    assert "cli_exited" in payload["error"]
    # the send event is durable: recoverable from the bus by msgId
    events = bus.read_all_events(workspace)
    assert [e["intent"] for e in events] == ["send"]
    assert payload["msgId"]


def _wire_codex_transport(monkeypatch, sent, accepted):
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.send_to_pane",
        lambda pane, text: sent.append((pane, text)) or accepted,
    )


def _wire_grok_transport(monkeypatch, sent, accepted):
    monkeypatch.setattr(
        "hive.adapters.grok_leader.send_to_pane",
        lambda pane, text: sent.append((pane, text)) or accepted,
    )


def _wire_claude_transport(monkeypatch, sent, accepted):
    # claude is addressed pane -> live pid -> registry entry -> that session's
    # inbox socket, so the recorded pane is the one whose socket got written
    from hive.adapters.claude_sessions import ClaudeSession

    sock = "/run/claude-%9.sock"
    monkeypatch.setattr(
        "hive.agent_cli.claude_pid_for_pane", lambda pane: 4242 if pane == "%9" else None
    )
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.session_for_pid",
        lambda pid: ClaudeSession(
            name="sess", pid=pid, cwd="", kind="cli", socket_path=sock, session_id="sid-1"
        ) if pid == 4242 else None,
    )
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda path, text, *, sender: (
            sent.append(("%9" if path == sock else path, text)) or accepted
        ),
    )


@pytest.mark.parametrize(
    "cli_name,wire,accepted",
    [
        ("codex", _wire_codex_transport, "turnStartAccepted"),
        ("grok", _wire_grok_transport, "sessionPromptQueued"),
        ("claude", _wire_claude_transport, "udsWriteAccepted"),
    ],
)
def test_send_with_live_cli_still_uses_native_transport(
    tmp_path, monkeypatch, cli_name, wire, accepted
):
    from hive.agent_cli import get_profile

    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    monkeypatch.setattr(
        "hive.agent_cli.detect_cli_process_for_pane", lambda _p: get_profile(cli_name)
    )
    sent: list[tuple] = []
    wire(monkeypatch, sent, accepted)
    agent = Agent(name="v", team_name="team-x", pane_id="%9", cli=cli_name)
    _wire_send(monkeypatch, workspace, agent)

    payload = sidecar._send_payload(
        workspace=str(workspace), team_name="team-x", sender_agent="w",
        sender_pane="%1", target_agent="v", body="hi", artifact="", reply_to="",
    )

    assert payload["ok"] is True
    assert sent and sent[0][0] == "%9"


# --- V4: sidecar consumers ignore retained shells -----------------------------


def test_idle_notify_excludes_retained_shell_pane(monkeypatch):
    monkeypatch.setattr(
        sidecar, "_team_member_bindings",
        lambda _t: {
            "w": {"role": "agent", "pane": "%1"},
            "v": {"role": "agent", "pane": "%2"},
        },
    )
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda _p: True)
    monkeypatch.setattr(
        sidecar, "detect_cli_process_for_pane",
        lambda pane: object() if pane == "%1" else None,
    )
    assert sidecar._idle_notify_agent_panes("t") == ["%1"]


def test_snapshot_tick_never_probes_or_stales_a_retained_shell(monkeypatch):
    monkeypatch.setattr(sidecar, "detect_cli_process_for_pane", lambda _p: None)
    monkeypatch.setattr(sidecar, "_pane_has_recent_output", lambda _p: True)
    _forbid(monkeypatch, "hive.sidecar._probe_session_id_from_pidfile",
            "retained shell must not be probed")
    store = RuntimeSnapshotStore()
    store.update_session_id("%1", "sid-1", source="pidfile", observed_at=9.0)

    sidecar._runtime_snapshot_tick(
        "t", store=store, now=10.0,
        members={"v": {"role": "agent", "pane": "%1", "cli": "claude"}},
    )

    snap = store.get("%1")
    assert snap is not None
    assert snap.sessionId.value == "sid-1"
    # plain shell output is not a session-rotation signal
    assert snap.sessionId.is_fresh(now=10.0) is True


def test_pairing_rejects_retained_shell_neighbor(monkeypatch):
    from hive.cli import _duo_neighbor_for_pairing
    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.agent_cli.detect_cli_process_for_pane", lambda _p: None
    )
    neighbor = PaneInfo(pane_id="%2", title="OpenAI Codex")
    picked = _duo_neighbor_for_pairing(
        "%1", [PaneInfo(pane_id="%1", title="[worker]"), neighbor], "claude"
    )
    assert picked is None


def test_doctor_payload_exposes_cli_alive(monkeypatch):
    fake_agent = SimpleNamespace(pane_id="%1", is_alive=lambda: True)
    fake_team = SimpleNamespace(name="t", agents={"v": fake_agent}, get=lambda _n: fake_agent)
    monkeypatch.setattr("hive.team.Team.load", lambda _t: fake_team)
    monkeypatch.setattr(
        sidecar, "_member_runtime_payload",
        lambda _p, role: {
            "alive": True, "cliAlive": False, "busy": False,
            "inputState": "offline", "inputReason": "cli_exited",
        },
    )
    diag = sidecar._doctor_payload("/tmp/ws", "t", "v")
    assert diag["alive"] is True
    assert diag["cliAlive"] is False


def test_team_payload_merge_carries_cli_alive(monkeypatch):
    from hive.cli import _augment_team_payload_with_runtime

    team = SimpleNamespace(name="t", workspace="/tmp/ws", tmux_window="dev:0", tmux_session="dev")
    monkeypatch.setattr("hive.cli._resolve_workspace_for_team", lambda _t: "/tmp/ws", raising=False)
    monkeypatch.setattr("hive.cli._ensure_team_sidecar", lambda _t, _w: 1)
    monkeypatch.setattr(
        "hive.sidecar.request_team_runtime",
        lambda _ws, team: {
            "ok": True,
            "members": {
                "v": {"alive": True, "cliAlive": False, "inputState": "offline"},
            },
        },
    )
    payload = {"members": [{"name": "v"}], "workspace": "/tmp/ws"}
    out = _augment_team_payload_with_runtime(team, payload)
    member = out["members"][0]
    assert member["alive"] is True
    assert member["cliAlive"] is False
    assert member["inputState"] == "offline"


def test_retained_shell_running_rg_codex_is_not_a_cli(monkeypatch, tmp_path):
    # r1 blocker regression: a retained shell whose foreground command merely
    # MENTIONS a CLI name (`rg codex src tests`) must not flip cliAlive nor
    # reopen native transport
    _pane_env(
        monkeypatch,
        command="rg",
        procs=[_proc("rg", "rg codex src tests"), _proc("-zsh")],
    )
    _forbid(monkeypatch, "hive.sidecar._codex_app_server_runtime",
            "daemon runtime must not be consulted")
    rt = sidecar._agent_runtime_payload("%9")
    assert rt["cliAlive"] is False
    assert rt["inputState"] == "offline"
    assert rt["inputReason"] == "cli_exited"

    # the real send boundary, real probe (no pin): both transports forbidden
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _forbid(monkeypatch, "hive.adapters.codex_app_server.send_to_pane",
            "native codex transport must not be called")
    _forbid(monkeypatch, "hive.adapters.claude_sessions.send",
            "native claude transport must not be called")
    _forbid(monkeypatch, "hive.agent._submit_interactive_text",
            "keystroke fallback is forbidden")
    agent = Agent(name="v", team_name="team-x", pane_id="%9", cli="codex")
    _wire_send(monkeypatch, workspace, agent)
    payload = sidecar._send_payload(
        workspace=str(workspace), team_name="team-x", sender_agent="w",
        sender_pane="%1", target_agent="v", body="hi", artifact="", reply_to="",
    )
    assert payload["ok"] is False
    assert "cli_exited" in payload["error"]
    assert [e["intent"] for e in bus.read_all_events(workspace)] == ["send"]


def test_resume_hint_colors_command_on_terminals_only(monkeypatch, tmp_path):
    # cyan on a real terminal, plain text everywhere else (pipes/tests/logs):
    # click strips the styling when stdout is not a tty
    from click.testing import CliRunner

    from hive.cli import cli as hive_cli

    work = tmp_path / "work"
    work.mkdir()
    monkeypatch.chdir(work)
    monkeypatch.setenv("HIVE_HOME", str(tmp_path / "hive-home"))
    monkeypatch.setenv("TMUX_PANE", "%5")
    tags = {"hive-team": "t1", "hive-agent": "worker"}
    monkeypatch.setattr("hive.cli.tmux.get_pane_option", lambda _p, key: tags.get(key))
    d = tmp_path / "hive-home" / "state" / "resume"
    d.mkdir(parents=True)
    import json as _json

    (d / "t1.json").write_text(_json.dumps({
        "schema": 1, "handle": "t1", "team": "t1", "group": "duo",
        "windowName": "", "workspace": "", "repoCwd": "", "repo": "",
        "branch": "", "pr": "", "createdAt": "1", "savedAt": "now",
        "members": [{"name": "worker", "sessionId": "sid-1"}],
    }))

    colored = CliRunner().invoke(hive_cli, ["resume-hint", "claude"], color=True)
    assert colored.exit_code == 0
    assert "\x1b[36m" in colored.output and "\x1b[0m" in colored.output
    assert "claude --resume sid-1" in colored.output

    plain = CliRunner().invoke(hive_cli, ["resume-hint", "claude"])
    assert plain.exit_code == 0
    assert "\x1b[" not in plain.output
    assert "claude --resume sid-1" in plain.output
