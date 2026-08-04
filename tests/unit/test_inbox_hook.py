"""The plugin Stop hook that delivers hive messages into a push-less session."""
import importlib.util
import json
from pathlib import Path

import pytest

pytestmark = pytest.mark.unit

_HOOK = (
    Path(__file__).resolve().parents[2]
    / "plugins" / "hive" / "scripts" / "inbox_hook.py"
)


def _load():
    spec = importlib.util.spec_from_file_location("inbox_hook", _HOOK)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _run(monkeypatch, capsys, *, event, collect_results, tmux_pane=""):
    hook = _load()
    calls: list[list[str]] = []

    def _fake_collect(args):
        calls.append(args)
        return collect_results.pop(0) if collect_results else {}

    monkeypatch.setattr(hook, "_collect", _fake_collect)
    monkeypatch.setenv("TMUX_PANE", tmux_pane)
    monkeypatch.setattr("sys.stdin", __import__("io").StringIO(json.dumps(event)))
    hook.main()
    return calls, capsys.readouterr().out


def test_hook_blocks_the_stop_with_the_inbound_envelope(monkeypatch, capsys):
    message = {
        "from": "validator", "to": "worker", "msgId": "18kd",
        "inReplyTo": "0Aea", "body": "VAL passed",
    }
    calls, out = _run(
        monkeypatch, capsys,
        event={"stop_hook_active": False},
        collect_results=[{"messages": [message], "count": 1}],
    )

    assert calls == [[]]  # drained, no wait needed — a message was already there
    payload = json.loads(out)
    assert payload["decision"] == "block"
    assert "<HIVE from=validator to=worker msgId=18kd reply-to=0Aea>" in payload["reason"]
    assert "VAL passed" in payload["reason"]


def test_hook_waits_only_while_a_reply_is_owed(monkeypatch, capsys):
    calls, out = _run(
        monkeypatch, capsys,
        event={"stop_hook_active": False},
        collect_results=[{"messages": []}, {"messages": [], "count": 0}],
    )

    assert calls == [[], ["--wait", "120", "--if-awaiting"]]
    assert out == ""  # nothing arrived: the turn ends normally


def test_hook_never_waits_again_while_already_blocking(monkeypatch, capsys):
    """stop_hook_active means a previous Stop already blocked — draining stays
    allowed (new messages still land) but the blocking wait must not repeat."""
    calls, out = _run(
        monkeypatch, capsys,
        event={"stop_hook_active": True},
        collect_results=[{"messages": []}],
    )

    assert calls == [[]]
    assert out == ""


def test_hook_is_a_no_op_inside_tmux(monkeypatch, capsys):
    calls, out = _run(
        monkeypatch, capsys,
        event={"stop_hook_active": False},
        collect_results=[{"messages": [{"body": "x"}]}],
        tmux_pane="%9",
    )

    assert calls == []  # native transports own delivery there
    assert out == ""


def test_post_tool_use_injects_mid_turn_without_blocking(monkeypatch, capsys):
    """The mid-turn path: a message that lands while the member works arrives
    with the next tool result, as additionalContext — never a block, and never
    a wait (a turn in progress must not stall on an empty inbox)."""
    message = {"from": "validator", "to": "worker", "msgId": "18kd", "body": "VAL failed: retry"}
    calls, out = _run(
        monkeypatch, capsys,
        event={"hook_event_name": "PostToolUse", "session_id": "sess-1"},
        collect_results=[{"messages": [message], "count": 1}],
    )

    assert calls == [["--session", "sess-1"]]  # scoped drain, no wait args
    payload = json.loads(out)
    assert payload["hookSpecificOutput"]["hookEventName"] == "PostToolUse"
    assert "VAL failed: retry" in payload["hookSpecificOutput"]["additionalContext"]
    assert "decision" not in payload  # must not block a turn in progress


def test_post_tool_use_is_silent_when_inbox_is_empty(monkeypatch, capsys):
    calls, out = _run(
        monkeypatch, capsys,
        event={"hook_event_name": "PostToolUse", "session_id": "sess-1"},
        collect_results=[{"messages": []}],
    )

    assert calls == [["--session", "sess-1"]]  # one drain, no blocking wait
    assert out == ""


def test_stop_scopes_its_drain_to_the_session(monkeypatch, capsys):
    calls, _out = _run(
        monkeypatch, capsys,
        event={"hook_event_name": "Stop", "stop_hook_active": False, "session_id": "sess-1"},
        collect_results=[{"messages": []}, {"messages": []}],
    )

    assert calls == [
        ["--session", "sess-1"],
        ["--session", "sess-1", "--wait", "120", "--if-awaiting"],
    ]
