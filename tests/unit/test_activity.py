from __future__ import annotations

from datetime import datetime

from hive.activity import classify_activity, probe_transcript_activity
from hive.adapters.base import Message, MessagePart


def _ts(value: str) -> datetime:
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def test_classify_activity_marks_user_tail_active():
    payload = classify_activity(
        [
            Message(
                message_id="m1",
                parent_id=None,
                role="user",
                parts=(MessagePart(kind="text", text="hi"),),
                timestamp=_ts("2026-04-16T05:00:00Z"),
            )
        ]
    )

    assert payload["activityState"] == "active"
    assert payload["activityReason"] == "last_role_user"
    assert payload["activityRole"] == "user"


def test_classify_activity_marks_assistant_tool_use_active():
    payload = classify_activity(
        [
            Message(
                message_id="m1",
                parent_id=None,
                role="assistant",
                parts=(MessagePart(kind="tool_use", tool_name="exec_command"),),
                timestamp=_ts("2026-04-16T05:00:00Z"),
            )
        ]
    )

    assert payload["activityState"] == "active"
    assert payload["activityReason"] == "assistant_tool_use_open"
    assert payload["activityPartKinds"] == ["tool_use"]


def test_classify_activity_marks_tool_result_tail_active():
    payload = classify_activity(
        [
            Message(
                message_id="call-1",
                parent_id=None,
                role="tool",
                parts=(MessagePart(kind="tool_result", tool_output="ok"),),
                timestamp=_ts("2026-04-16T05:00:00Z"),
            )
        ]
    )

    assert payload["activityState"] == "active"
    assert payload["activityReason"] == "last_role_tool"


def test_classify_activity_marks_terminal_assistant_idle_even_with_thinking():
    payload = classify_activity(
        [
            Message(
                message_id="m1",
                parent_id=None,
                role="assistant",
                parts=(
                    MessagePart(kind="thinking", text="plan"),
                    MessagePart(kind="text", text="done"),
                ),
                timestamp=_ts("2026-04-16T05:00:00Z"),
            )
        ]
    )

    assert payload["activityState"] == "idle"
    assert payload["activityReason"] == "assistant_terminal_message"
    assert payload["activityPartKinds"] == ["thinking", "text"]


def test_probe_transcript_activity_returns_unknown_for_missing_file(tmp_path):
    payload = probe_transcript_activity("claude", tmp_path / "missing.jsonl")

    assert payload["activityState"] == "unknown"
    assert payload["activityReason"] == "transcript_missing"


def test_probe_transcript_activity_returns_unknown_for_unknown_cli(tmp_path):
    path = tmp_path / "session.jsonl"
    path.write_text("")

    payload = probe_transcript_activity("unknown-cli", path)

    assert payload["activityState"] == "unknown"
    assert payload["activityReason"] == "no_adapter_unknown-cli"
