from __future__ import annotations

import json

from hive.activity import probe_transcript_turn_phase


def test_probe_transcript_turn_phase_claude_turn_closed_is_safe(tmp_path):
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

    payload = probe_transcript_turn_phase("claude", path)

    assert payload["turnPhase"] == "turn_closed"
    assert payload["phaseObservedAt"] == "2026-04-16T05:00:02Z"


def test_probe_transcript_turn_phase_claude_latest_backlog_is_unsafe(tmp_path):
    path = tmp_path / "claude-backlog.jsonl"
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
                        "type": "queue-operation",
                        "operation": "enqueue",
                        "timestamp": "2026-04-16T05:00:01Z",
                    }
                ),
            ]
        )
        + "\n"
    )

    payload = probe_transcript_turn_phase("claude", path)

    assert payload["turnPhase"] == "input_backlog"
    assert payload["phaseObservedAt"] == "2026-04-16T05:00:01Z"


def test_probe_transcript_turn_phase_claude_later_turn_close_overrides_backlog(tmp_path):
    path = tmp_path / "claude-stale-backlog.jsonl"
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
                            "content": [{"type": "text", "text": "done"}],
                            "stop_reason": "end_turn",
                        },
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

    payload = probe_transcript_turn_phase("claude", path)

    assert payload["turnPhase"] == "turn_closed"
    assert payload["phaseObservedAt"] == "2026-04-16T05:00:02Z"


def test_probe_transcript_turn_phase_claude_tool_result_is_unknown(tmp_path):
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

    payload = probe_transcript_turn_phase("claude", path)

    assert payload["turnPhase"] == "tool_result_pending_reply"


def test_probe_transcript_turn_phase_codex_has_no_probe(tmp_path):
    """Codex state is daemon-native only: even a transcript full of JCL events
    must not produce a phase (embedded codex is unsupported)."""
    path = tmp_path / "codex.jsonl"
    path.write_text(
        json.dumps(
            {
                "type": "event_msg",
                "timestamp": "2026-04-16T05:00:00Z",
                "payload": {"type": "task_started"},
            }
        )
        + "\n"
    )

    payload = probe_transcript_turn_phase("codex", path)

    assert payload["turnPhase"] == "unknown_evidence"


def test_probe_transcript_turn_phase_claude_user_prompt_with_string_content_is_active(tmp_path):
    """Anthropic legacy format: when a user types a prompt straight into
    the CLI, ``message.content`` is a plain string (not a list of blocks).
    Probe must still report ``user_prompt_pending`` so idle-notify doesn't
    mistake a queued prompt awaiting reply for an idle turn."""
    path = tmp_path / "claude-string-prompt.jsonl"
    path.write_text(
        "\n".join(
            [
                json.dumps(
                    {
                        "type": "system",
                        "subtype": "turn_duration",
                        "timestamp": "2026-04-28T11:00:00Z",
                    }
                ),
                json.dumps(
                    {
                        "type": "user",
                        "timestamp": "2026-04-28T11:11:39Z",
                        "message": {
                            "role": "user",
                            "content": "调查一下为何只有 status bar 通知",
                        },
                    }
                ),
            ]
        )
        + "\n"
    )

    payload = probe_transcript_turn_phase("claude", path)

    assert payload["turnPhase"] == "user_prompt_pending"
    assert payload["phaseObservedAt"] == "2026-04-28T11:11:39Z"


def test_probe_transcript_turn_phase_droid_assistant_text_stays_unknown(tmp_path):
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

    payload = probe_transcript_turn_phase("droid", path)

    assert payload["turnPhase"] == "assistant_text_idle"
