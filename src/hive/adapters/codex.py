"""Codex session adapter.

Codex stores every session as a JSONL file under
``$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<timestamp>-<session_id>.jsonl``.
Unlike droid and claude, the on-disk layout is partitioned by *date* rather
than by cwd, so ``find_session_file(session_id, cwd=...)`` ignores the cwd hint
and walks the sessions tree.

The first line of each file is ``{"type": "session_meta", "payload": {...}}``
carrying the session id, cwd, model provider and base instructions. Subsequent
lines are ``response_item`` records whose ``payload`` mirrors the OpenAI
Responses API shape; we currently normalize ``message`` items and surface
``reasoning`` / ``function_call`` / ``function_call_output`` as best-effort
parts, everything else degrades to ``kind="unknown"`` with the raw payload
preserved.
"""

from __future__ import annotations

import os
import re
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


_CODEX_SESSION_UUID_RE = re.compile(
    r"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$"
)


def _codex_home() -> Path:
    return Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex")))


class CodexAdapter:
    name = "codex"

    # --- discovery ---

    def resolve_current_session_id(self, pane_id: str) -> str | None:
        record = core_hooks.resolve_session_record(
            pane_id=pane_id,
            tty=tmux.get_pane_tty(pane_id) or "",
        )
        if record:
            session_id = record.get("session_id")
            if session_id:
                return str(session_id)

        sessions_prefix = str(_codex_home() / "sessions") + "/"
        tty = tmux.get_pane_tty(pane_id) or ""
        for process in tmux.list_tty_processes(tty):
            if _normalize(process.command) != "codex":
                continue
            for fpath in tmux.list_open_files(process.pid):
                if not fpath.startswith(sessions_prefix) or not fpath.endswith(".jsonl"):
                    continue
                match = _CODEX_SESSION_UUID_RE.search(fpath)
                if match:
                    return match.group(1)

        sessions_dir = _codex_home() / "sessions"
        cwd = tmux.display_value(pane_id, "#{pane_current_path}") or ""
        if not cwd or not sessions_dir.is_dir():
            return None
        for path in sorted(sessions_dir.rglob("*.jsonl"), key=_safe_mtime, reverse=True):
            meta = self.read_meta(path)
            if meta and meta.cwd == cwd:
                return meta.session_id
        return None

    def _sessions_root(self) -> Path:
        return _codex_home() / "sessions"

    def find_session_file(self, session_id: str, *, cwd: str | None = None) -> Path | None:
        if not session_id:
            return None
        root = self._sessions_root()
        if not root.is_dir():
            return None
        suffix = f"-{session_id}.jsonl"
        matches = [p for p in root.rglob("rollout-*.jsonl") if p.name.endswith(suffix)]
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
        files = sorted(root.rglob("rollout-*.jsonl"), key=_safe_mtime, reverse=True)
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
        if not payload or payload.get("type") != "session_meta":
            return None
        body = payload.get("payload")
        if not isinstance(body, dict):
            return None
        session_id = body.get("id")
        if not session_id:
            return None
        return SessionMeta(
            session_id=str(session_id),
            cli_name=self.name,
            cwd=_str_or_none(body.get("cwd")),
            title=None,
            started_at=parse_iso_timestamp(payload.get("timestamp") or body.get("timestamp")),
            jsonl_path=path,
        )

    def iter_messages(self, path: Path) -> Iterator[Message]:
        try:
            handle = path.open()
        except OSError:
            return iter(())
        return _codex_message_iter(handle)


def _codex_message_iter(handle) -> Iterator[Message]:
    current_turn_id: str | None = None
    with handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            payload = safe_json_loads(line)
            if not payload:
                continue
            item_kind = payload.get("type")
            if item_kind == "event_msg":
                body = payload.get("payload")
                turn_id = body.get("turn_id") if isinstance(body, dict) else None
                if isinstance(turn_id, str) and turn_id:
                    current_turn_id = turn_id
                continue
            if item_kind == "turn_context":
                body = payload.get("payload")
                turn_id = body.get("turn_id") if isinstance(body, dict) else None
                if isinstance(turn_id, str) and turn_id:
                    current_turn_id = turn_id
                continue
            if item_kind != "response_item":
                continue
            body = payload.get("payload")
            if not isinstance(body, dict):
                continue
            item_type = body.get("type")
            timestamp = parse_iso_timestamp(payload.get("timestamp"))
            raw_payload = dict(payload)
            if current_turn_id:
                raw_payload["turn_id"] = current_turn_id
            if item_type == "message":
                parts = tuple(_iter_codex_message_parts(body.get("content")))
                yield Message(
                    message_id=None,
                    parent_id=None,
                    role=str(body.get("role") or "unknown"),
                    parts=parts,
                    timestamp=timestamp,
                    raw=raw_payload,
                )
            elif item_type == "reasoning":
                text = _extract_reasoning_text(body)
                yield Message(
                    message_id=None,
                    parent_id=None,
                    role="assistant",
                    parts=(MessagePart(kind="thinking", text=text, raw=body),),
                    timestamp=timestamp,
                    raw=raw_payload,
                )
            elif item_type == "function_call":
                args = body.get("arguments")
                tool_input: dict[str, Any] | None = None
                if isinstance(args, dict):
                    tool_input = args
                elif isinstance(args, str):
                    parsed = safe_json_loads(args)
                    if parsed is not None:
                        tool_input = parsed
                yield Message(
                    message_id=_str_or_none(body.get("call_id")),
                    parent_id=None,
                    role="assistant",
                    parts=(
                        MessagePart(
                            kind="tool_use",
                            tool_name=_str_or_none(body.get("name")),
                            tool_input=tool_input,
                            raw=body,
                        ),
                    ),
                    timestamp=timestamp,
                    raw=raw_payload,
                )
            elif item_type == "function_call_output":
                output = body.get("output")
                if isinstance(output, dict):
                    output_text = _str_or_none(output.get("content") or output.get("text"))
                else:
                    output_text = _str_or_none(output)
                yield Message(
                    message_id=_str_or_none(body.get("call_id")),
                    parent_id=None,
                    role="tool",
                    parts=(MessagePart(kind="tool_result", tool_output=output_text, raw=body),),
                    timestamp=timestamp,
                    raw=raw_payload,
                )
            else:
                yield Message(
                    message_id=None,
                    parent_id=None,
                    role="unknown",
                    parts=(MessagePart(kind="unknown", raw=body),),
                    timestamp=timestamp,
                    raw=raw_payload,
                )


def _iter_codex_message_parts(content: Any) -> Iterator[MessagePart]:
    if isinstance(content, str):
        yield MessagePart(kind="text", text=content)
        return
    if not isinstance(content, list):
        return
    for block in content:
        if not isinstance(block, dict):
            continue
        kind = block.get("type")
        if kind in {"input_text", "output_text", "text"}:
            yield MessagePart(kind="text", text=str(block.get("text") or ""), raw=block)
        elif kind == "image" or kind == "input_image":
            yield MessagePart(kind="image", raw=block)
        elif kind == "tool_use":
            yield MessagePart(
                kind="tool_use",
                tool_name=_str_or_none(block.get("name")),
                tool_input=block.get("input") if isinstance(block.get("input"), dict) else None,
                raw=block,
            )
        elif kind == "tool_result":
            yield MessagePart(
                kind="tool_result",
                tool_output=_str_or_none(block.get("content") or block.get("text")),
                raw=block,
            )
        else:
            yield MessagePart(kind="unknown", raw=block)


def _extract_reasoning_text(body: dict[str, Any]) -> str | None:
    summary = body.get("summary")
    if isinstance(summary, list):
        chunks = [s.get("text", "") for s in summary if isinstance(s, dict)]
        joined = "\n".join(c for c in chunks if c)
        if joined:
            return joined
    text = body.get("text")
    if isinstance(text, str) and text:
        return text
    return None


def _normalize(value: str) -> str:
    value = (value or "").strip().lower().rsplit("/", 1)[-1]
    return value.lstrip("-")


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
