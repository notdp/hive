"""Grok session adapter.

Grok stores each session under
``$GROK_HOME/sessions/<urlencoded-cwd>/<session-id>/`` with ``summary.json``
as the index and ``updates.jsonl`` as the ACP event log.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any, Iterable, Iterator
from urllib.parse import quote, unquote

from .base import (
    Message,
    MessagePart,
    SessionMeta,
    parse_iso_timestamp,
    safe_json_loads,
    safe_mtime,
    str_or_none,
)


def _grok_home() -> Path:
    return Path(os.environ.get("GROK_HOME", str(Path.home() / ".grok")))


def _cwd_group(root: Path, cwd: str) -> Path | None:
    if not cwd:
        return None
    encoded = quote(cwd, safe="")
    direct = root / encoded
    if direct.is_dir():
        return direct
    if not root.is_dir():
        return None
    for group in root.iterdir():
        if not group.is_dir():
            continue
        marker = group / ".cwd"
        try:
            if marker.read_text().strip() == cwd:
                return group
        except OSError:
            continue
        if unquote(group.name) == cwd:
            return group
    return None


class GrokAdapter:
    name = "grok"

    def resolve_current_session_id(self, pane_id: str) -> str | None:
        from .grok_acp import session_id_for_pane

        return session_id_for_pane(pane_id)

    def _sessions_root(self) -> Path:
        return _grok_home() / "sessions"

    def find_session_file(self, session_id: str, *, cwd: str | None = None) -> Path | None:
        if not session_id:
            return None
        root = self._sessions_root()
        if cwd:
            group = _cwd_group(root, cwd)
            if group is not None:
                candidate = group / session_id / "updates.jsonl"
                if candidate.exists():
                    return candidate
        if not root.is_dir():
            return None
        matches = list(root.glob(f"*/{session_id}/updates.jsonl"))
        return matches[0] if matches else None

    def list_sessions(
        self,
        *,
        cwd: str | None = None,
        limit: int | None = None,
    ) -> Iterable[SessionMeta]:
        root = self._sessions_root()
        if not root.is_dir():
            return []
        groups: list[Path]
        if cwd:
            group = _cwd_group(root, cwd)
            groups = [group] if group is not None else []
        else:
            groups = [p for p in root.iterdir() if p.is_dir()]
        files: list[Path] = []
        for group in groups:
            files.extend(group.glob("*/updates.jsonl"))
        files.sort(key=safe_mtime, reverse=True)
        out: list[SessionMeta] = []
        for path in files:
            meta = self.read_meta(path)
            if not meta:
                continue
            out.append(meta)
            if limit is not None and len(out) >= limit:
                break
        return out

    def read_meta(self, path: Path) -> SessionMeta | None:
        summary = path.parent / "summary.json"
        try:
            payload = json.loads(summary.read_text())
        except (OSError, json.JSONDecodeError):
            payload = None
        if not isinstance(payload, dict):
            return None
        info = payload.get("info") if isinstance(payload.get("info"), dict) else {}
        session_id = str_or_none(info.get("id") or payload.get("id") or path.parent.name)
        if not session_id:
            return None
        return SessionMeta(
            session_id=session_id,
            cli_name=self.name,
            cwd=str_or_none(info.get("cwd") or payload.get("cwd")),
            title=str_or_none(payload.get("generated_title") or payload.get("session_summary")),
            started_at=parse_iso_timestamp(payload.get("created_at")),
            jsonl_path=path,
            model=str_or_none(payload.get("current_model_id") or payload.get("model")),
        )

    def iter_messages(self, path: Path) -> Iterator[Message]:
        try:
            handle = path.open()
        except OSError:
            return iter(())
        return _grok_message_iter(handle)

    def message_from_record(self, payload: dict[str, Any]) -> Message | None:
        return _message_from_update(payload)


def _update_body(payload: dict[str, Any]) -> dict[str, Any] | None:
    params = payload.get("params")
    if isinstance(params, dict) and isinstance(params.get("update"), dict):
        return params["update"]
    if isinstance(payload.get("update"), dict):
        return payload["update"]
    return None


def _message_from_update(payload: dict[str, Any]) -> Message | None:
    update = _update_body(payload)
    if not update:
        return None
    kind = update.get("sessionUpdate")
    timestamp = parse_iso_timestamp(payload.get("timestamp"))
    if isinstance(payload.get("timestamp"), (int, float)):
        timestamp = None
    if kind == "user_message_chunk":
        text = _text_from_content(update.get("content"))
        return Message(
            message_id=None,
            parent_id=None,
            role="user",
            parts=(MessagePart(kind="text", text=text, raw=update),),
            timestamp=timestamp,
            raw=payload,
        )
    if kind in {"agent_message_chunk", "agent_thought_chunk"}:
        text = _text_from_content(update.get("content"))
        part_kind = "thinking" if kind == "agent_thought_chunk" else "text"
        return Message(
            message_id=None,
            parent_id=None,
            role="assistant",
            parts=(MessagePart(kind=part_kind, text=text, raw=update),),
            timestamp=timestamp,
            raw=payload,
        )
    if kind == "tool_call":
        tool_meta = ((update.get("_meta") or {}).get("x.ai/tool") or {})
        return Message(
            message_id=str_or_none(update.get("toolCallId")),
            parent_id=None,
            role="assistant",
            parts=(
                MessagePart(
                    kind="tool_use",
                    tool_name=str_or_none(tool_meta.get("name") or update.get("title")),
                    tool_input=update.get("rawInput") if isinstance(update.get("rawInput"), dict) else None,
                    raw=update,
                ),
            ),
            timestamp=timestamp,
            raw=payload,
        )
    if kind == "tool_call_update":
        return Message(
            message_id=str_or_none(update.get("toolCallId")),
            parent_id=None,
            role="tool",
            parts=(MessagePart(kind="tool_result", tool_output=_tool_output_text(update), raw=update),),
            timestamp=timestamp,
            raw=payload,
        )
    return None


def _grok_message_iter(handle) -> Iterator[Message]:
    with handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            payload = safe_json_loads(line)
            if not payload:
                continue
            message = _message_from_update(payload)
            if message is not None:
                yield message


def _text_from_content(content: Any) -> str | None:
    if isinstance(content, str):
        return content
    if isinstance(content, dict):
        return str_or_none(content.get("text"))
    return None


def _tool_output_text(update: dict[str, Any]) -> str | None:
    raw = update.get("rawOutput")
    if isinstance(raw, dict):
        return str_or_none(raw.get("output_for_prompt") or raw.get("content") or raw.get("text"))
    content = update.get("content")
    if isinstance(content, list):
        chunks = []
        for block in content:
            if isinstance(block, dict):
                inner = block.get("content")
                if isinstance(inner, dict):
                    chunks.append(inner.get("text") or "")
        joined = "\n".join(c for c in chunks if c)
        return joined or None
    return None
