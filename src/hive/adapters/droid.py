"""Droid (Factory) session adapter.

Droid writes one JSONL per session under ``$FACTORY_HOME/sessions/<cwd-slug>/<id>.jsonl``.
The first line is a ``session_start`` record carrying the session id, title and cwd.
Subsequent lines are ``message`` records whose payload mirrors the Anthropic message
schema (``content`` is a list of blocks with ``type`` in ``text``/``tool_use``/
``tool_result``/``thinking``).
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any, Iterable, Iterator

from .. import core_hooks, tmux
from .base import (
    Message,
    MessagePart,
    SessionMeta,
    parse_iso_timestamp,
    safe_json_loads,
)


class DroidAdapter:
    name = "droid"

    # --- discovery ---

    def resolve_current_session_id(self, pane_id: str) -> str | None:
        record = core_hooks.resolve_session_record(
            pane_id=pane_id,
            tty=tmux.get_pane_tty(pane_id) or "",
        )
        if not record:
            return None
        session_id = record.get("session_id")
        return str(session_id) if session_id else None

    def _sessions_root(self) -> Path:
        return Path(os.environ.get("FACTORY_HOME", str(Path.home() / ".factory"))) / "sessions"

    def find_session_file(self, session_id: str, *, cwd: str | None = None) -> Path | None:
        if not session_id:
            return None
        root = self._sessions_root()
        if not root.is_dir():
            return None
        candidate = f"{session_id}.jsonl"
        if cwd:
            # Factory slugs paths the same way claude does: replace os.sep with "-"
            # and keep a leading "-". Instead of reinventing the slug, just glob
            # any directory containing the target filename.
            direct = list(root.glob(f"*/{candidate}"))
            if direct:
                return direct[0]
        matches = list(root.rglob(candidate))
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
        files = sorted(root.rglob("*.jsonl"), key=_safe_mtime, reverse=True)
        out: list[SessionMeta] = []
        for path in files:
            meta = self.read_meta(path)
            if not meta:
                continue
            if cwd and meta.cwd != cwd:
                continue
            out.append(meta)
            if limit is not None and len(out) >= limit:
                break
        return out

    # --- reading ---

    def read_meta(self, path: Path) -> SessionMeta | None:
        try:
            with path.open() as handle:
                first_line = handle.readline().strip()
        except OSError:
            return None
        payload = safe_json_loads(first_line)
        if not payload or payload.get("type") != "session_start":
            return None
        session_id = payload.get("id")
        if not session_id:
            return None
        return SessionMeta(
            session_id=str(session_id),
            cli_name=self.name,
            cwd=_str_or_none(payload.get("cwd")),
            title=_str_or_none(payload.get("sessionTitle") or payload.get("title")),
            started_at=None,
            jsonl_path=path,
        )

    def iter_messages(self, path: Path) -> Iterator[Message]:
        try:
            handle = path.open()
        except OSError:
            return iter(())
        return _droid_message_iter(handle)


def _droid_message_iter(handle) -> Iterator[Message]:
    with handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            payload = safe_json_loads(line)
            if not payload or payload.get("type") != "message":
                continue
            msg = payload.get("message")
            if not isinstance(msg, dict):
                continue
            parts = tuple(_iter_droid_parts(msg.get("content")))
            yield Message(
                message_id=_str_or_none(payload.get("id")),
                parent_id=_str_or_none(payload.get("parentId")),
                role=str(msg.get("role") or "unknown"),
                parts=parts,
                timestamp=parse_iso_timestamp(payload.get("timestamp")),
                raw=payload,
            )


def _iter_droid_parts(content: Any) -> Iterator[MessagePart]:
    if isinstance(content, str):
        yield MessagePart(kind="text", text=content)
        return
    if not isinstance(content, list):
        return
    for block in content:
        if not isinstance(block, dict):
            continue
        kind = block.get("type")
        if kind == "text":
            yield MessagePart(kind="text", text=str(block.get("text") or ""), raw=block)
        elif kind == "thinking":
            yield MessagePart(kind="thinking", text=str(block.get("thinking") or ""), raw=block)
        elif kind == "tool_use":
            yield MessagePart(
                kind="tool_use",
                tool_name=_str_or_none(block.get("name")),
                tool_input=block.get("input") if isinstance(block.get("input"), dict) else None,
                raw=block,
            )
        elif kind == "tool_result":
            output = block.get("content")
            if isinstance(output, list):
                text_parts = [b.get("text", "") for b in output if isinstance(b, dict) and b.get("type") == "text"]
                output_text = "\n".join(t for t in text_parts if t)
            else:
                output_text = str(output) if output is not None else None
            yield MessagePart(
                kind="tool_result",
                tool_output=output_text,
                raw=block,
            )
        elif kind == "image":
            yield MessagePart(kind="image", raw=block)
        else:
            yield MessagePart(kind="unknown", raw=block)


def _safe_mtime(path: Path) -> float:
    try:
        return path.stat().st_mtime
    except OSError:
        return -1


def _str_or_none(value: Any) -> str | None:
    if value is None:
        return None
    text = str(value)
    return text or None
