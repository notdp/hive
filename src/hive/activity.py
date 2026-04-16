"""Read-only transcript activity probe for CLI agents."""

from __future__ import annotations

from collections import deque
from datetime import datetime
from pathlib import Path
from typing import Any, Iterable

from . import adapters
from .adapters.base import Message


def _format_timestamp(value: datetime | None) -> str:
    if value is None:
        return ""
    return value.isoformat().replace("+00:00", "Z")


def _message_summary(message: Message) -> dict[str, Any]:
    return {
        "role": message.role,
        "partKinds": [part.kind for part in message.parts],
        "observedAt": _format_timestamp(message.timestamp),
    }


def classify_activity(messages: list[Message]) -> dict[str, Any]:
    if not messages:
        return {
            "activityState": "unknown",
            "activityReason": "no_messages",
            "evidence": {"tail": []},
        }

    tail = [_message_summary(message) for message in messages]
    last = messages[-1]
    last_kinds = [part.kind for part in last.parts]
    payload: dict[str, Any] = {
        "activityState": "unknown",
        "activityReason": "unsupported_last_message",
        "activityObservedAt": _format_timestamp(last.timestamp),
        "activityRole": last.role,
        "activityPartKinds": last_kinds,
        "evidence": {"tail": tail},
    }

    if last.role in {"user", "tool"}:
        payload["activityState"] = "active"
        payload["activityReason"] = f"last_role_{last.role}"
        return payload

    if last.role != "assistant":
        payload["activityReason"] = f"last_role_{last.role or 'unknown'}"
        return payload

    if "tool_use" in last_kinds:
        payload["activityState"] = "active"
        payload["activityReason"] = "assistant_tool_use_open"
        return payload

    if last_kinds and all(kind == "thinking" for kind in last_kinds):
        payload["activityState"] = "active"
        payload["activityReason"] = "assistant_thinking_only"
        return payload

    if last_kinds:
        payload["activityState"] = "idle"
        payload["activityReason"] = "assistant_terminal_message"
        return payload

    payload["activityReason"] = "assistant_without_parts"
    return payload


def probe_transcript_activity(
    cli_name: str,
    transcript: str | Path,
    *,
    sample_limit: int = 4,
) -> dict[str, Any]:
    path = Path(transcript)
    if not path.exists():
        return {
            "activityState": "unknown",
            "activityReason": "transcript_missing",
            "evidence": {"tail": []},
        }

    adapter = adapters.get(cli_name)
    if adapter is None:
        return {
            "activityState": "unknown",
            "activityReason": f"no_adapter_{cli_name or 'unknown'}",
            "evidence": {"tail": []},
        }

    recent: deque[Message] = deque(maxlen=max(sample_limit, 1))
    for message in adapter.iter_messages(path):
        recent.append(message)

    return classify_activity(list(recent))
