"""Binary delivery contract: the transport verdict is the whole state.

A send either returns {ok, to, msgId} because the native transport accepted
the message (the target's own runtime owns it from there — nothing is
tracked, observed, or pollable afterwards), or it fails synchronously with
the transport error. The original three-message busy incident stays covered
as an ordering/no-duplicates regression.
"""
from __future__ import annotations

import pytest

from hive import bus, hived

pytestmark = pytest.mark.unit


def _wire(monkeypatch, workspace, agent):
    class _Team:
        name = "team-x"
        workspace = ""
        tmux_session = "dev"
        tmux_window = "dev:0"

    _Team.workspace = str(workspace)
    monkeypatch.setattr("hive.hived._resolve_live_agent", lambda _t, _a: (_Team(), agent))
    monkeypatch.setattr("hive.hived._check_send_gate", lambda _t: None)


def test_accepted_send_returns_identity_only(tmp_path, monkeypatch):
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    class _Agent:
        pane_id = "%9"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> str:
            return "udsWriteAccepted"

    _wire(monkeypatch, workspace, _Agent())
    payload = hived._send_payload(
        workspace=str(workspace), team_name="team-x", sender_agent="a",
        sender_pane="%1", target_agent="b", body="hi", artifact="", reply_to="",
    )

    assert payload["ok"] is True
    assert payload["msgId"]
    assert "delivery" not in payload
    # exactly one durable event: the send itself — no observations, no tracking
    assert [e["intent"] for e in bus.read_all_events(workspace)] == ["send"]


def test_refused_send_fails_synchronously(tmp_path, monkeypatch):
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    class _Agent:
        pane_id = "%9"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> str:
            raise RuntimeError("no channel")

    _wire(monkeypatch, workspace, _Agent())
    payload = hived._send_payload(
        workspace=str(workspace), team_name="team-x", sender_agent="a",
        sender_pane="%1", target_agent="b", body="hi", artifact="", reply_to="",
    )

    assert payload["ok"] is False
    assert "transport refused" in payload["error"]


def test_three_message_busy_incident_regression(tmp_path, monkeypatch):
    """Three sends to a busy target all succeed in order with zero duplicate
    transport submissions and zero sender-pane disturbance — the transports'
    own contracts (channel queueing / turn steering) own everything after
    acceptance."""
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    delivered: list[str] = []

    class _BusyAgent:
        pane_id = "%9"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> str:
            delivered.append(text)
            return "udsWriteAccepted"

    _wire(monkeypatch, workspace, _BusyAgent())
    results = []
    for body in ("first", "second", "third"):
        results.append(hived._send_payload(
            workspace=str(workspace), team_name="team-x", sender_agent="validator",
            sender_pane="%1", target_agent="worker", body=body, artifact="", reply_to="",
        ))

    assert all(r["ok"] for r in results)
    assert [d.split("\n")[1] for d in delivered] == ["first", "second", "third"]
    assert len(delivered) == 3  # no duplicate submissions, ever
    assert len({r["msgId"] for r in results}) == 3
    # the exception injector stays dead: nothing ever disturbs the sender pane
    assert not hasattr(hived, "_inject_exception")


def test_send_to_flow_mailbox_writes_bus_row_without_transport(tmp_path, monkeypatch):
    """The reserved `flow` address is a mailbox: the durable bus row IS the
    delivery. No member resolution, no gate, no transport — a member's
    `hive send flow` must succeed with no flow-runner pane anywhere."""
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    def boom(_t, _a):
        raise AssertionError("mailbox send must not resolve a live agent")

    monkeypatch.setattr("hive.hived._resolve_live_agent", boom)

    payload = hived._send_payload(
        workspace=str(workspace), team_name="team-x", sender_agent="impl",
        sender_pane="%1", target_agent="flow", body="done", artifact="/tmp/a.md",
        reply_to="m1",
    )

    assert payload["ok"] is True and payload["mailbox"] is True
    (event,) = bus.read_all_events(workspace)
    assert event["to"] == "flow" and event["from"] == "impl"
    assert event["inReplyTo"] == "m1"


def test_send_to_the_canonical_mailbox_address_also_lands(tmp_path, monkeypatch):
    """`flow.run` is the canonical mailbox address; the legacy bare `flow`
    stays a working alias (previous test). Same contract: bus row is the
    delivery, no live-agent resolution."""
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    monkeypatch.setattr(
        "hive.hived._resolve_live_agent",
        lambda _t, _a: (_ for _ in ()).throw(AssertionError("no resolution")),
    )

    payload = hived._send_payload(
        workspace=str(workspace), team_name="team-x", sender_agent="impl",
        sender_pane="%1", target_agent="flow.run", body="done", artifact="",
        reply_to="",
    )

    assert payload["ok"] is True and payload["mailbox"] is True
    (event,) = bus.read_all_events(workspace)
    assert event["to"] == "flow.run"
