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
            "result": "success",
            "observedAt": "2026-04-15T00:00:00Z",
        },
    )

    payload = sidecar._thread_payload(
        str(workspace),
        {"a003": {}},
        "a003",
    )

    assert payload["ok"] is True
    assert payload["rootMsgId"] == "a001"
    assert payload["focusMsgId"] == "a003"
    assert [item["msgId"] for item in payload["messages"]] == ["a001", "a002", "a003"]
    assert [item["depth"] for item in payload["messages"]] == [0, 1, 2]
    assert payload["messages"][1]["delivery"]["delivery"] == "success"
    assert payload["messages"][2]["delivery"]["delivery"] == "pending"
    assert payload["messages"][2]["focus"] is True
