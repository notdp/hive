"""Binary delivery contract: the transport verdict is the whole state.

A send either returns {ok, to, msgId} because the native transport accepted
the message (the target's own runtime owns it from there — nothing is
tracked, observed, or pollable afterwards), or it fails synchronously with
the transport error. The original three-message busy incident stays covered
as an ordering/no-duplicates regression.
"""
from __future__ import annotations

import pytest

from hive import bus, sidecar

pytestmark = pytest.mark.unit


def _wire(monkeypatch, workspace, agent):
    class _Team:
        name = "team-x"
        workspace = ""
        tmux_session = "dev"
        tmux_window = "dev:0"

    _Team.workspace = str(workspace)
    monkeypatch.setattr("hive.sidecar._resolve_live_agent", lambda _t, _a: (_Team(), agent))
    monkeypatch.setattr("hive.sidecar._check_send_gate", lambda _t: None)


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
    payload = sidecar._send_payload(
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
    payload = sidecar._send_payload(
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
        results.append(sidecar._send_payload(
            workspace=str(workspace), team_name="team-x", sender_agent="validator",
            sender_pane="%1", target_agent="worker", body=body, artifact="", reply_to="",
        ))

    assert all(r["ok"] for r in results)
    assert [d.split("\n")[1] for d in delivered] == ["first", "second", "third"]
    assert len(delivered) == 3  # no duplicate submissions, ever
    assert len({r["msgId"] for r in results}) == 3
    # the exception injector stays dead: nothing ever disturbs the sender pane
    assert not hasattr(sidecar, "_inject_exception")
