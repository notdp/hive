import json

from hive import bus
from hive.cli import cli

FIXED_ID = bus.format_msg_id(1)


def _write_artifact(tmp_path, name: str = "details.md", content: str = "details") -> str:
    path = tmp_path / name
    path.write_text(content)
    return str(path)


def _patch_ack(monkeypatch):
    """Disable ACK resolution and collapse the send-grace loop to a single
    iteration; see the twin helper in test_message_commands.py."""
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
            "interruptSafety": "safe",
            "safetyReason": "turn_closed",
        },
    )

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
        enforce_safety_gate: bool = False,
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
                enforce_safety_gate=enforce_safety_gate,
            )
        except Exception as exc:
            return {"ok": False, "error": str(exc)}

    monkeypatch.setattr("hive.sidecar.request_send", _request_send)
    return pending


def _fake_team(workspace, *, sent_transcript):
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


def test_send_rejects_structured_body_for_new_root(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "root-structured.md")

    sent: list[str] = []
    team = _fake_team(workspace, sent_transcript=sent)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    body = "# Findings\n- item one\n- item two"
    result = runner.invoke(cli, ["send", "dodo", body, "--artifact", artifact])

    assert result.exit_code != 0
    assert "new root send body must stay short and unstructured" in result.output
    assert sent == []


def test_reply_warns_for_fenced_block_body_but_still_replies(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    inbound = bus.write_send_event(workspace, from_agent="dodo", to_agent="orch", body="see patch")

    sent: list[str] = []
    team = _fake_team(workspace, sent_transcript=sent)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    body = "```diff\n+ one line\n```"
    result = runner.invoke(cli, ["reply", "dodo", body])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["autoReplyTo"] == inbound.msg_id
    assert "warning: body looks long or structured" in result.stderr
    assert 'hive reply <agent> "<short summary>" --artifact -' in result.stderr


def test_send_requires_artifact_for_new_root(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    sent: list[str] = []
    team = _fake_team(workspace, sent_transcript=sent)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["send", "dodo", "ack: see #1234"])

    assert result.exit_code != 0
    assert "new root send requires --artifact" in result.output


def test_send_accepts_short_root_summary_with_artifact(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "root-short.md")

    sent: list[str] = []
    team = _fake_team(workspace, sent_transcript=sent)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["send", "dodo", "ack: see #1234", "--artifact", artifact])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["msgId"] == FIXED_ID
    assert payload["state"] == "pending"
    assert result.stderr == ""
