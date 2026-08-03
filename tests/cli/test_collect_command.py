"""`hive collect` — the blocking inbox for members without channel push."""
import json

from hive import bus
from hive import context as hive_context
from hive.cli import cli


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
