from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any


_CODEX_COMMAND_SKILL_RE = re.compile(r"^\s*\$(?:cvim|vim)(?:\s|$)")


def _available_adapter_names() -> list[str]:
    from hive import adapters as hive_adapters

    return hive_adapters.available()


def _get_adapter(name: str):
    from hive import adapters as hive_adapters

    return hive_adapters.get(name)


def _detect_profile_for_pane(pane_id: str):
    from hive.agent_cli import detect_profile_for_pane

    return detect_profile_for_pane(pane_id)


def _resolve_hive_runtime_session_id(pane_id: str, cli_name: str = "") -> tuple[bool, str | None]:
    try:
        from hive import tmux
        from hive.hived import request_runtime_snapshot
    except Exception:
        return False, None

    try:
        workspace = tmux.display_value(pane_id, "#{@hive-workspace}") or ""
    except Exception:
        return False, None
    if not workspace:
        return False, None

    try:
        payload = request_runtime_snapshot(workspace, pane_id=pane_id)
        if not isinstance(payload, dict) or payload.get("ok") is False:
            return False, None
        snapshot = payload.get("snapshot")
        if not isinstance(snapshot, dict):
            # No hived snapshot for this pane: not hived-managed truth.
            # The adapter is the authority (claude resolves through its bg
            # job record / session registry, codex through its thread
            # record), so fall through to it.
            return False, None
        if snapshot.get("_sessionIdFresh") is False:
            return True, None
        session_id = snapshot.get("sessionId")
        if isinstance(session_id, str) and session_id and session_id != "unresolved":
            return True, session_id
    except Exception:
        return False, None
    return False, None


def list_recent_assistant_messages(
    file_path: Path, *, limit: int = 10
) -> list[dict[str, Any]]:
    """Return up to *limit* most-recent assistant messages, newest first.

    Each entry carries the raw *offset* such that
    ``extract_last_assistant_text(file_path, offset=offset)`` returns the same
    text (i.e. this walks assistant messages in the same order). Entries also
    include a ``timestamp`` (HH:MM local, ``""`` when missing) and an 80-char
    first-line ``preview`` suitable for a menu label.
    """
    adapter = _detect_adapter_for_transcript(file_path)
    entries: list[dict[str, Any]]
    if adapter is not None:
        entries = _list_messages_via_adapter(adapter, file_path, limit=limit)
    else:
        entries = _list_messages_via_raw_jsonl(file_path, limit=limit)
    return entries


def _list_messages_via_adapter(adapter, file_path: Path, *, limit: int) -> list[dict[str, Any]]:
    try:
        messages = list(adapter.iter_messages(file_path))
    except Exception:
        return []
    out: list[dict[str, Any]] = []
    offset = 0
    for message in reversed(messages):
        if getattr(message, "role", "") != "assistant":
            continue
        text = _assistant_text_from_normalized_message(message)
        if not text:
            continue
        timestamp = _format_timestamp(getattr(message, "timestamp", None))
        out.append({
            "offset": offset,
            "timestamp": timestamp,
            "preview": _build_preview(text),
            "text": text,
        })
        offset += 1
        if offset >= limit:
            break
    return out


def _list_messages_via_raw_jsonl(file_path: Path, *, limit: int) -> list[dict[str, Any]]:
    try:
        lines = file_path.read_text(errors="ignore").splitlines()
    except OSError:
        return []
    out: list[dict[str, Any]] = []
    offset = 0
    for line in reversed(lines):
        try:
            obj = json.loads(line)
        except Exception:
            continue
        if obj.get("type") != "message":
            continue
        message = obj.get("message") or {}
        if message.get("role") != "assistant":
            continue
        text = _assistant_text_from_raw_claude_message(message)
        if not text:
            continue
        out.append({
            "offset": offset,
            "timestamp": _format_timestamp(obj.get("timestamp")),
            "preview": _build_preview(text),
            "text": text,
        })
        offset += 1
        if offset >= limit:
            break
    return out


def _format_timestamp(value: Any) -> str:
    from datetime import datetime

    if isinstance(value, datetime):
        return value.astimezone().strftime("%H:%M")
    if isinstance(value, str) and value:
        raw = value.replace("Z", "+00:00") if value.endswith("Z") else value
        try:
            dt = datetime.fromisoformat(raw)
        except ValueError:
            return ""
        return dt.astimezone().strftime("%H:%M")
    return ""


def _build_preview(text: str, *, width: int = 80) -> str:
    for line in text.splitlines():
        stripped = line.strip()
        if stripped:
            return stripped if len(stripped) <= width else stripped[:width - 1] + "…"
    return ""


def _assistant_text_from_raw_claude_message(message: dict[str, Any]) -> str:
    parts: list[str] = []
    for item in message.get("content") or []:
        if item.get("type") == "text":
            text = item.get("text") or ""
            if text.strip():
                parts.append(text.rstrip("\n"))
        elif item.get("type") == "tool_use" and item.get("name") in ("ExitSpecMode", "ExitPlanMode"):
            tool_input = item.get("input") or {}
            plan = tool_input.get("plan") if isinstance(tool_input, dict) else ""
            title = tool_input.get("title") if isinstance(tool_input, dict) else ""
            if isinstance(plan, str) and plan.strip():
                header = ""
                if isinstance(title, str) and title.strip():
                    header = f'Propose Specification title: "{title.strip()}"\n\n'
                parts.append(f"{header}Specification for approval:\n\n{plan.strip()}")
    return "\n\n".join(parts).strip() if parts else ""


def extract_last_assistant_text(file_path: Path, offset: int = 0) -> str:
    """Return the Nth assistant message from the end (0=last, 1=second-to-last, ...)."""
    adapter = _detect_adapter_for_transcript(file_path)
    if adapter is not None:
        return _extract_last_assistant_text_via_adapter(
            adapter,
            file_path,
            offset=resolve_assistant_offset(file_path, offset=offset, adapter=adapter),
        )

    try:
        lines = file_path.read_text(errors="ignore").splitlines()
    except OSError:
        return ""
    skip = offset
    for line in reversed(lines):
        try:
            obj = json.loads(line)
        except Exception:
            continue
        if obj.get("type") != "message":
            continue
        message = obj.get("message") or {}
        if message.get("role") != "assistant":
            continue
        text_result = _assistant_text_from_raw_claude_message(message)
        if text_result:
            if skip <= 0:
                return text_result
            skip -= 1
    return ""


def _detect_adapter_for_transcript(file_path: Path):
    try:
        for name in _available_adapter_names():
            adapter = _get_adapter(name)
            if adapter is None:
                continue
            try:
                meta = adapter.read_meta(file_path)
            except Exception:
                meta = None
            if meta is not None:
                return adapter
    except Exception:
        return None
    return None


def _extract_last_assistant_text_via_adapter(adapter, file_path: Path, *, offset: int = 0) -> str:
    skip = offset
    try:
        messages = list(adapter.iter_messages(file_path))
    except Exception:
        return ""
    for message in reversed(messages):
        if getattr(message, "role", "") != "assistant":
            continue
        text_result = _assistant_text_from_normalized_message(message)
        if text_result:
            if skip <= 0:
                return text_result
            skip -= 1
    return ""


def resolve_assistant_offset(file_path: Path, offset: int = 0, *, adapter=None) -> int:
    if adapter is None:
        adapter = _detect_adapter_for_transcript(file_path)
    if adapter is None or getattr(adapter, "name", "") != "codex":
        return offset
    try:
        messages = list(adapter.iter_messages(file_path))
    except Exception:
        return offset
    return _resolve_codex_skill_turn_offset(messages, offset=offset)


def _resolve_codex_skill_turn_offset(messages: list[Any], *, offset: int = 0) -> int:
    tail_turn_id = None
    for message in reversed(messages):
        turn_id = _message_turn_id(message)
        if turn_id:
            tail_turn_id = turn_id
            break
    if not tail_turn_id:
        return offset

    tail_turn = [message for message in messages if _message_turn_id(message) == tail_turn_id]
    if not _turn_invokes_codex_command_skill(tail_turn):
        return offset

    synthetic_assistant_messages = sum(
        1 for message in tail_turn if _is_codex_commentary_assistant_message(message)
    )
    return offset + synthetic_assistant_messages


def _message_turn_id(message: Any) -> str | None:
    raw = getattr(message, "raw", None)
    if not isinstance(raw, dict):
        return None
    turn_id = raw.get("turn_id")
    return turn_id if isinstance(turn_id, str) and turn_id else None


def _turn_invokes_codex_command_skill(messages: list[Any]) -> bool:
    for message in messages:
        if getattr(message, "role", "") != "user":
            continue
        for item in getattr(message, "parts", ()) or ():
            if getattr(item, "kind", "") != "text":
                continue
            text = getattr(item, "text", "") or ""
            if isinstance(text, str) and _CODEX_COMMAND_SKILL_RE.match(text):
                return True
    return False


def _is_codex_commentary_assistant_message(message: Any) -> bool:
    if getattr(message, "role", "") != "assistant":
        return False
    raw = getattr(message, "raw", None)
    if not isinstance(raw, dict):
        return False
    payload = raw.get("payload")
    if not isinstance(payload, dict):
        return False
    return payload.get("type") == "message" and payload.get("phase") == "commentary"


def _assistant_text_from_normalized_message(message: Any) -> str:
    parts: list[str] = []
    for item in getattr(message, "parts", ()) or ():
        kind = getattr(item, "kind", "")
        if kind == "text":
            text = getattr(item, "text", "") or ""
            if isinstance(text, str) and text.strip():
                parts.append(text.rstrip("\n"))
        elif kind == "tool_use" and getattr(item, "tool_name", "") in ("ExitSpecMode", "ExitPlanMode"):
            tool_input = getattr(item, "tool_input", None) or {}
            if not isinstance(tool_input, dict):
                continue
            plan = tool_input.get("plan")
            title = tool_input.get("title")
            if isinstance(plan, str) and plan.strip():
                header = ""
                if isinstance(title, str) and title.strip():
                    header = f'Propose Specification title: "{title.strip()}"\n\n'
                parts.append(f"{header}Specification for approval:\n\n{plan.strip()}")
    return "\n\n".join(parts).strip() if parts else ""


def write_seed(cwd: str, dst: Path, preferred: Path | None = None, offset: int = 0) -> None:
    if preferred is not None:
        text = extract_last_assistant_text(preferred, offset=offset)
        dst.write_text(text + "\n" if text else "")
        return
    dst.write_text("")


def resolve_transcript_path_for_pane(
    *,
    pane_id: str,
    cwd: str,
) -> str | None:
    if pane_id:
        try:
            profile = _detect_profile_for_pane(pane_id)
        except Exception:
            profile = None
        if profile is not None:
            adapter = _get_adapter(profile.name)
            if adapter is not None:
                hive_managed, session_id = _resolve_hive_runtime_session_id(pane_id, profile.name)
                if not hive_managed:
                    try:
                        session_id = adapter.resolve_current_session_id(pane_id)
                    except Exception:
                        session_id = None
                if session_id:
                    try:
                        transcript_path = adapter.find_session_file(session_id, cwd=cwd)
                    except Exception:
                        transcript_path = None
                    if transcript_path is not None and Path(transcript_path).is_file():
                        return str(transcript_path)
    return None
