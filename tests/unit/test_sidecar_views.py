import hive.sidecar as sidecar
from hive import bus


class _FakeAgent:
    def __init__(self, name: str, pane_id: str, cli: str):
        self.name = name
        self.pane_id = pane_id
        self.cli = cli

    def is_alive(self) -> bool:
        return True


class _FakeTeam:
    def __init__(self):
        self.name = "team-x"
        self.agents = {
            "momo": _FakeAgent("momo", "%1", "codex"),
            "orch": _FakeAgent("orch", "%2", "claude"),
            "peer": _FakeAgent("peer", "%3", "codex"),
            "offline": _FakeAgent("offline", "%4", "claude"),
        }
        self.terminals = {}

    def lead_agent(self):
        return None


def test_suggest_prefers_ready_different_model_and_cli(monkeypatch):
    monkeypatch.setattr("hive.team.Team.load", lambda _team_name: _FakeTeam())
    monkeypatch.setattr(
        sidecar,
        "_team_runtime_payload",
        lambda _team_name: {
            "ok": True,
            "members": {
                "momo": {
                    "alive": True,
                    "model": "gpt-5.4",
                    "inputState": "ready",
                    "_cli": "codex",
                },
                "orch": {
                    "alive": True,
                    "model": "claude-opus-4-6",
                    "inputState": "ready",
                    "_cli": "claude",
                    "sessionId": "sess-orch",
                },
                "peer": {
                    "alive": True,
                    "model": "gpt-5.4",
                    "inputState": "ready",
                    "_cli": "codex",
                    "sessionId": "sess-peer",
                },
                "offline": {
                    "alive": False,
                    "model": "claude-opus-4-6",
                    "inputState": "offline",
                    "_cli": "claude",
                },
            },
        },
    )

    payload = sidecar._suggest_payload("team-x", "momo")

    assert payload["ok"] is True
    assert payload["source"]["name"] == "momo"
    assert [candidate["name"] for candidate in payload["candidates"]] == ["orch", "peer"]
    assert payload["candidates"][0]["score"] > payload["candidates"][1]["score"]
    assert "different_model" in payload["candidates"][0]["reasons"]
    assert "different_cli" in payload["candidates"][0]["reasons"]
    assert payload["candidates"][1]["reasons"] == ["ready", "same_model_fallback", "same_cli_fallback"]


def test_thread_payload_projects_send_chain_and_delivery_states(tmp_path):
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    bus.write_event(
        workspace,
        from_agent="momo",
        to_agent="orch",
        intent="send",
        body="root",
        message_id="a001",
    )
    bus.write_event(
        workspace,
        from_agent="orch",
        to_agent="momo",
        intent="send",
        body="reply",
        message_id="a002",
        reply_to="a001",
    )
    bus.write_event(
        workspace,
        from_agent="momo",
        to_agent="orch",
        intent="send",
        body="follow-up",
        message_id="a003",
        reply_to="a002",
    )
    bus.write_event(
        workspace,
        from_agent="_system",
        to_agent="",
        intent="observation",
        message_id="a002",
        metadata={
            "msgId": "a002",
            "result": "confirmed",
            "observedAt": "2026-04-15T00:00:00Z",
        },
    )

    payload = sidecar._thread_payload(
        str(workspace),
        {
            "a003": {
                "runtimeQueueState": "queued",
                "queueSource": "capture",
            }
        },
        "a003",
    )

    assert payload["ok"] is True
    assert payload["rootMsgId"] == "a001"
    assert payload["focusMsgId"] == "a003"
    assert [item["msgId"] for item in payload["messages"]] == ["a001", "a002", "a003"]
    assert [item["depth"] for item in payload["messages"]] == [0, 1, 2]
    assert payload["messages"][1]["delivery"]["state"] == "confirmed"
    assert payload["messages"][2]["delivery"]["state"] == "queued"
    assert payload["messages"][2]["delivery"]["queueSource"] == "capture"
    assert payload["messages"][2]["focus"] is True
