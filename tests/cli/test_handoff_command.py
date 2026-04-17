import json

from hive import bus
from hive.cli import cli


def _patch_ack(monkeypatch):
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

    monkeypatch.setattr("hive.sidecar.request_send", _request_send)
    return pending


class _FakeAgent:
    def __init__(self, name: str, pane_id: str, *, alive: bool = True):
        self.name = name
        self.pane_id = pane_id
        self.alive = alive
        self.sent: list[str] = []

    def is_alive(self) -> bool:
        return self.alive

    def send(self, text: str) -> None:
        self.sent.append(text)


class _FakeTeam:
    def __init__(self, workspace: str):
        self.workspace = workspace
        self.name = "team-x"
        self.tmux_session = "dev"
        self.tmux_window = "dev:0"
        self.agents: dict[str, _FakeAgent] = {}

    def get(self, name: str):
        if name not in self.agents:
            raise KeyError(f"Agent '{name}' not found")
        return self.agents[name]


def test_handoff_direct_uses_send_path_for_delegate_and_announce(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    artifact = tmp_path / "task.md"
    artifact.write_text("review this")
    bus.init_workspace(workspace)
    inbound = bus.write_send_event(workspace, from_agent="lulu", to_agent="orch", body="need help")

    team = _FakeTeam(str(workspace))
    team.agents["dodo"] = _FakeAgent("dodo", "%21")
    team.agents["lulu"] = _FakeAgent("lulu", "%22")
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["handoff", "dodo", "--artifact", str(artifact), "--note", "review the patch"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["mode"] == "direct"
    assert payload["target"] == "dodo"
    assert payload["targetPane"] == "%21"
    assert payload["originalSender"] == "lulu"
    assert payload["anchorMsgId"] == inbound.msg_id
    assert payload["delegate"]["to"] == "dodo"
    assert payload["delegate"]["artifact"] == str(artifact)
    assert payload["announce"]["to"] == "lulu"
    assert payload["announce"]["state"] == "pending"

    events = bus.read_all_events(workspace)
    delegate = [event for event in events if event.get("intent") == "send" and event.get("to") == "dodo"][-1]
    announce = [event for event in events if event.get("intent") == "send" and event.get("to") == "lulu"][-1]
    handoff = [event for event in events if event.get("intent") == "handoff"][-1]

    assert "Original sender: lulu" in delegate["body"]
    assert f"Anchor msgId: {inbound.msg_id}" in delegate["body"]
    assert "Note: review the patch" in delegate["body"]
    assert announce["inReplyTo"] == inbound.msg_id
    assert handoff["msgId"] == payload["handoffId"]
    assert handoff["metadata"] == {
        "anchorMsgId": inbound.msg_id,
        "mode": "direct",
        "delegateMsgId": payload["delegate"]["msgId"],
        "announceMsgId": payload["announce"]["msgId"],
    }


def test_handoff_rejects_reply_to_that_is_not_current_agent_inbound(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    outbound = bus.write_send_event(workspace, from_agent="orch", to_agent="dodo", body="wrong direction")

    team = _FakeTeam(str(workspace))
    team.agents["dodo"] = _FakeAgent("dodo", "%21")
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["handoff", "dodo", "--reply-to", outbound.msg_id])

    assert result.exit_code != 0
    assert f"msgId '{outbound.msg_id}' is not an inbound send event for 'orch'" in result.output


def test_handoff_requires_explicit_spawn_or_fork_when_target_is_missing(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    bus.write_send_event(workspace, from_agent="lulu", to_agent="orch", body="need help")

    team = _FakeTeam(str(workspace))
    team.agents["lulu"] = _FakeAgent("lulu", "%22")
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["handoff", "dodo-2"])

    assert result.exit_code != 0
    assert "does not exist; pass --spawn or --fork explicitly" in result.output


def test_handoff_rejects_spawn_or_fork_for_existing_target(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    bus.write_send_event(workspace, from_agent="lulu", to_agent="orch", body="need help")

    team = _FakeTeam(str(workspace))
    team.agents["dodo"] = _FakeAgent("dodo", "%21")
    team.agents["lulu"] = _FakeAgent("lulu", "%22")
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    spawn_result = runner.invoke(cli, ["handoff", "dodo", "--spawn"])
    fork_result = runner.invoke(cli, ["handoff", "dodo", "--fork"])

    assert spawn_result.exit_code != 0
    assert "direct handoff does not accept --spawn/--fork" in spawn_result.output
    assert fork_result.exit_code != 0
    assert "direct handoff does not accept --spawn/--fork" in fork_result.output


def test_handoff_target_exists_error_wins_over_missing_anchor(runner, configure_hive_home, monkeypatch, tmp_path):
    """target-exists + --spawn/--fork must surface the dispatch error, not 'no unanswered inbound',
    when both conditions apply. Exercises the guardrail ordering: dispatch check before anchor resolution.
    """
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    # Deliberately do NOT write any inbound send event for orch, so anchor resolution would fail
    # if it ran first.

    team = _FakeTeam(str(workspace))
    team.agents["dodo"] = _FakeAgent("dodo", "%21")
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    spawn_result = runner.invoke(cli, ["handoff", "dodo", "--spawn"])

    assert spawn_result.exit_code != 0
    assert "direct handoff does not accept --spawn/--fork" in spawn_result.output
    assert "no unanswered inbound" not in spawn_result.output


def test_handoff_spawn_mode_creates_worker_then_sends_delegate_and_announce(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    inbound = bus.write_send_event(workspace, from_agent="lulu", to_agent="orch", body="need help")

    team = _FakeTeam(str(workspace))
    team.agents["lulu"] = _FakeAgent("lulu", "%22")
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")

    spawned: list[str] = []

    def _spawn_team_agent(*_args, team_name: str, agent_name: str, **_kwargs):
        spawned.append(agent_name)
        agent = _FakeAgent(agent_name, "%31")
        team.agents[agent_name] = agent
        return agent

    monkeypatch.setattr("hive.cli._spawn_team_agent", _spawn_team_agent)
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["handoff", "dodo-2", "--spawn"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert spawned == ["dodo-2"]
    assert payload["mode"] == "spawn"
    assert payload["targetPane"] == "%31"
    assert payload["delegate"]["to"] == "dodo-2"
    assert payload["announce"]["to"] == "lulu"
    events = bus.read_all_events(workspace)
    handoff = [event for event in events if event.get("intent") == "handoff"][-1]
    assert handoff["metadata"]["anchorMsgId"] == inbound.msg_id
    assert handoff["metadata"]["mode"] == "spawn"


def test_handoff_fork_mode_creates_worker_then_sends_delegate_and_announce(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    inbound = bus.write_send_event(workspace, from_agent="lulu", to_agent="orch", body="need help")

    team = _FakeTeam(str(workspace))
    team.agents["lulu"] = _FakeAgent("lulu", "%22")
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")

    forked: list[str] = []

    def _fork_registered_agent(*_args, join_as: str, **_kwargs):
        forked.append(join_as)
        agent = _FakeAgent(join_as, "%41")
        team.agents[join_as] = agent
        return agent, "%41"

    monkeypatch.setattr("hive.cli._fork_registered_agent", _fork_registered_agent)
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["handoff", "orch-2", "--fork"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert forked == ["orch-2"]
    assert payload["mode"] == "fork"
    assert payload["targetPane"] == "%41"
    assert payload["delegate"]["to"] == "orch-2"
    assert payload["announce"]["to"] == "lulu"
    events = bus.read_all_events(workspace)
    handoff = [event for event in events if event.get("intent") == "handoff"][-1]
    assert handoff["metadata"]["anchorMsgId"] == inbound.msg_id
    assert handoff["metadata"]["mode"] == "fork"


def test_handoff_treats_announce_failure_as_best_effort(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    inbound = bus.write_send_event(workspace, from_agent="lulu", to_agent="orch", body="need help")

    team = _FakeTeam(str(workspace))
    team.agents["dodo"] = _FakeAgent("dodo", "%21")
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["handoff", "dodo"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["delegate"]["to"] == "dodo"
    assert payload["announce"]["state"] == "failed"
    assert "Agent 'lulu' not found" in payload["announce"]["error"]

    handoff = [event for event in bus.read_all_events(workspace) if event.get("intent") == "handoff"][-1]
    assert handoff["metadata"] == {
        "anchorMsgId": inbound.msg_id,
        "mode": "direct",
        "delegateMsgId": payload["delegate"]["msgId"],
        "announceMsgId": "",
    }
