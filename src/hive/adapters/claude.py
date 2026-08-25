"""Claude Code session adapter.

Claude stores session history under ``$CLAUDE_HOME/projects/<cwd-slug>/<id>.jsonl``.
Every record carries ``sessionId``, ``cwd``, ``parentUuid``, ``timestamp`` and
``gitBranch``; the ``message.content`` field is an Anthropic-style list of blocks
or (rarely) a plain string.

Current-session resolution: a hive claude member is a bg job, so its pane's
job record answers directly (the live engine entry's sessionId, which follows
an in-session ``/clear``). An interactive claude on the pane tty — a guest
session, a human's own pane — resolves through its ``sessions/<pid>.json``
registry entry, which claude keeps current itself.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any, Iterable, Iterator

from .. import tmux
from .base import (
    Message,
    MessagePart,
    SessionMeta,
    normalize_command_token,
    parse_iso_timestamp,
    safe_json_loads,
    safe_mtime,
    str_or_none,
)


def _claude_home() -> Path:
    # One resolver for every reader of the claude config tree (delivery reads
    # the session registry through the same function).
    from .claude_sessions import _config_dir

    return _config_dir()


class ClaudeAdapter:
    name = "claude"

    # --- discovery ---

    def resolve_current_session_id(self, pane_id: str) -> str | None:
        # A bg member pane answers from its job record (engine entry first —
        # it follows an in-session /clear — then the record's snapshot for a
        # parked engine).
        from .claude_bg import session_id_for_pane

        session_id = session_id_for_pane(pane_id)
        if session_id:
            return session_id
        # Interactive claude on the pane tty (guest pane, a human's own
        # session): its registry entry is claude's own current-session truth.
        sessions_dir = _claude_home() / "sessions"
        tty = tmux.get_pane_tty(pane_id) or ""
        for process in tmux.list_tty_processes(tty):
            if not _is_claude_process(process.command, process.argv):
                continue
            payload = _read_json_file(sessions_dir / f"{process.pid}.json")
            if not payload:
                continue
            session_id = str_or_none(payload.get("sessionId"))
            if session_id:
                return session_id
        return None

    def _projects_root(self) -> Path:
        return _claude_home() / "projects"

    def find_session_file(self, session_id: str, *, cwd: str | None = None) -> Path | None:
        if not session_id:
            return None
        root = self._projects_root()
        if not root.is_dir():
            return None
        candidate = f"{session_id}.jsonl"
        if cwd:
            # Claude projects slug replaces path separators with "-".
            slug = _cwd_slug(cwd)
            direct = root / slug / candidate
            if direct.exists():
                return direct
        matches = list(root.rglob(candidate))
        return matches[0] if matches else None

    def list_sessions(
        self,
        *,
        cwd: str | None = None,
        limit: int | None = None,
    ) -> Iterable[SessionMeta]:
        root = self._projects_root()
        if not root.is_dir():
            return []
        files = sorted(root.rglob("*.jsonl"), key=safe_mtime, reverse=True)
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
        session_id: str | None = None
        cwd: str | None = None
        timestamp = None
        model: str | None = None
        try:
            with path.open() as handle:
                for _ in range(_META_SCAN_LIMIT):
                    raw = handle.readline()
                    if not raw:
                        break
                    payload = safe_json_loads(raw.strip())
                    if not payload:
                        continue
                    session_id = session_id or str_or_none(payload.get("sessionId"))
                    cwd = cwd or str_or_none(payload.get("cwd"))
                    timestamp = timestamp or parse_iso_timestamp(payload.get("timestamp"))
                    if not model:
                        msg = payload.get("message")
                        if isinstance(msg, dict):
                            model = str_or_none(msg.get("model"))
                    if session_id and cwd and model:
                        break
        except OSError:
            return None
        if not session_id:
            return None
        return SessionMeta(
            session_id=session_id,
            cli_name=self.name,
            cwd=cwd,
            title=None,
            started_at=timestamp,
            jsonl_path=path,
            model=model,
        )

    def iter_messages(self, path: Path) -> Iterator[Message]:
        try:
            handle = path.open()
        except OSError:
            return iter(())
        return _claude_message_iter(handle)

    def message_from_record(self, payload: dict[str, Any]) -> Message | None:
        record_type = payload.get("type")
        if record_type not in {"user", "assistant"}:
            return None
        msg = payload.get("message")
        if not isinstance(msg, dict):
            return None
        return Message(
            message_id=str_or_none(payload.get("uuid")),
            parent_id=str_or_none(payload.get("parentUuid")),
            role=str(msg.get("role") or record_type),
            parts=tuple(_iter_claude_parts(msg.get("content"))),
            timestamp=parse_iso_timestamp(payload.get("timestamp")),
            raw=payload,
        )


_META_SCAN_LIMIT = 20


def _claude_message_iter(handle) -> Iterator[Message]:
    with handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            payload = safe_json_loads(line)
            if not payload:
                continue
            record_type = payload.get("type")
            if record_type not in {"user", "assistant"}:
                continue
            msg = payload.get("message")
            if not isinstance(msg, dict):
                continue
            parts = tuple(_iter_claude_parts(msg.get("content")))
            yield Message(
                message_id=str_or_none(payload.get("uuid")),
                parent_id=str_or_none(payload.get("parentUuid")),
                role=str(msg.get("role") or record_type),
                parts=parts,
                timestamp=parse_iso_timestamp(payload.get("timestamp")),
                raw=payload,
            )


def _iter_claude_parts(content: Any) -> Iterator[MessagePart]:
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
                tool_name=str_or_none(block.get("name")),
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
            yield MessagePart(kind="tool_result", tool_output=output_text, raw=block)
        elif kind == "image":
            yield MessagePart(kind="image", raw=block)
        else:
            yield MessagePart(kind="unknown", raw=block)


def _read_json_file(path: Path) -> dict[str, Any] | None:
    try:
        data = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return None
    return data if isinstance(data, dict) else None


_CLAUDE_TOKENS = frozenset({"claude", "claude.exe"})


def _is_claude_process(command: str, argv: str) -> bool:
    """Match the executable itself (ps comm / argv[0]), or the script-runtime
    shape ``node <.../claude> …``.

    Later argv tokens are the process's own arguments — ``rg claude src`` is
    a search, not a claude — so they are never scanned.
    """
    if normalize_command_token(command) in _CLAUDE_TOKENS:
        return True
    parts = (argv or "").split()
    if not parts:
        return False
    if normalize_command_token(parts[0]) in _CLAUDE_TOKENS:
        return True
    return (
        len(parts) >= 2
        and normalize_command_token(parts[0]) == "node"
        and normalize_command_token(parts[1]) in _CLAUDE_TOKENS
    )


def _cwd_slug(cwd: str) -> str:
    return cwd.replace("/", "-")
