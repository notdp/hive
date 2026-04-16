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
        self._peer_map = {"momo": "peer", "peer": "momo"}

    def lead_agent(self):
        return None

    def resolve_peer(self, name: str):
        return self._peer_map.get(name)


def test_suggest_prefers_idle_before_default_peer_bias(monkeypatch):
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
                    "activityState": "idle",
                    "activityReason": "assistant_terminal_message",
                    "activityObservedAt": "2026-04-16T05:00:00Z",
                },
                "orch": {
                    "alive": True,
                    "model": "claude-opus-4-6",
                    "inputState": "ready",
                    "_cli": "claude",
                    "sessionId": "sess-orch",
                    "activityState": "idle",
                    "activityReason": "assistant_terminal_message",
                    "activityObservedAt": "2026-04-16T05:01:00Z",
                },
                "peer": {
                    "alive": True,
                    "model": "gpt-5.4",
                    "inputState": "ready",
                    "_cli": "codex",
                    "sessionId": "sess-peer",
                    "activityState": "active",
                    "activityReason": "last_role_user",
                    "activityObservedAt": "2026-04-16T05:02:00Z",
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
    assert payload["source"]["peer"] == "peer"
    assert payload["source"]["activityState"] == "idle"
    assert payload["source"]["activityReason"] == "assistant_terminal_message"
    assert payload["source"]["activityObservedAt"] == "2026-04-16T05:00:00Z"
    assert [candidate["name"] for candidate in payload["candidates"]] == ["orch", "peer"]
    assert payload["candidates"][0]["score"] > payload["candidates"][1]["score"]
    assert payload["candidates"][0].get("isPeer") is not True
    assert "different_model" in payload["candidates"][0]["reasons"]
    assert "different_cli" in payload["candidates"][0]["reasons"]
    assert "activity_idle" in payload["candidates"][0]["reasons"]
    assert payload["candidates"][0]["activityState"] == "idle"
    assert payload["candidates"][0]["activityReason"] == "assistant_terminal_message"
    assert payload["candidates"][0]["activityObservedAt"] == "2026-04-16T05:01:00Z"
    assert payload["candidates"][1]["isPeer"] is True
    assert "default_peer" in payload["candidates"][1]["reasons"]
    assert "activity_active" in payload["candidates"][1]["reasons"]
    assert payload["candidates"][1]["activityState"] == "active"
    assert payload["candidates"][1]["activityReason"] == "last_role_user"
    assert payload["candidates"][1]["activityObservedAt"] == "2026-04-16T05:02:00Z"


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
