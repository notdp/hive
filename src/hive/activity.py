"""Read-only transcript activity probe for CLI agents."""

from __future__ import annotations

from collections import deque
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from . import adapters
from .adapters.base import Message, safe_json_loads

_INITIAL_TAIL_BYTES = 8 * 1024
_MAX_TAIL_BYTES = 128 * 1024


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


def _raw_timestamp(payload: dict[str, Any]) -> str:
    value = payload.get("timestamp")
    return value if isinstance(value, str) else ""


def _parse_timestamp(value: str) -> datetime | None:
    if not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def _timestamp_at_or_after(value: str, baseline: str) -> bool:
    if not baseline:
        return True
    observed = _parse_timestamp(value)
    baseline_dt = _parse_timestamp(baseline)
    if observed is None or baseline_dt is None:
        return False
    if observed.tzinfo is None:
        observed = observed.replace(tzinfo=UTC)
    if baseline_dt.tzinfo is None:
        baseline_dt = baseline_dt.replace(tzinfo=UTC)
    return observed >= baseline_dt


def _content_blocks(message: dict[str, Any]) -> list[dict[str, Any]]:
    content = message.get("content")
    if not isinstance(content, list):
        return []
    return [block for block in content if isinstance(block, dict)]


def _raw_record_summary(payload: dict[str, Any]) -> dict[str, Any]:
    summary: dict[str, Any] = {
        "type": str(payload.get("type") or "unknown"),
        "observedAt": _raw_timestamp(payload),
    }
    subtype = payload.get("subtype")
    if isinstance(subtype, str) and subtype:
        summary["subtype"] = subtype
    operation = payload.get("operation")
    if isinstance(operation, str) and operation:
        summary["operation"] = operation

    message = payload.get("message")
    if isinstance(message, dict):
        role = message.get("role")
        if isinstance(role, str) and role:
            summary["role"] = role
        part_kinds = [str(block.get("type")) for block in _content_blocks(message) if block.get("type")]
        if part_kinds:
            summary["partKinds"] = part_kinds
        stop_reason = message.get("stop_reason")
        if isinstance(stop_reason, str) and stop_reason:
            summary["stopReason"] = stop_reason

    body = payload.get("payload")
    if isinstance(body, dict):
        item_type = body.get("type")
        if isinstance(item_type, str) and item_type:
            if payload.get("type") == "event_msg":
                summary["eventType"] = item_type
            else:
                summary["itemType"] = item_type
        name = body.get("name")
        if isinstance(name, str) and name:
            summary["name"] = name
        role = body.get("role")
        if isinstance(role, str) and role and "role" not in summary:
            summary["role"] = role
    return summary


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


def _read_tail_messages(adapter: Any, path: Path, *, sample_limit: int) -> list[Message]:
    limit = max(sample_limit, 1)
    try:
        file_size = path.stat().st_size
    except OSError:
        return []
    if file_size == 0:
        return []

    chunk = _INITIAL_TAIL_BYTES
    recent: list[Message] = []
    while chunk <= _MAX_TAIL_BYTES:
        offset = max(0, file_size - chunk)
        try:
            with path.open("rb") as handle:
                handle.seek(offset)
                raw = handle.read()
        except OSError:
            return recent

        data = raw.decode("utf-8", errors="replace")
        lines = data.split("\n")
        if offset > 0:
            lines = lines[1:]

        sampled: deque[Message] = deque(maxlen=limit)
        for line in lines:
            line = line.strip()
            if not line:
                continue
            payload = safe_json_loads(line)
            if payload is None:
                continue
            message = adapter.message_from_record(payload)
            if message is not None:
                sampled.append(message)

        recent = list(sampled)
        if len(recent) >= limit or offset == 0:
            return recent
        chunk *= 2

    return recent


def _read_tail_payloads(path: Path, *, sample_limit: int) -> list[dict[str, Any]]:
    limit = max(sample_limit, 1)
    try:
        file_size = path.stat().st_size
    except OSError:
        return []
    if file_size == 0:
        return []

    chunk = _INITIAL_TAIL_BYTES
    recent: list[dict[str, Any]] = []
    while chunk <= _MAX_TAIL_BYTES:
        offset = max(0, file_size - chunk)
        try:
            with path.open("rb") as handle:
                handle.seek(offset)
                raw = handle.read()
        except OSError:
            return recent

        data = raw.decode("utf-8", errors="replace")
        lines = data.split("\n")
        if offset > 0:
            lines = lines[1:]

        sampled: deque[dict[str, Any]] = deque(maxlen=limit)
        for line in lines:
            line = line.strip()
            if not line:
                continue
            payload = safe_json_loads(line)
            if payload is not None:
                sampled.append(payload)

        recent = list(sampled)
        if len(recent) >= limit or offset == 0:
            return recent
        chunk *= 2

    return recent


def _interrupt_payload(
    *,
    state: str,
    reason: str,
    observed_at: str = "",
    evidence_tail: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "interruptSafety": state,
        "safetyReason": reason,
        "evidence": {"tail": evidence_tail or []},
    }
    if observed_at:
        payload["safetyObservedAt"] = observed_at
    return payload


def _claude_real_user_text(record: dict[str, Any]) -> bool:
    if bool(record.get("isMeta")) or bool(record.get("isSidechain")):
        return False
    if record.get("type") != "user":
        return False
    message = record.get("message")
    if not isinstance(message, dict):
        return False
    for block in _content_blocks(message):
        if block.get("type") != "text":
            continue
        text = str(block.get("text") or "").strip()
        if not text:
            continue
        if text.startswith("<system-reminder>"):
            continue
        return True
    return False


def _claude_tool_result_state(record: dict[str, Any]) -> str | None:
    if record.get("type") != "user":
        return None
    message = record.get("message")
    if not isinstance(message, dict):
        return None
    for block in _content_blocks(message):
        if block.get("type") != "tool_result":
            continue
        return "error" if bool(block.get("is_error")) else "ok"
    return None


def _assistant_has_text(message: dict[str, Any]) -> bool:
    return any(block.get("type") == "text" and str(block.get("text") or "").strip() for block in _content_blocks(message))


def _probe_claude_interrupt_safety(records: list[dict[str, Any]]) -> dict[str, Any]:
    tail = [_raw_record_summary(record) for record in records]
    backlog = 0
    for record in records:
        if record.get("type") != "queue-operation":
            continue
        operation = str(record.get("operation") or "")
        if operation == "enqueue":
            backlog += 1
        elif operation in {"dequeue", "remove"} and backlog > 0:
            backlog -= 1
    if backlog > 0:
        queue_record = next(
            (record for record in reversed(records) if record.get("type") == "queue-operation" and str(record.get("operation") or "") == "enqueue"),
            records[-1] if records else {},
        )
        return _interrupt_payload(
            state="unsafe",
            reason="input_backlog",
            observed_at=_raw_timestamp(queue_record),
            evidence_tail=tail,
        )

    for record in reversed(records):
        record_type = str(record.get("type") or "")
        if record_type == "system":
            subtype = str(record.get("subtype") or "")
            if subtype == "turn_duration":
                return _interrupt_payload(
                    state="safe",
                    reason="turn_closed",
                    observed_at=_raw_timestamp(record),
                    evidence_tail=tail,
                )
            if subtype == "stop_hook_summary" and record.get("preventedContinuation") is False:
                return _interrupt_payload(
                    state="safe",
                    reason="turn_closed",
                    observed_at=_raw_timestamp(record),
                    evidence_tail=tail,
                )
            continue
        if record_type == "assistant":
            message = record.get("message")
            if not isinstance(message, dict):
                continue
            stop_reason = str(message.get("stop_reason") or "")
            part_kinds = [str(block.get("type")) for block in _content_blocks(message)]
            if stop_reason == "tool_use" or "tool_use" in part_kinds:
                return _interrupt_payload(
                    state="unsafe",
                    reason="tool_open",
                    observed_at=_raw_timestamp(record),
                    evidence_tail=tail,
                )
            if _assistant_has_text(message):
                return _interrupt_payload(
                    state="unknown",
                    reason="assistant_text_idle",
                    observed_at=_raw_timestamp(record),
                    evidence_tail=tail,
                )
            continue
        if record_type == "user":
            tool_result_state = _claude_tool_result_state(record)
            if tool_result_state is not None:
                return _interrupt_payload(
                    state="unknown",
                    reason="tool_result_pending_reply",
                    observed_at=_raw_timestamp(record),
                    evidence_tail=tail,
                )
            if _claude_real_user_text(record):
                return _interrupt_payload(
                    state="unknown",
                    reason="user_prompt_pending",
                    observed_at=_raw_timestamp(record),
                    evidence_tail=tail,
                )

    return _interrupt_payload(state="unknown", reason="unknown_evidence", evidence_tail=tail)


def _codex_message_has_text(body: dict[str, Any]) -> bool:
    content = body.get("content")
    if not isinstance(content, list):
        return False
    for part in content:
        if not isinstance(part, dict):
            continue
        if part.get("type") != "output_text":
            continue
        if str(part.get("text") or "").strip():
            return True
    return False


def _probe_codex_interrupt_safety(records: list[dict[str, Any]]) -> dict[str, Any]:
    tail = [_raw_record_summary(record) for record in records]
    for record in reversed(records):
        record_type = str(record.get("type") or "")
        body = record.get("payload")
        if record_type == "event_msg" and isinstance(body, dict):
            event_type = str(body.get("type") or "")
            if event_type == "task_started":
                return _interrupt_payload(
                    state="unsafe",
                    reason="tool_open",
                    observed_at=_raw_timestamp(record),
                    evidence_tail=tail,
                )
            if event_type in {"task_complete", "turn_aborted"}:
                return _interrupt_payload(
                    state="safe",
                    reason="task_closed",
                    observed_at=_raw_timestamp(record),
                    evidence_tail=tail,
                )
            if event_type in {"exec_command_end", "mcp_tool_call_end", "patch_apply_end"}:
                return _interrupt_payload(
                    state="unknown",
                    reason="tool_result_pending_reply",
                    observed_at=_raw_timestamp(record),
                    evidence_tail=tail,
                )
            if event_type == "user_message":
                return _interrupt_payload(
                    state="unknown",
                    reason="user_prompt_pending",
                    observed_at=_raw_timestamp(record),
                    evidence_tail=tail,
                )
        if record_type == "response_item" and isinstance(body, dict):
            item_type = str(body.get("type") or "")
            if item_type in {"function_call", "custom_tool_call"}:
                return _interrupt_payload(
                    state="unsafe",
                    reason="tool_open",
                    observed_at=_raw_timestamp(record),
                    evidence_tail=tail,
                )
            if item_type in {"function_call_output", "custom_tool_call_output"}:
                return _interrupt_payload(
                    state="unknown",
                    reason="tool_result_pending_reply",
                    observed_at=_raw_timestamp(record),
                    evidence_tail=tail,
                )
            if item_type == "message":
                role = str(body.get("role") or "")
                if role == "user":
                    return _interrupt_payload(
                        state="unknown",
                        reason="user_prompt_pending",
                        observed_at=_raw_timestamp(record),
                        evidence_tail=tail,
                    )
                if role == "assistant" and _codex_message_has_text(body):
                    return _interrupt_payload(
                        state="unknown",
                        reason="assistant_text_idle",
                        observed_at=_raw_timestamp(record),
                        evidence_tail=tail,
                    )

    return _interrupt_payload(state="unknown", reason="unknown_evidence", evidence_tail=tail)


def _droid_real_user_text(record: dict[str, Any]) -> bool:
    if record.get("type") != "message":
        return False
    message = record.get("message")
    if not isinstance(message, dict) or str(message.get("role") or "") != "user":
        return False
    saw_non_reminder = False
    for block in _content_blocks(message):
        if block.get("type") != "text":
            continue
        text = str(block.get("text") or "").strip()
        if not text:
            continue
        if text.startswith("<system-reminder>"):
            continue
        saw_non_reminder = True
    return saw_non_reminder


def _droid_has_tool_result(record: dict[str, Any]) -> bool:
    if record.get("type") != "message":
        return False
    message = record.get("message")
    if not isinstance(message, dict):
        return False
    return any(block.get("type") == "tool_result" for block in _content_blocks(message))


def _droid_has_tool_use(record: dict[str, Any]) -> bool:
    if record.get("type") != "message":
        return False
    message = record.get("message")
    if not isinstance(message, dict):
        return False
    return any(block.get("type") == "tool_use" for block in _content_blocks(message))


def _droid_has_assistant_text(record: dict[str, Any]) -> bool:
    if record.get("type") != "message":
        return False
    message = record.get("message")
    if not isinstance(message, dict) or str(message.get("role") or "") != "assistant":
        return False
    return any(
        block.get("type") == "text" and str(block.get("text") or "").strip()
        for block in _content_blocks(message)
    )


def _probe_droid_interrupt_safety(records: list[dict[str, Any]]) -> dict[str, Any]:
    tail = [_raw_record_summary(record) for record in records]
    for record in reversed(records):
        if _droid_has_tool_use(record):
            return _interrupt_payload(
                state="unsafe",
                reason="tool_open",
                observed_at=_raw_timestamp(record),
                evidence_tail=tail,
            )
        if _droid_has_tool_result(record):
            return _interrupt_payload(
                state="unknown",
                reason="tool_result_pending_reply",
                observed_at=_raw_timestamp(record),
                evidence_tail=tail,
            )
        if _droid_real_user_text(record):
            return _interrupt_payload(
                state="unknown",
                reason="user_prompt_pending",
                observed_at=_raw_timestamp(record),
                evidence_tail=tail,
            )
        if _droid_has_assistant_text(record):
            return _interrupt_payload(
                state="unknown",
                reason="assistant_text_idle",
                observed_at=_raw_timestamp(record),
                evidence_tail=tail,
            )

    return _interrupt_payload(state="unknown", reason="unknown_evidence", evidence_tail=tail)


def _claude_artifact_read_match(record: dict[str, Any], artifact_path: str, *, since: str) -> str:
    if record.get("type") != "assistant":
        return ""
    if not _timestamp_at_or_after(_raw_timestamp(record), since):
        return ""
    message = record.get("message")
    if not isinstance(message, dict):
        return ""
    for block in _content_blocks(message):
        if block.get("type") != "tool_use":
            continue
        if str(block.get("name") or "") != "Read":
            continue
        tool_input = block.get("input")
        if not isinstance(tool_input, dict):
            continue
        if str(tool_input.get("file_path") or "") == artifact_path:
            return _raw_timestamp(record)
    return ""


def _droid_artifact_read_match(record: dict[str, Any], artifact_path: str, *, since: str) -> str:
    if record.get("type") != "message":
        return ""
    if not _timestamp_at_or_after(_raw_timestamp(record), since):
        return ""
    message = record.get("message")
    if not isinstance(message, dict) or str(message.get("role") or "") != "assistant":
        return ""
    for block in _content_blocks(message):
        if block.get("type") != "tool_use":
            continue
        if str(block.get("name") or "") != "Read":
            continue
        tool_input = block.get("input")
        if not isinstance(tool_input, dict):
            continue
        if str(tool_input.get("file_path") or "") == artifact_path:
            return _raw_timestamp(record)
    return ""


def _codex_artifact_read_match(record: dict[str, Any], artifact_path: str, *, since: str) -> str:
    if record.get("type") != "event_msg":
        return ""
    if not _timestamp_at_or_after(_raw_timestamp(record), since):
        return ""
    body = record.get("payload")
    if not isinstance(body, dict) or str(body.get("type") or "") != "exec_command_end":
        return ""
    parsed_cmd = body.get("parsed_cmd")
    if not isinstance(parsed_cmd, list):
        return ""
    for item in parsed_cmd:
        if not isinstance(item, dict):
            continue
        if str(item.get("type") or "") != "read":
            continue
        if str(item.get("path") or "") == artifact_path:
            return _raw_timestamp(record)
    return ""


def probe_transcript_artifact_opened(
    cli_name: str,
    transcript: str | Path,
    artifact_path: str,
    *,
    since: str = "",
) -> dict[str, Any]:
    path = Path(transcript)
    if not artifact_path or not path.exists():
        return {"opened": False}

    try:
        with path.open("r", encoding="utf-8", errors="replace") as handle:
            for raw_line in handle:
                line = raw_line.strip()
                if not line:
                    continue
                payload = safe_json_loads(line)
                if not isinstance(payload, dict):
                    continue
                observed_at = ""
                if cli_name == "claude":
                    observed_at = _claude_artifact_read_match(payload, artifact_path, since=since)
                elif cli_name == "droid":
                    observed_at = _droid_artifact_read_match(payload, artifact_path, since=since)
                elif cli_name == "codex":
                    observed_at = _codex_artifact_read_match(payload, artifact_path, since=since)
                if observed_at:
                    return {
                        "opened": True,
                        "observedAt": observed_at,
                    }
    except OSError:
        return {"opened": False}
    return {"opened": False}


def probe_transcript_interrupt_safety(
    cli_name: str,
    transcript: str | Path,
    *,
    sample_limit: int = 12,
) -> dict[str, Any]:
    path = Path(transcript)
    if not path.exists():
        return _interrupt_payload(state="unknown", reason="unknown_evidence")

    records = _read_tail_payloads(path, sample_limit=sample_limit)
    if not records:
        return _interrupt_payload(state="unknown", reason="unknown_evidence")

    if cli_name == "claude":
        return _probe_claude_interrupt_safety(records)
    if cli_name == "codex":
        return _probe_codex_interrupt_safety(records)
    if cli_name == "droid":
        return _probe_droid_interrupt_safety(records)
    return _interrupt_payload(
        state="unknown",
        reason="unknown_evidence",
        evidence_tail=[_raw_record_summary(record) for record in records],
    )


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

    return classify_activity(_read_tail_messages(adapter, path, sample_limit=sample_limit))
