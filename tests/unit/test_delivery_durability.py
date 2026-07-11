"""Durable delivery-state contract (VAL-4/6/8).

The transport outcome is written durably before the send returns, survives a
sidecar restart as queued (never tracking_lost/failed), upgrades to success
only via the target's transcript, and historical records keep their meaning.
"""
from __future__ import annotations

import json

import pytest

from hive import bus, sidecar

pytestmark = pytest.mark.unit


def _write_send(workspace, msg_id: str, body: str = "msg") -> None:
    bus.write_event(
        workspace,
        from_agent="claude",
        to_agent="gpt",
        intent="send",
        body=body,
        message_id=msg_id,
    )


def _write_queued_observation(workspace, msg_id: str, transcript: str) -> None:
    """Exact shape `_send_payload` writes durably before returning."""
    bus.write_event(
        workspace,
        from_agent="_system",
        to_agent="",
        intent="observation",
        message_id=msg_id,
        metadata={
            "msgId": msg_id,
            "result": "queued",
            "observedAt": "2026-07-11T00:00:00Z",
            "injectStatus": "submitted",
            "turnObserved": "pending",
            "transportAccepted": "mcpWriteAccepted",
            "targetPane": "%9",
            "targetTranscript": transcript,
            "baseline": "0",
        },
    )


def _transcript_line(msg_id: str) -> str:
    return json.dumps({
        "type": "user",
        "message": {"role": "user", "content": f"<HIVE msgId={msg_id} from=a to=b />"},
    }) + "\n"


def test_queued_record_survives_sidecar_restart(tmp_path):
    """VAL-4: accepted send + emptied in-memory tracker (restart) still reads
    queued — never tracking_lost/failed."""
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    transcript = tmp_path / "session.jsonl"
    transcript.write_text("")
    _write_send(workspace, "dur1")
    _write_queued_observation(workspace, "dur1", str(transcript))

    payload = sidecar._delivery_payload(str(workspace), {}, "dur1")

    assert payload["ok"] is True
    assert payload["delivery"] == "queued"
    assert payload.get("reason") != "tracking_lost"


def test_queued_record_upgrades_via_transcript_after_restart(tmp_path):
    """VAL-4: after a restart, a transcript hit upgrades the same record to
    success with confirmationSource=transcript, written durably."""
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    transcript = tmp_path / "session.jsonl"
    transcript.write_text("")
    _write_send(workspace, "dur2")
    _write_queued_observation(workspace, "dur2", str(transcript))

    transcript.write_text(_transcript_line("dur2"))
    payload = sidecar._delivery_payload(str(workspace), {}, "dur2")

    assert payload["delivery"] == "success"
    assert payload["confirmationSource"] == "transcript"
    # the upgrade itself is durable: a later read needs no transcript re-check
    latest = bus.find_latest_observation(workspace, "dur2")
    assert latest["metadata"]["result"] == "success"
    assert latest["metadata"]["confirmationSource"] == "transcript"


def test_transport_failure_stays_failed_across_restart(tmp_path):
    """VAL-4: an explicit transport failure is durable and restart-proof."""
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _write_send(workspace, "dur3")
    bus.write_event(
        workspace,
        from_agent="_system",
        to_agent="",
        intent="observation",
        message_id="dur3",
        metadata={"msgId": "dur3", "result": "failed", "injectStatus": "failed",
                  "turnObserved": "unavailable", "observedAt": "2026-07-11T00:00:00Z"},
    )

    payload = sidecar._delivery_payload(str(workspace), {}, "dur3")

    assert payload["delivery"] == "failed"


def test_legacy_stream_success_remains_readable_and_unrewritten(tmp_path):
    """VAL-8: pre-change successes with confirmationSource=stream stay
    readable as success with that source; reads write nothing new."""
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _write_send(workspace, "old1")
    bus.write_event(
        workspace,
        from_agent="_system",
        to_agent="",
        intent="observation",
        message_id="old1",
        metadata={"msgId": "old1", "result": "success", "injectStatus": "submitted",
                  "turnObserved": "confirmed", "confirmationSource": "stream",
                  "observedAt": "2026-01-01T00:00:00Z"},
    )
    before = len(bus.read_all_events(workspace))

    payload = sidecar._delivery_payload(str(workspace), {}, "old1")

    assert payload["delivery"] == "success"
    assert payload["confirmationSource"] == "stream"
    assert len(bus.read_all_events(workspace)) == before  # read-only


def test_historical_terminal_failures_are_not_promoted(tmp_path):
    """VAL-8: legacy failed/unconfirmed/tracking_lost terminal observations
    stay failed even when a transcript would now match."""
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    transcript = tmp_path / "session.jsonl"
    transcript.write_text(_transcript_line("old2"))
    _write_send(workspace, "old2")
    bus.write_event(
        workspace,
        from_agent="_system",
        to_agent="",
        intent="observation",
        message_id="old2",
        metadata={"msgId": "old2", "result": "unconfirmed", "injectStatus": "submitted",
                  "turnObserved": "unconfirmed", "targetTranscript": str(transcript),
                  "baseline": "0", "observedAt": "2026-01-01T00:00:00Z"},
    )

    payload = sidecar._delivery_payload(str(workspace), {}, "old2")

    assert payload["delivery"] == "failed"


def test_three_message_busy_incident_regression(tmp_path, monkeypatch):
    """VAL-6: three sends to a busy (transcript-silent) target all queue in
    order, none fails, no sender-pane exception exists, and each later
    upgrades independently once its msgId reaches the transcript."""
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    transcript = tmp_path / "session.jsonl"
    transcript.write_text("")

    delivered: list[str] = []

    class _BusyAgent:
        pane_id = "%9"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> str:
            delivered.append(text)
            return "mcpWriteAccepted"

    class _Team:
        name = "team-x"
        workspace = ""
        tmux_session = "dev"
        tmux_window = "dev:0"

    _Team.workspace = str(workspace)

    agent = _BusyAgent()
    monkeypatch.setattr(
        "hive.sidecar._resolve_live_agent", lambda _t, _a: (_Team(), agent)
    )
    monkeypatch.setattr(
        "hive.sidecar._resolve_ack_baseline", lambda _t: (transcript, 0)
    )
    monkeypatch.setattr("hive.sidecar._check_send_gate", lambda _p: None)
    monkeypatch.setattr("hive.sidecar.detect_profile_for_pane", lambda _p: None)
    monkeypatch.setattr("hive.sidecar.SEND_GRACE_TIMEOUT", 0.0)

    pending: dict[str, dict] = {}
    results = []
    for i, body in enumerate(("first", "second", "third")):
        results.append(sidecar._send_payload(
            workspace=str(workspace),
            team_name="team-x",
            pending=pending,
            sender_agent="validator",
            sender_pane="%1",
            target_agent="worker",
            body=body,
            artifact="",
            reply_to="",
            wait=False,
        ))

    # ordered transport submissions, one per message, no duplicates
    assert [r["delivery"] for r in results] == ["queued", "queued", "queued"]
    bodies = [d.split("\n")[1] for d in delivered]
    assert bodies == ["first", "second", "third"]
    assert len(pending) == 3

    # sender pane is never disturbed: the exception injector is gone for good
    assert not hasattr(sidecar, "_inject_exception")

    # each message upgrades independently as its msgId lands in the transcript
    msg_ids = [r["msgId"] for r in results]
    lines = ""
    for mid in msg_ids:
        lines += _transcript_line(mid)
        transcript.write_text(lines)
        payload = sidecar._delivery_payload(str(workspace), {}, mid)
        assert payload["delivery"] == "success"
        assert payload["confirmationSource"] == "transcript"
    # no duplicate transport submission happened during tracking or query
    assert len(delivered) == 3
