"""Grok session adapter.

Grok stores every session as a directory under
``$GROK_HOME/sessions/<urllib.parse.quote(cwd, safe="")>/<session_id>/``. The
conversation lives in ``chat_history.jsonl`` — one record per line typed
``system`` / ``user`` / ``assistant`` / ``reasoning`` / ``tool_result`` — and a
sibling ``summary.json`` carries title/model/timestamp once grok writes it.

Unlike claude and codex the records carry no session id, cwd or uuid: the path
is the metadata, so ``read_meta`` reads the two enclosing directory names and
only falls back to the file for the model.
"""

from __future__ import annotations

from datetime import datetime, timezone
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

_HISTORY_NAME = "chat_history.jsonl"
_META_SCAN_LIMIT = 20
_ROLE_BY_TYPE = {"user": "user", "assistant": "assistant", "system": "system"}


class GrokAdapter:
    name = "grok"

    # --- discovery ---

    def resolve_current_session_id(self, pane_id: str) -> str | None:
        # A grok session is owned by its per-pane leader daemon, which records
        # the minted session id in the pane session file.
        from .grok_leader import session_id_for_pane

        return session_id_for_pane(pane_id)

    def _sessions_root(self) -> Path:
        from .grok_leader import grok_home

        return grok_home() / "sessions"

    def find_session_file(self, session_id: str, *, cwd: str | None = None) -> Path | None:
        if not session_id:
            return None
        root = self._sessions_root()
        if not root.is_dir():
            return None
        if cwd:
            direct = root / quote(cwd, safe="") / session_id / _HISTORY_NAME
            if direct.exists():
                return direct
        matches = sorted(root.glob(f"*/{session_id}/{_HISTORY_NAME}"))
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
        files = sorted(root.glob(f"*/*/{_HISTORY_NAME}"), key=safe_mtime, reverse=True)
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
        if path.name != _HISTORY_NAME:
            return None
        session_id = path.parent.name
        if not session_id:
            return None
        try:
            summary = safe_json_loads((path.parent / "summary.json").read_text()) or {}
        except OSError:
            summary = {}
        started_at = parse_iso_timestamp(summary.get("timestamp"))
        if started_at is None:
            mtime = safe_mtime(path)
            started_at = datetime.fromtimestamp(mtime, timezone.utc) if mtime >= 0 else None
        return SessionMeta(
            session_id=session_id,
            cli_name=self.name,
            cwd=unquote(path.parent.parent.name),
            title=str_or_none(summary.get("title")),
            started_at=started_at,
            jsonl_path=path,
            model=str_or_none(summary.get("model")) or _first_assistant_model(path),
        )

    def iter_messages(self, path: Path) -> Iterator[Message]:
        try:
            handle = path.open()
        except OSError:
            return iter(())
        return _grok_message_iter(handle)

    def message_from_record(self, payload: dict[str, Any]) -> Message | None:
        return _message_from_record(payload)


def _grok_message_iter(handle) -> Iterator[Message]:
    with handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            payload = safe_json_loads(line)
            if not payload:
                continue
            message = _message_from_record(payload)
            if message is not None:
                yield message


def _message_from_record(payload: dict[str, Any]) -> Message | None:
    record_type = payload.get("type")
    content = payload.get("content")
    role = _ROLE_BY_TYPE.get(record_type)
    if role:
        return _message(role, tuple(_iter_grok_parts(content)), payload)
    if record_type == "reasoning":
        text = _text_of(content) or str_or_none(payload.get("text"))
        return _message("assistant", (MessagePart(kind="thinking", text=text, raw=payload),), payload)
    if record_type == "tool_result":
        return _message(
            "tool",
            (
                MessagePart(
                    kind="tool_result",
                    tool_name=str_or_none(payload.get("tool_name")),
                    tool_output=_text_of(content),
                    raw=payload,
                ),
            ),
            payload,
        )
    return None


def _message(role: str, parts: tuple[MessagePart, ...], payload: dict[str, Any]) -> Message:
    return Message(
        message_id=None,
        parent_id=None,
        role=role,
        parts=parts,
        timestamp=parse_iso_timestamp(payload.get("ts")),
        raw=payload,
    )


def _iter_grok_parts(content: Any) -> Iterator[MessagePart]:
    if isinstance(content, str):
        yield MessagePart(kind="text", text=content)
        return
    if not isinstance(content, list):
        return
    for block in content:
        if not isinstance(block, dict):
            continue
        if block.get("type") == "text":
            yield MessagePart(kind="text", text=str(block.get("text") or ""), raw=block)
        else:
            yield MessagePart(kind="unknown", raw=block)


def _text_of(content: Any) -> str | None:
    if isinstance(content, str):
        return content or None
    if isinstance(content, list):
        chunks = [
            str(block.get("text") or "")
            for block in content
            if isinstance(block, dict) and block.get("type") == "text"
        ]
        return "\n".join(c for c in chunks if c) or None
    return None


def _first_assistant_model(path: Path) -> str | None:
    try:
        with path.open() as handle:
            for _ in range(_META_SCAN_LIMIT):
                raw = handle.readline()
                if not raw:
                    break
                payload = safe_json_loads(raw.strip())
                if not payload or payload.get("type") != "assistant":
                    continue
                model = str_or_none(payload.get("model_id"))
                if model:
                    return model
    except OSError:
        return None
    return None
