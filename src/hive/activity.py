"""Read-only transcript interrupt-safety and artifact-open probes for CLI agents."""

from __future__ import annotations

from collections import deque
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from . import adapters
from .adapters.base import safe_json_loads

_INITIAL_TAIL_BYTES = 8 * 1024
_MAX_TAIL_BYTES = 128 * 1024

# Phases that mean "the agent is in the middle of (or about to start) a turn":
# - tool_open: assistant has opened a tool_use, no result yet
# - tool_result_pending_reply: tool result arrived, assistant has not continued
# - user_prompt_pending: user prompt arrived, assistant has not acknowledged
# - input_backlog: unresolved queue enqueue is the newest decisive evidence
#   (a new prompt is waiting in the queue for the assistant to pick up)
# Used by the sidecar busy probe (_pane_in_active_turn, which also gates
# idle-notify firing) and the duo pairing idle gate (_pane_is_idle_for_pairing).
ACTIVE_TURN_PHASES = frozenset({
    "tool_open",
    "tool_result_pending_reply",
    "user_prompt_pending",
    "input_backlog",
})


def _format_timestamp(value: datetime | None) -> str:
    if value is None:
        return ""
    return value.isoformat().replace("+00:00", "Z")


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
    if isinstance(content, str):
        # Anthropic legacy: ``message.content`` can be a plain string for
        # user records typed straight into the CLI. Treat it as a single
        # text block so downstream detectors (real_user_text, has_text)
        # see it; otherwise turnPhase falls back to ``turn_closed`` and
        # idle-notify mistakes a queued user prompt for an idle turn.
        return [{"type": "text", "text": content}]
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


def _phase_payload(
    *,
    reason: str,
    observed_at: str = "",
    evidence_tail: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "turnPhase": reason,
        "evidence": {"tail": evidence_tail or []},
    }
    if observed_at:
        payload["phaseObservedAt"] = observed_at
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


def _claude_input_backlog_candidate(records: list[dict[str, Any]]) -> tuple[int, dict[str, Any]] | None:
    backlog = 0
    latest_enqueue: tuple[int, dict[str, Any]] | None = None
    for index, record in enumerate(records):
        if record.get("type") != "queue-operation":
            continue
        operation = str(record.get("operation") or "")
        if operation == "enqueue":
            backlog += 1
            latest_enqueue = (index, record)
        elif operation in {"dequeue", "remove"} and backlog > 0:
            backlog -= 1
    if backlog <= 0:
        return None
    return latest_enqueue


def _probe_claude_turn_phase(records: list[dict[str, Any]]) -> dict[str, Any]:
    tail = [_raw_record_summary(record) for record in records]
    backlog_candidate = _claude_input_backlog_candidate(records)

    for index, record in reversed(list(enumerate(records))):
        if backlog_candidate is not None and index == backlog_candidate[0]:
            queue_record = backlog_candidate[1]
            return _phase_payload(
                reason="input_backlog",
                observed_at=_raw_timestamp(queue_record),
                evidence_tail=tail,
            )
        record_type = str(record.get("type") or "")
        if record_type == "system":
            subtype = str(record.get("subtype") or "")
            if subtype == "turn_duration":
                return _phase_payload(
                    reason="turn_closed",
                    observed_at=_raw_timestamp(record),
                    evidence_tail=tail,
                )
            if subtype == "stop_hook_summary" and record.get("preventedContinuation") is False:
                return _phase_payload(
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
                return _phase_payload(
                    reason="tool_open",
                    observed_at=_raw_timestamp(record),
                    evidence_tail=tail,
                )
            if _assistant_has_text(message):
                return _phase_payload(
                    reason="assistant_text_idle",
                    observed_at=_raw_timestamp(record),
                    evidence_tail=tail,
                )
            continue
        if record_type == "user":
            tool_result_state = _claude_tool_result_state(record)
            if tool_result_state is not None:
                return _phase_payload(
                    reason="tool_result_pending_reply",
                    observed_at=_raw_timestamp(record),
                    evidence_tail=tail,
                )
            if _claude_real_user_text(record):
                return _phase_payload(
                    reason="user_prompt_pending",
                    observed_at=_raw_timestamp(record),
                    evidence_tail=tail,
                )

    return _phase_payload(reason="unknown_evidence", evidence_tail=tail)


def probe_transcript_turn_phase(
    cli_name: str,
    transcript: str | Path,
    *,
    sample_limit: int = 12,
) -> dict[str, Any]:
    path = Path(transcript)
    if not path.exists():
        return _phase_payload(reason="unknown_evidence")

    records = _read_tail_payloads(path, sample_limit=sample_limit)
    if not records:
        return _phase_payload(reason="unknown_evidence")

    # codex has no transcript probe: a daemon-backed pane reports natively over
    # its app-server socket, and embedded codex is unsupported (rejected at
    # team entry), so it falls through to unknown_evidence below.
    if cli_name == "claude":
        return _probe_claude_turn_phase(records)
    return _phase_payload(
        reason="unknown_evidence",
        evidence_tail=[_raw_record_summary(record) for record in records],
    )
