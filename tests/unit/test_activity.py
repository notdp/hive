from __future__ import annotations

from datetime import datetime
import json

from hive.activity import (
    probe_transcript_artifact_opened,
    probe_transcript_interrupt_safety,
)
from hive.adapters.base import Message, MessagePart


def _ts(value: str) -> datetime:
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def test_probe_transcript_interrupt_safety_claude_turn_closed_is_safe(tmp_path):
    path = tmp_path / "claude.jsonl"
    path.write_text(
        "\n".join(
            [
                json.dumps(
                    {
                        "type": "assistant",
                        "timestamp": "2026-04-16T05:00:00Z",
                        "message": {
                            "role": "assistant",
                            "content": [{"type": "text", "text": "done"}],
                            "stop_reason": "end_turn",
                        },
                    }
                ),
                json.dumps(
                    {
                        "type": "system",
                        "subtype": "stop_hook_summary",
                        "preventedContinuation": False,
                        "timestamp": "2026-04-16T05:00:01Z",
                    }
                ),
                json.dumps(
                    {
                        "type": "system",
                        "subtype": "turn_duration",
                        "timestamp": "2026-04-16T05:00:02Z",
                    }
                ),
            ]
        )
        + "\n"
    )

    payload = probe_transcript_interrupt_safety("claude", path)

    assert payload["interruptSafety"] == "safe"
    assert payload["safetyReason"] == "turn_closed"
    assert payload["safetyObservedAt"] == "2026-04-16T05:00:02Z"


def test_probe_transcript_interrupt_safety_claude_backlog_is_unsafe(tmp_path):
    path = tmp_path / "claude-backlog.jsonl"
    path.write_text(
        "\n".join(
            [
                json.dumps(
                    {
                        "type": "queue-operation",
                        "operation": "enqueue",
                        "timestamp": "2026-04-16T05:00:00Z",
                    }
                ),
                json.dumps(
                    {
                        "type": "assistant",
                        "timestamp": "2026-04-16T05:00:01Z",
                        "message": {
                            "role": "assistant",
                            "content": [{"type": "tool_use", "name": "Bash"}],
                            "stop_reason": "tool_use",
                        },
                    }
                ),
            ]
        )
        + "\n"
    )

    payload = probe_transcript_interrupt_safety("claude", path)

    assert payload["interruptSafety"] == "unsafe"
    assert payload["safetyReason"] == "input_backlog"


def test_probe_transcript_interrupt_safety_claude_tool_result_is_unknown(tmp_path):
    path = tmp_path / "claude-tool-result.jsonl"
    path.write_text(
        json.dumps(
            {
                "type": "user",
                "timestamp": "2026-04-16T05:00:00Z",
                "message": {
                    "role": "user",
                    "content": [{"type": "tool_result", "is_error": True, "content": "boom"}],
                },
            }
        )
        + "\n"
    )

    payload = probe_transcript_interrupt_safety("claude", path)

    assert payload["interruptSafety"] == "unknown"
    assert payload["safetyReason"] == "tool_result_pending_reply"


def test_probe_transcript_interrupt_safety_codex_task_close_is_safe(tmp_path):
    path = tmp_path / "codex.jsonl"
    path.write_text(
        "\n".join(
            [
                json.dumps(
                    {
                        "type": "event_msg",
                        "timestamp": "2026-04-16T05:00:00Z",
                        "payload": {"type": "task_started"},
                    }
                ),
                json.dumps(
                    {
                        "type": "event_msg",
                        "timestamp": "2026-04-16T05:00:01Z",
                        "payload": {"type": "task_complete"},
                    }
                ),
            ]
        )
        + "\n"
    )

    payload = probe_transcript_interrupt_safety("codex", path)

    assert payload["interruptSafety"] == "safe"
    assert payload["safetyReason"] == "task_closed"


def test_probe_transcript_interrupt_safety_droid_assistant_text_stays_unknown(tmp_path):
    path = tmp_path / "droid.jsonl"
    path.write_text(
        json.dumps(
            {
                "type": "message",
                "timestamp": "2026-04-16T05:00:00Z",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "已完成。"}],
                },
            }
        )
        + "\n"
    )

    payload = probe_transcript_interrupt_safety("droid", path)

    assert payload["interruptSafety"] == "unknown"
    assert payload["safetyReason"] == "assistant_text_idle"


def test_probe_transcript_artifact_opened_matches_claude_read(tmp_path):
    path = tmp_path / "claude-open.jsonl"
    path.write_text(
        json.dumps(
            {
                "type": "assistant",
                "timestamp": "2026-04-18T00:00:00Z",
                "message": {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "name": "Read",
                            "input": {"file_path": "/tmp/report.md"},
                        }
                    ],
                },
            }
        )
        + "\n"
    )

    payload = probe_transcript_artifact_opened("claude", path, "/tmp/report.md", since="2026-04-18T00:00:00Z")

    assert payload["opened"] is True
    assert payload["observedAt"] == "2026-04-18T00:00:00Z"


def test_probe_transcript_artifact_opened_matches_codex_read(tmp_path):
    path = tmp_path / "codex-open.jsonl"
    path.write_text(
        json.dumps(
            {
                "type": "event_msg",
                "timestamp": "2026-04-18T00:00:00Z",
                "payload": {
                    "type": "exec_command_end",
                    "parsed_cmd": [
                        {
                            "type": "read",
                            "path": "/tmp/report.md",
                        }
                    ],
                },
            }
        )
        + "\n"
    )

    payload = probe_transcript_artifact_opened("codex", path, "/tmp/report.md", since="2026-04-18T00:00:00Z")

    assert payload["opened"] is True
    assert payload["observedAt"] == "2026-04-18T00:00:00Z"


def test_probe_transcript_artifact_opened_matches_droid_read(tmp_path):
    path = tmp_path / "droid-open.jsonl"
    path.write_text(
        json.dumps(
            {
                "type": "message",
                "timestamp": "2026-04-18T00:00:00Z",
                "message": {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "name": "Read",
                            "input": {"file_path": "/tmp/report.md"},
                        }
                    ],
                },
            }
        )
        + "\n"
    )

    payload = probe_transcript_artifact_opened("droid", path, "/tmp/report.md", since="2026-04-18T00:00:00Z")

    assert payload["opened"] is True
    assert payload["observedAt"] == "2026-04-18T00:00:00Z"
