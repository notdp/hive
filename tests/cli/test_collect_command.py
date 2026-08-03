"""`hive collect` — the blocking inbox for members without channel push."""
import json

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


def test_collect_if_awaiting_skips_the_wait_when_nothing_is_owed(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """The Stop hook ends an idle turn immediately: --if-awaiting only blocks
    while one of my own messages is still unanswered."""
    ws = _bind_desktop_worker(configure_hive_home, monkeypatch, tmp_path)
    slept: list[float] = []
    monkeypatch.setattr("hive.cli.time.sleep", lambda s: slept.append(s))

    # Nothing sent yet → nothing owed → returns at once despite --wait.
    payload = json.loads(runner.invoke(cli, ["collect", "--wait", "30", "--if-awaiting"]).output)
    assert payload["count"] == 0
    assert slept == []

    # An unanswered outbound → the wait is armed (loop runs until the deadline).
    bus.write_send_event(ws, from_agent="worker", to_agent="validator", body="please review")
    monkeypatch.setattr("hive.cli.time.monotonic", _fake_clock())
    payload = json.loads(runner.invoke(cli, ["collect", "--wait", "30", "--if-awaiting"]).output)
    assert payload["timedOut"] is True
    assert slept  # it actually blocked

    # Once the peer answers, nothing is owed again.
    reply = bus.write_send_event(
        ws, from_agent="validator", to_agent="worker", body="looks good", reply_to="0Aea"
    )
    assert reply.msg_id
