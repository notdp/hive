"""Hold-until-idle delivery: a claude member mid-turn is not interrupted.

A send aimed at a busy claude member parks in the sidecar's durable queue and
is handed over the moment the member's registry reports idle, FIFO per target.
The sender is answered ``ok`` at park time, so the queue survives a restart.
"""
from __future__ import annotations

import json

import pytest

from hive import bus, parked, sidecar

pytestmark = pytest.mark.unit


class _Member:
    def __init__(self, name: str, *, pane: str, cli: str = "claude") -> None:
        self.name = name
        self.pane_id = pane
        self.cli = cli
        self.sent: list[str] = []
        self.refuse = False

    def is_alive(self) -> bool:
        return True

    def send(self, text: str) -> str:
        if self.refuse:
            raise RuntimeError("inbox is not listening")
        self.sent.append(text)
        return "udsWriteAccepted"


def _wire(monkeypatch, members: dict[str, _Member], busy: dict[str, bool | None]):
    class _Team:
        name = "team-x"

    def _resolve(_team_name: str, agent_name: str):
        if agent_name not in members:
            raise RuntimeError(f"agent '{agent_name}' is not alive")
        return _Team(), members[agent_name]

    monkeypatch.setattr(sidecar, "_resolve_live_agent", _resolve)
    monkeypatch.setattr(sidecar, "_check_send_gate", lambda _t: None)
    monkeypatch.setattr(sidecar, "_claude_registry_busy", lambda pane: busy.get(pane))


def _send(workspace, target: str, body: str) -> dict:
    return sidecar._send_payload(
        workspace=str(workspace), team_name="team-x", sender_agent="orch",
        sender_pane="%1", target_agent=target, body=body, artifact="", reply_to="",
    )


def _workspace(tmp_path):
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    sidecar._PARK_QUEUES.clear()
    return workspace


def _bodies(member: _Member) -> list[str]:
    return [text.split("\n")[1] for text in member.sent]


def test_busy_claude_member_parks_instead_of_interrupting(tmp_path, monkeypatch):
    workspace = _workspace(tmp_path)
    worker = _Member("worker", pane="%9")
    _wire(monkeypatch, {"worker": worker}, {"%9": True})

    payload = _send(workspace, "worker", "review this")

    assert payload["ok"] is True and payload["held"] is True
    assert payload["msgId"]
    assert worker.sent == []  # the transport was never touched
    # the durable record is written at park time, exactly once
    assert [e["intent"] for e in bus.read_all_events(workspace)] == ["send"]
    (held,) = sidecar._park_queue(str(workspace)).pending()
    assert held.msg_id == payload["msgId"] and held.target == "worker"


def test_idle_member_and_other_clis_deliver_immediately(tmp_path, monkeypatch):
    workspace = _workspace(tmp_path)
    idle = _Member("idle", pane="%1")
    unknown = _Member("unknown", pane="%2")
    coder = _Member("coder", pane="%3", cli="codex")
    _wire(
        monkeypatch,
        {"idle": idle, "unknown": unknown, "coder": coder},
        {"%1": False, "%2": None, "%3": True},
    )

    for name in ("idle", "unknown", "coder"):
        assert "held" not in _send(workspace, name, f"to {name}")

    assert _bodies(idle) == ["to idle"]
    assert _bodies(unknown) == ["to unknown"]  # unknown busy state is not a hold
    assert _bodies(coder) == ["to coder"]  # only claude renders arrival timing
    assert sidecar._park_queue(str(workspace)).pending() == []


def test_held_messages_flush_in_fifo_order_once_idle(tmp_path, monkeypatch):
    workspace = _workspace(tmp_path)
    worker = _Member("worker", pane="%9")
    busy = {"%9": True}
    _wire(monkeypatch, {"worker": worker}, busy)

    for body in ("first", "second", "third"):
        assert _send(workspace, "worker", body)["held"] is True

    sidecar._flush_parked(str(workspace))
    assert worker.sent == []  # still mid-turn

    busy["%9"] = False
    for _ in range(3):
        sidecar._flush_parked(str(workspace))

    assert _bodies(worker) == ["first", "second", "third"]
    assert sidecar._park_queue(str(workspace)).pending() == []


def test_a_fresh_send_never_overtakes_a_held_message(tmp_path, monkeypatch):
    workspace = _workspace(tmp_path)
    worker = _Member("worker", pane="%9")
    busy = {"%9": True}
    _wire(monkeypatch, {"worker": worker}, busy)

    assert _send(workspace, "worker", "first")["held"] is True
    busy["%9"] = False
    assert _send(workspace, "worker", "second")["held"] is True  # queue not empty

    sidecar._flush_parked(str(workspace))
    sidecar._flush_parked(str(workspace))
    assert _bodies(worker) == ["first", "second"]


def test_a_hold_older_than_the_cap_is_forced_through(tmp_path, monkeypatch):
    workspace = _workspace(tmp_path)
    worker = _Member("worker", pane="%9")
    _wire(monkeypatch, {"worker": worker}, {"%9": True})

    _send(workspace, "worker", "still waiting")
    queue = sidecar._park_queue(str(workspace))
    sidecar._flush_parked(str(workspace))
    assert worker.sent == []

    for row in queue.pending():
        row.parked_at -= parked.MAX_HOLD_SECONDS + 1
    sidecar._flush_parked(str(workspace))

    assert _bodies(worker) == ["still waiting"]  # busy the whole time
    assert queue.pending() == []


def test_holds_survive_a_sidecar_restart(tmp_path, monkeypatch):
    workspace = _workspace(tmp_path)
    worker = _Member("worker", pane="%9")
    busy = {"%9": True}
    _wire(monkeypatch, {"worker": worker}, busy)

    _send(workspace, "worker", "first")
    _send(workspace, "worker", "second")

    rows = [
        json.loads(line)
        for line in (workspace / "run" / parked.PARKED_FILE_NAME).read_text().splitlines()
    ]
    assert [r["target"] for r in rows] == ["worker", "worker"]

    sidecar._PARK_QUEUES.clear()  # restart: nothing but the file survives
    busy["%9"] = False
    sidecar._flush_parked(str(workspace))
    sidecar._flush_parked(str(workspace))

    assert _bodies(worker) == ["first", "second"]
    assert (workspace / "run" / parked.PARKED_FILE_NAME).read_text() == ""


def test_corrupt_rows_are_skipped_not_fatal(tmp_path, monkeypatch):
    workspace = _workspace(tmp_path)
    worker = _Member("worker", pane="%9")
    _wire(monkeypatch, {"worker": worker}, {"%9": False})

    good = parked.ParkedMessage(
        team="team-x", target="worker", msg_id="m2", envelope="hdr\nkept", parked_at=1.0,
    )
    path = workspace / "run" / parked.PARKED_FILE_NAME
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "{not json at all\n"
        + json.dumps({"target": "worker", "msgId": "m1"}) + "\n"
        + json.dumps(good.to_row()) + "\n"
    )

    queue = sidecar._park_queue(str(workspace))
    assert queue.skipped == 2
    sidecar._flush_parked(str(workspace))
    assert _bodies(worker) == ["kept"]


def test_a_refused_hand_over_drops_the_hold(tmp_path, monkeypatch):
    workspace = _workspace(tmp_path)
    worker = _Member("worker", pane="%9")
    busy = {"%9": True}
    _wire(monkeypatch, {"worker": worker}, busy)

    _send(workspace, "worker", "first")
    _send(workspace, "worker", "second")
    worker.refuse = True
    busy["%9"] = False

    sidecar._flush_parked(str(workspace))  # first: transport refuses
    worker.refuse = False
    sidecar._flush_parked(str(workspace))  # second: not blocked behind it

    assert _bodies(worker) == ["second"]
    assert sidecar._park_queue(str(workspace)).pending() == []
    # the bus keeps the record of both, nothing is retried forever
    assert len(bus.read_all_events(workspace)) == 2


def test_a_hold_whose_member_is_gone_is_dropped(tmp_path, monkeypatch):
    workspace = _workspace(tmp_path)
    worker = _Member("worker", pane="%9")
    members = {"worker": worker}
    _wire(monkeypatch, members, {"%9": True})

    _send(workspace, "worker", "first")
    members.pop("worker")
    sidecar._flush_parked(str(workspace))

    assert sidecar._park_queue(str(workspace)).pending() == []
