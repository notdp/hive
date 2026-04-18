import json
from types import SimpleNamespace

import pytest

from hive import bus
import hive.cli as cli_module
from hive.cli import cli

FIXED_ID = bus.format_msg_id(1)


def _write_artifact(tmp_path, name: str = "details.md", content: str = "details") -> str:
    path = tmp_path / name
    path.write_text(content)
    return str(path)


def _patch_ack(monkeypatch):
    """Disable ACK resolution so tests don't need a real transcript.

    Also collapses the send-grace window to 0 so the loop runs exactly
    once instead of polling for 3s. The real `_observe_send_grace`
    still executes — it hits the transcript check, runs the probe, and
    captures its result — which keeps the regression surface intact
    for tests that happen to route through a transcript-less fake
    agent. Tests that genuinely exercise grace timing set up their own
    ack/probe and don't call this helper.
    """
    monkeypatch.setattr(
        "hive.sidecar._resolve_ack_baseline",
        lambda _target: (_ for _ in ()).throw(RuntimeError("no transcript")),
        raising=False,
    )
    monkeypatch.setattr("hive.sidecar.SEND_GRACE_TIMEOUT", 0.0)


def _patch_sidecar_requests(monkeypatch, team_obj, *, pending=None):
    if pending is None:
        pending = {}

    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)

    def _resolve_live_agent(_team_name: str, agent_name: str):
        agent = team_obj.get(agent_name)
        if not agent.is_alive():
            raise RuntimeError(f"agent '{agent_name}' is not alive")
        return team_obj, agent

    monkeypatch.setattr("hive.sidecar._resolve_live_agent", _resolve_live_agent)
    monkeypatch.setattr(
        "hive.sidecar._agent_runtime_payload",
        lambda _pane_id: {
            "alive": True,
            "turnPhase": "turn_closed",
        },
    )

    def _request_team_runtime(_workspace: str, *, team: str):
        from hive.sidecar import _agent_runtime_payload

        members_payload = {}
        member_map = getattr(team_obj, "members", None)
        if not isinstance(member_map, dict):
            member_map = getattr(team_obj, "agents", None)
        if not isinstance(member_map, dict):
            member_map = {}
        for name, agent in member_map.items():
            payload = _agent_runtime_payload(getattr(agent, "pane_id", "") or "")
            payload["alive"] = bool(agent.is_alive())
            members_payload[name] = payload
        return {"ok": True, "team": team, "members": members_payload}

    def _request_send(
        workspace: str,
        *,
        team: str,
        sender_agent: str,
        sender_pane: str,
        target_agent: str,
        body: str,
        artifact: str = "",
        reply_to: str = "",
        wait: bool = False,
    ):
        from hive.sidecar import _send_payload

        try:
            return _send_payload(
                workspace=workspace,
                team_name=team,
                pending=pending,
                sender_agent=sender_agent,
                sender_pane=sender_pane,
                target_agent=target_agent,
                body=body,
                artifact=artifact,
                reply_to=reply_to,
                wait=wait,
            )
        except Exception as exc:
            return {"ok": False, "error": str(exc)}

    def _request_answer(
        workspace: str,
        *,
        team: str,
        sender_agent: str,
        target_agent: str,
        text: str,
    ):
        from hive.sidecar import _answer_payload

        try:
            return _answer_payload(
                workspace=workspace,
                team_name=team,
                sender_agent=sender_agent,
                target_agent=target_agent,
                text=text,
            )
        except Exception as exc:
            return {"ok": False, "error": str(exc)}

    monkeypatch.setattr("hive.sidecar.request_send", _request_send)
    monkeypatch.setattr("hive.sidecar.request_answer", _request_answer)
    monkeypatch.setattr("hive.sidecar.request_team_runtime", _request_team_runtime)
    return pending



def test_send_injects_hive_envelope_into_target_pane(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    artifact = _write_artifact(tmp_path, "review.md", "review request")
    bus.init_workspace(workspace)

    sent: list[str] = []

    class _FakeAgent:
        pane_id = "%99"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            sent.append(text)

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, name: str):
            assert name == "gpt"
            return _FakeAgent()

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane_id, profile=None: "sess-1")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(
        cli,
        [
            "send",
            "gpt",
            "please review this",
            "--artifact",
            artifact,
        ],
    )

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["from"] == "claude"
    assert payload["to"] == "gpt"
    assert payload["artifact"] == artifact
    assert "summary" not in payload
    assert payload["delivery"] == "pending"
    assert payload["meaning"] == "Submit completed and background delivery tracking continues."
    assert payload["recommendedAction"] == "continue"
    assert "injectStatus" not in payload
    assert "turnObserved" not in payload
    assert "followUp" not in payload
    assert len(sent) == 1
    assert payload["msgId"] == FIXED_ID
    assert sent == [f"<HIVE from=claude to=gpt msgId={FIXED_ID} artifact={artifact}>\nplease review this\n</HIVE>"]
    assert len(bus.read_all_events(workspace)) == 1
    assert bus.read_all_events(workspace)[0]["intent"] == "send"



def test_send_does_not_defer_root_send_when_turn_phase_is_unknown(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "unknown.md", "full details")

    sent: list[str] = []

    class _FakeAgent:
        pane_id = "%99"
        cli = "droid"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            sent.append(text)

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, name: str):
            assert name == "gpt"
            return _FakeAgent()

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane_id, profile=None: "sess-1")
    _patch_sidecar_requests(monkeypatch, team)
    monkeypatch.setattr(
        "hive.sidecar._agent_runtime_payload",
        lambda _pane_id: {
            "alive": True,
            "turnPhase": "assistant_text_idle",
        },
    )

    result = runner.invoke(cli, ["send", "gpt", "please review this", "--artifact", artifact])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["delivery"] != "deferred"
    assert len(sent) == 1
    assert sent[0].startswith("<HIVE from=claude to=gpt ")


def test_send_busy_unknown_root_forks_target_and_primes_clone_context(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "forked.md", "full details")

    class _FakeAgent:
        def __init__(self, name: str, pane_id: str, *, cli: str = "claude") -> None:
            self.name = name
            self.pane_id = pane_id
            self.cli = cli
            self.sent: list[str] = []

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            self.sent.append(text)

    class _FakeTeam:
        def __init__(self) -> None:
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"
            self.lead_name = "orch"
            self.members = {"gpt": _FakeAgent("gpt", "%99")}

        def get(self, name: str):
            return self.members[name]

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane_id, profile=None: "sess-1")
    _patch_sidecar_requests(monkeypatch, team)

    def _runtime_payload(pane_id: str):
        if pane_id == "%99":
            return {
                "alive": True,
                "busy": True,
                "inputState": "ready",
                "turnPhase": "assistant_text_idle",
            }
        return {
            "alive": True,
            "busy": False,
            "inputState": "ready",
            "turnPhase": "task_closed",
        }

    monkeypatch.setattr("hive.sidecar._agent_runtime_payload", _runtime_payload)

    def _fork_registered_agent(*_args, join_as: str, boundary_prompt: str = "", **_kwargs):
        clone = _FakeAgent(join_as, "%41")
        if boundary_prompt:
            clone.send(boundary_prompt)
        team.members[join_as] = clone
        return clone, "%41"

    monkeypatch.setattr("hive.cli._fork_registered_agent", _fork_registered_agent)

    result = runner.invoke(cli, ["send", "gpt", "please review this", "--artifact", artifact])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["requestedTo"] == "gpt"
    assert payload["effectiveTarget"] == "gpt-c1"
    assert payload["routingMode"] == "fork_handoff"
    assert payload["routingReason"] == "active_turn_fork"
    assert payload["forkedFromPane"] == "%99"
    assert payload["forkedToPane"] == "%41"
    assert payload["to"] == "gpt-c1"
    assert payload["delivery"] != "deferred"
    assert "deferredPolicy" not in payload

    original = team.members["gpt"]
    clone = team.members["gpt-c1"]
    assert original.sent == []
    assert len(clone.sent) == 2
    assert clone.sent[0].startswith("<HIVE-SYSTEM type=busy-fork target=gpt clone=gpt-c1>")
    assert "FORK BOUNDARY: you are the fork 'gpt-c1'" in clone.sent[0]
    assert "Do not continue or re-execute the original agent's pending work." in clone.sent[0]
    assert clone.sent[1] == f"<HIVE from=claude to=gpt-c1 msgId={FIXED_ID} artifact={artifact}>\nplease review this\n</HIVE>"

    events = bus.read_all_events(workspace)
    assert len(events) == 1
    assert events[0]["to"] == "gpt-c1"


def test_send_busy_safe_root_does_not_fork_and_direct_sends_to_target(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "busy-safe.md", "full details")

    class _FakeAgent:
        def __init__(self, name: str, pane_id: str, *, cli: str = "claude") -> None:
            self.name = name
            self.pane_id = pane_id
            self.cli = cli
            self.sent: list[str] = []

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            self.sent.append(text)

    class _FakeTeam:
        def __init__(self) -> None:
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"
            self.lead_name = "orch"
            self.members = {"gpt": _FakeAgent("gpt", "%99")}

        def get(self, name: str):
            return self.members[name]

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane_id, profile=None: "sess-1")
    _patch_sidecar_requests(monkeypatch, team)

    def _runtime_payload(pane_id: str):
        return {
            "alive": True,
            "busy": True,
            "inputState": "ready",
            "turnPhase": "turn_closed",
        }

    monkeypatch.setattr("hive.sidecar._agent_runtime_payload", _runtime_payload)

    def _fork_registered_agent(*_args, **_kwargs):
        raise AssertionError("fork should not be invoked when turnPhase indicates turn closure")

    monkeypatch.setattr("hive.cli._fork_registered_agent", _fork_registered_agent)

    result = runner.invoke(cli, ["send", "gpt", "please review this", "--artifact", artifact])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["to"] == "gpt"
    assert payload["delivery"] != "deferred"
    assert "routingMode" not in payload
    assert "routingReason" not in payload
    assert "effectiveTarget" not in payload
    assert "deferredPolicy" not in payload

    original = team.members["gpt"]
    assert "gpt-c1" not in team.members
    assert len(original.sent) == 1
    assert original.sent[0] == f"<HIVE from=claude to=gpt msgId={FIXED_ID} artifact={artifact}>\nplease review this\n</HIVE>"

    events = bus.read_all_events(workspace)
    assert len(events) == 1
    assert events[0]["to"] == "gpt"


def test_send_busy_root_forks_even_when_turn_phase_is_unsafe(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "busy-only.md", "full details")

    class _FakeAgent:
        def __init__(self, name: str, pane_id: str, *, cli: str = "claude") -> None:
            self.name = name
            self.pane_id = pane_id
            self.cli = cli
            self.sent: list[str] = []

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            self.sent.append(text)

    class _FakeTeam:
        def __init__(self) -> None:
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"
            self.lead_name = "orch"
            self.members = {"gpt": _FakeAgent("gpt", "%99")}

        def get(self, name: str):
            return self.members[name]

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane_id, profile=None: "sess-1")
    _patch_sidecar_requests(monkeypatch, team)

    def _runtime_payload(pane_id: str):
        if pane_id == "%99":
            return {
                "alive": True,
                "busy": True,
                "inputState": "waiting_user",
                "turnPhase": "tool_open",
            }
        return {"alive": True, "busy": False}

    monkeypatch.setattr("hive.sidecar._agent_runtime_payload", _runtime_payload)

    def _fork_registered_agent(*_args, join_as: str, boundary_prompt: str = "", **_kwargs):
        clone = _FakeAgent(join_as, "%41")
        if boundary_prompt:
            clone.send(boundary_prompt)
        team.members[join_as] = clone
        return clone, "%41"

    monkeypatch.setattr("hive.cli._fork_registered_agent", _fork_registered_agent)

    result = runner.invoke(cli, ["send", "gpt", "please review this", "--artifact", artifact])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["effectiveTarget"] == "gpt-c1"
    assert payload["routingMode"] == "fork_handoff"
    assert payload["routingReason"] == "active_turn_fork"
    assert payload["delivery"] != "deferred"
    assert "deferredPolicy" not in payload


@pytest.mark.parametrize(
    "reason",
    ["user_prompt_pending", "tool_result_pending_reply", "tool_open"],
)
def test_send_false_busy_with_active_turn_reason_forks(runner, configure_hive_home, monkeypatch, tmp_path, reason):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, f"false-busy-{reason}.md", "full details")

    class _FakeAgent:
        def __init__(self, name: str, pane_id: str, *, cli: str = "claude") -> None:
            self.name = name
            self.pane_id = pane_id
            self.cli = cli
            self.sent: list[str] = []

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            self.sent.append(text)

    class _FakeTeam:
        def __init__(self) -> None:
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"
            self.lead_name = "orch"
            self.members = {"gpt": _FakeAgent("gpt", "%99")}

        def get(self, name: str):
            return self.members[name]

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane_id, profile=None: "sess-1")
    _patch_sidecar_requests(monkeypatch, team)

    def _runtime_payload(pane_id: str):
        if pane_id == "%99":
            return {
                "alive": True,
                "busy": False,
                "inputState": "ready",
                "turnPhase": reason,
            }
        return {"alive": True, "busy": False}

    monkeypatch.setattr("hive.sidecar._agent_runtime_payload", _runtime_payload)

    def _fork_registered_agent(*_args, join_as: str, boundary_prompt: str = "", **_kwargs):
        clone = _FakeAgent(join_as, "%41")
        if boundary_prompt:
            clone.send(boundary_prompt)
        team.members[join_as] = clone
        return clone, "%41"

    monkeypatch.setattr("hive.cli._fork_registered_agent", _fork_registered_agent)

    result = runner.invoke(cli, ["send", "gpt", "please review this", "--artifact", artifact])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["effectiveTarget"] == "gpt-c1"
    assert payload["routingMode"] == "fork_handoff"
    assert payload["routingReason"] == "active_turn_fork"
    assert payload["delivery"] != "deferred"


@pytest.mark.parametrize(
    "reason",
    ["assistant_text_idle", "unknown_evidence"],
)
def test_send_false_busy_with_non_active_turn_reason_does_not_fork(runner, configure_hive_home, monkeypatch, tmp_path, reason):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, f"false-busy-{reason}.md", "full details")

    class _FakeAgent:
        def __init__(self, name: str, pane_id: str, *, cli: str = "claude") -> None:
            self.name = name
            self.pane_id = pane_id
            self.cli = cli
            self.sent: list[str] = []

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            self.sent.append(text)

    class _FakeTeam:
        def __init__(self) -> None:
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"
            self.lead_name = "orch"
            self.members = {"gpt": _FakeAgent("gpt", "%99")}

        def get(self, name: str):
            return self.members[name]

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane_id, profile=None: "sess-1")
    _patch_sidecar_requests(monkeypatch, team)

    def _runtime_payload(pane_id: str):
        return {
            "alive": True,
            "busy": False,
            "inputState": "ready",
            "turnPhase": reason,
        }

    monkeypatch.setattr("hive.sidecar._agent_runtime_payload", _runtime_payload)

    def _fork_registered_agent(*_args, **_kwargs):
        raise AssertionError(f"fork should not be invoked when busy=False and turnPhase={reason}")

    monkeypatch.setattr("hive.cli._fork_registered_agent", _fork_registered_agent)

    result = runner.invoke(cli, ["send", "gpt", "please review this", "--artifact", artifact])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["to"] == "gpt"
    assert "routingMode" not in payload
    assert "routingReason" not in payload
    assert "effectiveTarget" not in payload


def test_send_busy_root_falls_back_to_direct_send_when_fork_is_unavailable(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "fallback.md", "full details")

    class _FakeAgent:
        def __init__(self, name: str, pane_id: str, *, cli: str = "droid") -> None:
            self.name = name
            self.pane_id = pane_id
            self.cli = cli
            self.sent: list[str] = []

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            self.sent.append(text)

    class _FakeTeam:
        def __init__(self) -> None:
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"
            self.lead_name = "orch"
            self.members = {"gpt": _FakeAgent("gpt", "%99")}

        def get(self, name: str):
            return self.members[name]

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    _patch_sidecar_requests(monkeypatch, team)
    monkeypatch.setattr(
        "hive.sidecar._agent_runtime_payload",
        lambda _pane_id: {"alive": True, "busy": True},
    )

    def _fork_registered_agent(*_args, **_kwargs):
        raise SystemExit(1)

    monkeypatch.setattr("hive.cli._fork_registered_agent", _fork_registered_agent)

    result = runner.invoke(cli, ["send", "gpt", "please review this", "--artifact", artifact])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["to"] == "gpt"
    assert "effectiveTarget" not in payload
    assert "routingMode" not in payload


def _reply_fake_team(workspace, *, sent_transcript):
    class _FakeAgent:
        pane_id = "%99"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            sent_transcript.append(text)

    class _FakeTeam:
        def __init__(self) -> None:
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    return _FakeTeam()


def test_reply_auto_fills_reply_to_from_latest_inbound(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    inbound = bus.write_send_event(workspace, from_agent="dodo", to_agent="orch", body="see patch")

    sent: list[str] = []
    team = _reply_fake_team(workspace, sent_transcript=sent)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["reply", "dodo", "ack, looking"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["from"] == "orch"
    assert payload["to"] == "dodo"
    assert payload["autoReplyTo"] == inbound.msg_id
    events = bus.read_all_events(workspace)
    outbound = [event for event in events if event.get("from") == "orch" and event.get("to") == "dodo"]
    assert len(outbound) == 1
    assert outbound[0].get("inReplyTo") == inbound.msg_id


def test_reply_fails_when_no_inbound_from_agent(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    sent: list[str] = []
    team = _reply_fake_team(workspace, sent_transcript=sent)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["reply", "dodo", "late answer"])

    assert result.exit_code != 0
    assert "no recent message from 'dodo'" in result.output
    assert sent == []


def test_reply_fails_when_latest_inbound_already_replied(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    inbound = bus.write_send_event(workspace, from_agent="dodo", to_agent="orch", body="see patch")
    bus.write_send_event(
        workspace,
        from_agent="orch",
        to_agent="dodo",
        body="thanks, looking",
        reply_to=inbound.msg_id,
    )

    sent: list[str] = []
    team = _reply_fake_team(workspace, sent_transcript=sent)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["reply", "dodo", "one more thing"])

    assert result.exit_code != 0
    assert "already replied to" in result.output
    assert "pass --reply-to explicitly" in result.output


def test_reply_honors_explicit_reply_to_override(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    first = bus.write_send_event(workspace, from_agent="dodo", to_agent="orch", body="older msg")
    second = bus.write_send_event(workspace, from_agent="dodo", to_agent="orch", body="newer msg")
    bus.write_send_event(
        workspace,
        from_agent="orch",
        to_agent="dodo",
        body="auto",
        reply_to=second.msg_id,
    )

    sent: list[str] = []
    team = _reply_fake_team(workspace, sent_transcript=sent)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["reply", "dodo", "on older thread", "--reply-to", first.msg_id])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert "autoReplyTo" not in payload
    events = bus.read_all_events(workspace)
    latest_outbound = [
        event for event in events if event.get("from") == "orch" and event.get("to") == "dodo"
    ][-1]
    assert latest_outbound.get("inReplyTo") == first.msg_id


def test_send_rejects_legacy_to_option_with_positional_hint(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    called = []
    monkeypatch.setattr(
        "hive.cli._resolve_scoped_team",
        lambda _team, required=True: called.append("resolved") or ("team-x", object()),
    )

    result = runner.invoke(cli, ["send", "--to", "gpt", "--msg", "hello"])

    assert result.exit_code != 0
    assert "hive send takes positional args" in result.output
    assert "Drop --to/--msg" in result.output
    assert called == []  # Guard must short-circuit before touching the team.


def test_send_without_agent_surfaces_usage_hint(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    called = []
    monkeypatch.setattr(
        "hive.cli._resolve_scoped_team",
        lambda _team, required=True: called.append("resolved") or ("team-x", object()),
    )

    result = runner.invoke(cli, ["send"])

    assert result.exit_code != 0
    assert "hive send requires <agent>" in result.output
    assert "Drop --to/--msg" not in result.output
    assert called == []


def test_reply_rejects_legacy_msg_option_with_positional_hint(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    called = []
    monkeypatch.setattr(
        "hive.cli._resolve_scoped_team",
        lambda _team, required=True: called.append("resolved") or ("team-x", object()),
    )

    result = runner.invoke(cli, ["reply", "dodo", "--msg", "hello"])

    assert result.exit_code != 0
    assert "hive reply takes positional args" in result.output
    assert "Drop --to/--msg" in result.output
    assert called == []


def test_send_requires_tmux(runner, monkeypatch):
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)

    result = runner.invoke(cli, ["send", "gpt", "hello from current context"])

    assert result.exit_code != 0
    assert "requires tmux" in result.output


def test_send_requires_live_registered_agent(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "live-agent.md")

    class _DeadAgent:
        def is_alive(self) -> bool:
            return False

        def send(self, text: str) -> None:
            raise AssertionError("should not send")

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, _name: str):
            return _DeadAgent()

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["send", "gpt", "hello", "--artifact", artifact])
    assert result.exit_code != 0
    assert "not alive" in result.output


def test_inject_delegates_to_agent(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    sent: list[str] = []

    class _FakeAgent:
        def send(self, text: str) -> None:
            sent.append(text)

    class _FakeTeam:
        tmux_session = "dev"
        tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(cli, ["inject", "claude", "plain prompt"])
    assert result.exit_code == 0
    assert sent == ["plain prompt"]
    assert "Injected raw input into claude." in result.output


def test_capture_reads_agent_output(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    class _FakeAgent:
        def capture(self, lines: int) -> str:
            assert lines == 12
            return "captured output"

    class _FakeTeam:
        tmux_session = "dev"
        tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(cli, ["capture", "claude", "--lines", "12"])
    assert result.exit_code == 0
    assert result.output.strip() == "captured output"


def test_interrupt_delegates_to_agent(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    calls: list[str] = []

    class _FakeAgent:
        def interrupt(self) -> None:
            calls.append("interrupt")

    class _FakeTeam:
        tmux_session = "dev"
        tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(cli, ["interrupt", "claude"])
    assert result.exit_code == 0
    assert calls == ["interrupt"]


def test_kill_removes_agent(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    killed: list[str] = []

    class _FakeAgent:
        def kill(self) -> None:
            killed.append("killed")

    class _FakeTeam:
        tmux_session = "dev"
        tmux_window = "dev:0"
        agents = {"opus": _FakeAgent()}

        def get(self, name: str):
            return self.agents[name]

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(cli, ["kill", "opus"])
    assert result.exit_code == 0
    assert killed == ["killed"]
    assert "Killed opus." in result.output
    assert "opus" not in _FakeTeam.agents


def test_notify_uses_current_pane_by_default(runner, monkeypatch):
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%72")
    monkeypatch.setattr(
        "hive.cli.notify_ui.notify",
        lambda message, pane_id, seconds, highlight, window_status: {
            "message": message,
            "paneId": pane_id,
            "seconds": seconds,
            "highlight": highlight,
            "windowStatus": window_status,
        },
    )

    result = runner.invoke(cli, ["notify", "按 Tab 和我对话"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload == {
        "message": "按 Tab 和我对话",
        "paneId": "%72",
        "seconds": 12,
        "highlight": False,
        "windowStatus": True,
    }


def test_notify_fails_outside_tmux(runner, monkeypatch):
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "")

    result = runner.invoke(cli, ["notify", "需要确认"])

    assert result.exit_code == 1
    assert "requires tmux" in result.output


def test_internal_notify_hook_command_delegates_to_notify_hook_main(runner, monkeypatch):
    monkeypatch.setattr("hive.cli.notify_hook.main", lambda: 0)

    result = runner.invoke(cli, ["_notify-hook"])

    assert result.exit_code == 0


# --- ACK-specific tests ---


def test_send_ack_confirmed(runner, configure_hive_home, monkeypatch, tmp_path):
    """ACK returns confirmed when nonce appears in transcript."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "ack-confirmed.md")

    transcript = tmp_path / "session.jsonl"
    transcript.write_text("")

    sent: list[str] = []

    class _FakeAgent:
        pane_id = "%99"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            sent.append(text)
            # Simulate CLI accepting input — write a user turn with the id.
            transcript.write_text(
                '{"type": "user", "message": {"role": "user", "content": "'
                + f"id: {FIXED_ID}"
                + '"}}\n'
            )

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, name: str):
            return _FakeAgent()

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.sidecar._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["send", "gpt", "test", "--artifact", artifact, "--wait"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["delivery"] == "success"
    assert payload["meaning"] == "Target pane rendered the message id; delivery confirmed."
    assert payload["recommendedAction"] == "continue"
    assert "followUp" not in payload


def test_send_ack_unconfirmed_on_timeout(runner, configure_hive_home, monkeypatch, tmp_path):
    """ACK returns unconfirmed when transcript never shows the nonce."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "ack-timeout.md")

    transcript = tmp_path / "session.jsonl"
    transcript.write_text("")

    class _FakeAgent:
        pane_id = "%99"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            pass

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, name: str):
            return _FakeAgent()

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.sidecar._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)
    monkeypatch.setattr("hive.sidecar._wait_for_delivery_confirmation", lambda **_kw: "")
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    # The test drives the unconfirmed branch via _wait_for_delivery_confirmation; we
    # don't need the grace loop to wall-clock through its 3s before that runs.
    monkeypatch.setattr("hive.sidecar.SEND_GRACE_TIMEOUT", 0.0)
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["send", "gpt", "test", "--artifact", artifact, "--wait"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["delivery"] == "failed"
    assert payload["meaning"] == "Delivery failed: either submit errored or the target pane never rendered the message id."
    assert payload["recommendedAction"] == "retry"
    assert "followUp" not in payload


def test_send_wait_confirms_via_stream_when_transcript_silent(runner, configure_hive_home, monkeypatch, tmp_path):
    """--wait branch confirms via pane output stream when transcript never carries msgId."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    transcript = tmp_path / "session.jsonl"
    transcript.write_text("")
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "stream-confirm.md")

    class _FakeAgent:
        pane_id = "%99"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            pass

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, name: str):
            return _FakeAgent()

    team = _FakeTeam()

    class _FakeMonitor:
        def saw_msg_id(self, pane_id: str, msg_id: str) -> bool:
            return bool(pane_id) and bool(msg_id)

        def is_busy(self, pane_id: str, *, threshold_seconds: float) -> bool:
            return False

    monkeypatch.setattr("hive.sidecar._OUTPUT_BUSY_MONITOR", _FakeMonitor(), raising=False)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.sidecar._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    monkeypatch.setattr("hive.sidecar.SEND_GRACE_TIMEOUT", 0.0)
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["send", "gpt", "test", "--artifact", artifact, "--wait"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["delivery"] == "success"


def test_check_pending_confirms_via_stream_output():
    """Background pending check accepts stream output as delivery confirmation."""
    import time

    from hive import sidecar

    class _FakeMonitor:
        def saw_msg_id(self, pane_id: str, msg_id: str) -> bool:
            return pane_id == "%99" and msg_id == "abc1"

    original = sidecar._OUTPUT_BUSY_MONITOR
    sidecar._set_output_busy_monitor(_FakeMonitor())
    try:
        record: dict[str, object] = {
            "msgId": "abc1",
            "targetPane": "%99",
            "targetTranscript": "",
            "baseline": 0,
            "queueProbeText": "",
            "targetCli": "codex",
            "deadlineAt": time.time() + 60.0,
        }
        result = sidecar._check_pending(record)
        assert result == "success"
        assert record.get("confirmationSource") == "stream"
    finally:
        sidecar._set_output_busy_monitor(original)


def test_control_mode_monitor_saw_msg_id_via_parsed_payload():
    """Monitor's rolling buffer captures payload substring for msgId lookups."""
    from hive.tmux import ControlModeOutputMonitor, parse_control_mode_output

    pane_id, payload = parse_control_mode_output("%output %99 <HIVE msgId=abc1 from=x to=y />")
    assert pane_id == "%99"
    assert "msgId=abc1" in payload

    monitor = ControlModeOutputMonitor("dev")
    monitor._append_output(pane_id, payload)
    assert monitor.saw_msg_id("%99", "abc1") is True
    assert monitor.saw_msg_id("%99", "zzzz") is False
    assert monitor.saw_msg_id("%77", "abc1") is False


def test_parse_control_mode_output_decodes_octal_escape():
    """Decode \\NNN sequences so all-digit msgIds don't false-match escape boundaries."""
    from hive.tmux import parse_control_mode_output

    # Raw line has \012 (LF) followed by literal '3'; undecoded substring would contain '0123'.
    pane_id, payload = parse_control_mode_output("%output %99 before\\0123after")
    assert pane_id == "%99"
    assert "0123" not in payload
    assert payload == "before\n3after"


def test_parse_control_mode_output_strips_extended_prefix():
    """%extended-output carries 'age ... : payload'; strip up to first colon."""
    from hive.tmux import parse_control_mode_output

    pane_id, payload = parse_control_mode_output("%extended-output %99 1234 : msgId=abc1 body")
    assert pane_id == "%99"
    assert payload == "msgId=abc1 body"


def test_send_ack_skipped_when_transcript_unresolvable(runner, configure_hive_home, monkeypatch, tmp_path):
    """ACK gracefully degrades to skipped when transcript cannot be found."""
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "ack-skipped.md")

    class _FakeAgent:
        pane_id = "%99"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            pass

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, name: str):
            return _FakeAgent()

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["send", "gpt", "test", "--artifact", artifact])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["delivery"] == "pending"
    assert payload["meaning"] == "Submit completed and background delivery tracking continues."
    assert payload["recommendedAction"] == "continue"
    assert "injectStatus" not in payload
    assert "followUp" not in payload


def test_send_async_pending_enqueues_sidecar(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "async-pending.md")

    transcript = tmp_path / "session.jsonl"
    transcript.write_text("")

    class _FakeAgent:
        pane_id = "%99"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            pass

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, name: str):
            return _FakeAgent()

    pending = {}
    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.sidecar._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    # Nothing confirms during grace; collapse the 3s window so tests don't wait.
    monkeypatch.setattr("hive.sidecar.SEND_GRACE_TIMEOUT", 0.0)
    _patch_sidecar_requests(monkeypatch, team, pending=pending)

    result = runner.invoke(cli, ["send", "gpt", "test", "--artifact", artifact])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["delivery"] == "pending"
    assert payload["meaning"] == "Submit completed and background delivery tracking continues."
    assert payload["recommendedAction"] == "continue"
    assert "followUp" not in payload
    assert len(pending) == 1


def test_send_inject_failure_no_sidecar(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "inject-failure.md")

    transcript = tmp_path / "session.jsonl"
    transcript.write_text("")

    class _BrokenAgent:
        pane_id = "%99"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            raise RuntimeError("boom")

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, name: str):
            return _BrokenAgent()

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.sidecar._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["send", "gpt", "test", "--artifact", artifact])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["delivery"] == "failed"
    assert payload["meaning"] == "Delivery failed: either submit errored or the target pane never rendered the message id."
    assert payload["recommendedAction"] == "retry"
    assert "injectStatus" not in payload
    assert "turnObserved" not in payload
    assert "observerPid" not in payload
    assert "followUp" not in payload


def test_send_help_explains_delivery_states(runner):
    result = runner.invoke(cli, ["send", "--help"])
    help_text = " ".join(result.output.split())

    assert result.exit_code == 0
    assert "`success`: target pane rendered the msgId" in help_text
    assert "`pending`: submit completed; background tracking continues" in help_text
    assert "`failed`: submit errored OR target pane never rendered msgId" in help_text
    assert "`queued`" not in help_text
    assert "`unconfirmed`" not in help_text
    assert "`deferred`" not in help_text
    assert "--force" not in help_text


# --- Send gate tests ---


def _gate_test_setup(monkeypatch, tmp_path, transcript_records=None):
    """Common setup for gate tests. Returns (workspace, transcript, sent list)."""
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    transcript = tmp_path / "session.jsonl"
    if transcript_records is not None:
        transcript.write_text(
            "\n".join(json.dumps(r) for r in transcript_records) + "\n"
        )
    else:
        transcript.write_text("")

    sent: list[str] = []

    class _FakeAgent:
        pane_id = "%99"
        name = "gpt"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            sent.append(text)

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.sidecar._resolve_ack_baseline", lambda _target: (transcript, transcript.stat().st_size), raising=False)
    monkeypatch.setattr("hive.sidecar._wait_for_delivery_confirmation", lambda **_kw: "")
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    # Gate tests only care about the gate projection; collapse the 3s grace loop.
    monkeypatch.setattr("hive.sidecar.SEND_GRACE_TIMEOUT", 0.0)
    _patch_sidecar_requests(monkeypatch, team)

    return workspace, transcript, sent


def test_send_blocked_by_gate(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    artifact = _write_artifact(tmp_path, "gate-blocked.md")
    _gate_test_setup(monkeypatch, tmp_path, transcript_records=[
        {"type": "user", "message": {"role": "user", "content": "do something"}},
        {
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "name": "AskUserQuestion", "input": {"question": "proceed?"}},
                ],
            },
        },
    ])

    result = runner.invoke(cli, ["send", "gpt", "hello", "--artifact", artifact])

    assert result.exit_code != 0
    assert "waiting for a user answer" in result.output


def test_gate_fail_open_no_transcript(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "gate-open.md")

    class _FakeAgent:
        pane_id = "%99"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            pass

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["send", "gpt", "hello", "--artifact", artifact])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["delivery"] == "pending"
    assert payload["gate"] == "skipped"
    assert "gateNote" in payload
    assert "injectStatus" not in payload


def test_gate_clear_appears_in_send_output(runner, configure_hive_home, monkeypatch, tmp_path):
    """When transcript resolves successfully and gate is clear, output contains gate=clear."""
    configure_hive_home()
    artifact = _write_artifact(tmp_path, "gate-clear.md")
    workspace, transcript, sent = _gate_test_setup(monkeypatch, tmp_path, transcript_records=[
        {"type": "user", "message": {"role": "user", "content": "hello"}},
    ])

    result = runner.invoke(cli, ["send", "gpt", "hello", "--artifact", artifact])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["gate"] == "clear"
    assert "gateNote" not in payload


# --- Answer command tests ---


def test_answer_when_not_waiting_fails(runner, configure_hive_home, monkeypatch, tmp_path):
    """answer should fail when the target is not in waiting state."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    transcript = tmp_path / "session.jsonl"
    transcript.write_text(
        json.dumps({"type": "user", "message": {"role": "user", "content": "hello"}}) + "\n"
    )

    class _FakeAgent:
        pane_id = "%99"
        name = "gpt"
        cli = "claude"

        def is_alive(self) -> bool:
            return True

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.sidecar._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["answer", "gpt", "yes"])

    assert result.exit_code != 0
    assert "not waiting for an answer" in result.output


def test_answer_when_waiting_injects_text(runner, configure_hive_home, monkeypatch, tmp_path):
    """answer should inject text when the target is waiting."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    transcript = tmp_path / "session.jsonl"
    transcript.write_text(
        json.dumps({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "name": "AskUserQuestion", "input": {"question": "proceed?"}},
                ],
            },
        }) + "\n"
    )

    injected: list[tuple[str, str]] = []

    class _FakeAgent:
        pane_id = "%99"
        name = "gpt"
        cli = "claude"

        def is_alive(self) -> bool:
            return True

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    def fake_submit(pane_id, text, cli):
        injected.append((pane_id, text))
        # Simulate the answer being accepted — write a user turn.
        transcript.write_text(
            transcript.read_text()
            + json.dumps({"type": "user", "message": {"role": "user", "content": "yes"}}) + "\n"
        )

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.sidecar._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    monkeypatch.setattr("hive.agent._submit_interactive_text", fake_submit)
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["answer", "gpt", "yes"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["ack"] == "confirmed"
    assert payload["answer"] == "yes"
    assert payload["question"] == "proceed?"
    assert len(injected) == 1
    assert injected[0] == ("%99", "yes")
    # Event was written
    events = bus.read_all_events(workspace)
    assert len(events) == 1
    assert events[0]["intent"] == "answer"
