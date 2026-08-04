"""`hive collect` — the blocking inbox for members without channel push."""
import json
from pathlib import Path

from hive import bus
from hive import context as hive_context
from hive.cli import cli


def _fake_clock():
    """Monotonic clock that jumps past any deadline after two reads."""
    ticks = iter([0.0, 1.0] + [10_000.0] * 50)
    return lambda: next(ticks)


def _bind_desktop_worker(configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(tmux_inside=False)
    ws = tmp_path / "ws"
    bus.init_workspace(ws)
    hive_context.save_current_context(team="hive-ccd-w1", workspace=str(ws), agent="worker")
    from types import SimpleNamespace

    team = SimpleNamespace(name="hive-ccd-w1", workspace=str(ws))
    monkeypatch.setattr(
        "hive.cli._resolve_scoped_team", lambda _t, required=True: ("hive-ccd-w1", team)
    )
    return ws


def test_collect_drains_unread_and_advances_cursor(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    ws = _bind_desktop_worker(configure_hive_home, monkeypatch, tmp_path)
    bus.write_send_event(ws, from_agent="validator", to_agent="worker", body="verdict one")
    bus.write_send_event(ws, from_agent="worker", to_agent="validator", body="not mine")
    bus.write_send_event(ws, from_agent="validator", to_agent="worker", body="verdict two")

    result = runner.invoke(cli, ["collect"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["count"] == 2
    assert [m["body"] for m in payload["messages"]] == ["verdict one", "verdict two"]
    assert all(m["from"] == "validator" for m in payload["messages"])

    # Second drain: cursor advanced, nothing new, timedOut marks a bounded wait.
    result2 = runner.invoke(cli, ["collect", "--wait", "1"])
    payload2 = json.loads(result2.output)
    assert payload2["count"] == 0
    assert payload2["timedOut"] is True

    # A new message after the cursor is picked up.
    bus.write_send_event(ws, from_agent="validator", to_agent="worker", body="verdict three")
    payload3 = json.loads(runner.invoke(cli, ["collect"]).output)
    assert [m["body"] for m in payload3["messages"]] == ["verdict three"]


def test_collect_immediate_empty_has_no_timeout_flag(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    _bind_desktop_worker(configure_hive_home, monkeypatch, tmp_path)

    payload = json.loads(runner.invoke(cli, ["collect"]).output)

    assert payload == {"agent": "worker", "team": "hive-ccd-w1", "messages": [], "count": 0}


def test_collect_if_awaiting_arms_only_while_a_reply_is_owed(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """--if-awaiting blocks only while the member's latest move is an outbound
    still owed a reply; an idle session returns at once."""
    ws = _bind_desktop_worker(configure_hive_home, monkeypatch, tmp_path)
    slept: list[float] = []
    monkeypatch.setattr("hive.cli.time.sleep", lambda s: slept.append(s))

    # Nothing sent yet → nothing owed → returns at once despite --wait.
    payload = json.loads(runner.invoke(cli, ["collect", "--wait", "30", "--if-awaiting"]).output)
    assert payload["count"] == 0
    assert slept == []

    # Latest move is worker's own unanswered send → the wait is armed.
    bus.write_send_event(ws, from_agent="worker", to_agent="validator", body="please review")
    monkeypatch.setattr("hive.cli.time.monotonic", _fake_clock())
    payload = json.loads(runner.invoke(cli, ["collect", "--wait", "30", "--if-awaiting"]).output)
    assert payload["timedOut"] is True
    assert slept  # it actually blocked


def _sent_at(ws) -> float:
    """Wall-clock 'now' matching the timestamp bus just wrote (second precision)."""
    import datetime as dt

    return dt.datetime.now(dt.timezone.utc).timestamp()


def test_is_awaiting_reply_structure_and_recency(tmp_path):
    """Truth table for the gate: structure (latest move is my unanswered send)
    AND recency (that send is newer than the window). Both required."""
    ws = tmp_path / "ws"
    bus.init_workspace(ws)
    t0 = _sent_at(ws)

    # No history → not awaiting.
    assert bus.is_awaiting_reply(ws, sender="worker", within_seconds=120, now=t0) is False

    # Own send, unanswered, fresh → awaiting.
    a = bus.write_send_event(ws, from_agent="worker", to_agent="validator", body="review this")
    assert bus.is_awaiting_reply(ws, sender="worker", within_seconds=120, now=t0) is True

    # Same send, but now stale (queried far in the future) → NOT awaiting.
    # This is the bug fix: a trailing send never re-arms later unrelated turns.
    assert bus.is_awaiting_reply(ws, sender="worker", within_seconds=120, now=t0 + 600) is False

    # Peer replies → latest move is inbound → worker no longer awaits; validator does.
    bus.write_send_event(ws, from_agent="validator", to_agent="worker", body="ok", reply_to=a.msg_id)
    assert bus.is_awaiting_reply(ws, sender="worker", within_seconds=120, now=t0) is False
    assert bus.is_awaiting_reply(ws, sender="validator", within_seconds=120, now=t0) is True


def test_is_awaiting_reply_ignores_a_finished_exchanges_trailing_signoff(tmp_path):
    """A completed exchange leaves worker's own sign-off as the last unanswered
    send. Fresh, it may arm once (harmless); stale, it must never re-arm — the
    original bug was an all-history test that latched True forever."""
    ws = tmp_path / "ws"
    bus.init_workspace(ws)
    t0 = _sent_at(ws)
    a = bus.write_send_event(ws, from_agent="worker", to_agent="validator", body="review this")
    b = bus.write_send_event(
        ws, from_agent="validator", to_agent="worker", body="VAL passed", reply_to=a.msg_id
    )
    bus.write_send_event(
        ws, from_agent="worker", to_agent="validator", body="thanks, shipping", reply_to=b.msg_id
    )

    # Turns later, the stale sign-off does not drag every future turn into a wait.
    assert bus.is_awaiting_reply(ws, sender="worker", within_seconds=120, now=t0 + 600) is False


def test_collect_session_claim_keeps_siblings_out(runner, configure_hive_home, monkeypatch, tmp_path):
    """Every desktop session runs the same inbox hook, so the first to claim the
    member identity owns it; a sibling draining would deliver the member's mail
    into the wrong conversation."""
    ws = _bind_desktop_worker(configure_hive_home, monkeypatch, tmp_path)
    bus.write_send_event(ws, from_agent="validator", to_agent="worker", body="verdict")

    # A sibling session with no claim on file takes it and drains.
    payload = json.loads(runner.invoke(cli, ["collect", "--session", "sess-A"]).output)
    assert payload["count"] == 1
    assert hive_context.load_current_context()["session"] == "sess-A"

    # A different session is refused outright — no drain, no cursor movement.
    bus.write_send_event(ws, from_agent="validator", to_agent="worker", body="second")
    other = json.loads(runner.invoke(cli, ["collect", "--session", "sess-B"]).output)
    assert other == {"messages": [], "count": 0, "notMine": True}

    # The owner still gets it.
    mine = json.loads(runner.invoke(cli, ["collect", "--session", "sess-A"]).output)
    assert [m["body"] for m in mine["messages"]] == ["second"]


def test_collect_cursor_lock_prevents_duplicate_drain(tmp_path, monkeypatch):
    """Two collects racing on the same cursor must not both return the same
    message. The claim (read cursor → read rows → advance cursor) is locked, so
    the loser sees the advanced cursor and drains nothing."""
    import threading

    ws = tmp_path / "ws"
    bus.init_workspace(ws)
    (ws / "state").mkdir(parents=True, exist_ok=True)
    for i in range(5):
        bus.write_send_event(ws, from_agent="validator", to_agent="worker", body=f"m{i}")

    from hive import cli as cli_mod

    cursor = Path(ws) / "state" / "collect-cursor-worker"
    drained: list[list[int]] = []
    barrier = threading.Barrier(2)

    def claim():
        barrier.wait()  # maximize overlap
        with cli_mod._cursor_claim_lock(cursor):
            try:
                c = int(cursor.read_text().strip())
            except (OSError, ValueError):
                c = 0
            rows = bus.read_inbound_after(ws, recipient="worker", after_seq=c)
            if rows:
                cursor.write_text(str(rows[-1][0]))
            drained.append([s for s, _ in rows])

    ts = [threading.Thread(target=claim) for _ in range(2)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()

    all_seqs = [s for batch in drained for s in batch]
    assert sorted(all_seqs) == sorted(set(all_seqs))  # no seq delivered twice
    winner = [b for b in drained if b]
    assert len(winner) == 1 and len(winner[0]) == 5  # one drains all, other empty
