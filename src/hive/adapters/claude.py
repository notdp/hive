"""Claude Code session adapter.

Claude stores session history under ``$CLAUDE_HOME/projects/<cwd-slug>/<id>.jsonl``.
Every record carries ``sessionId``, ``cwd``, ``parentUuid``, ``timestamp`` and
``gitBranch``; the ``message.content`` field is an Anthropic-style list of blocks
or (rarely) a plain string.

The ``$CLAUDE_HOME/sessions/<pid>.json`` files only map running processes to the
session they own; we keep using them to resolve the *current* session id of a
pane but they are not the source of truth for history.
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
    parse_iso_timestamp,
    safe_json_loads,
)


def _claude_home() -> Path:
    return Path(os.environ.get("CLAUDE_HOME", str(Path.home() / ".claude")))


class ClaudeAdapter:
    name = "claude"

    # --- discovery ---

    def resolve_current_session_id(self, pane_id: str) -> str | None:
        sessions_dir = _claude_home() / "sessions"
        tty = tmux.get_pane_tty(pane_id) or ""
        cwd = tmux.display_value(pane_id, "#{pane_current_path}") or None
        for process in tmux.list_tty_processes(tty):
            if not _is_claude_process(process.command, process.argv):
                continue
            payload = _read_json_file(sessions_dir / f"{process.pid}.json")
            if not payload:
                continue
            session_id = _str_or_none(payload.get("sessionId"))
            if session_id:
                newer_session_id = self._resolve_newer_project_session_id(
                    session_id,
                    cwd=cwd,
                    pane_id=pane_id,
                )
                if newer_session_id:
                    return newer_session_id
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
                    session_id = session_id or _str_or_none(payload.get("sessionId"))
                    cwd = cwd or _str_or_none(payload.get("cwd"))
                    timestamp = timestamp or parse_iso_timestamp(payload.get("timestamp"))
                    if not model:
                        msg = payload.get("message")
                        if isinstance(msg, dict):
                            model = _str_or_none(msg.get("model"))
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

    def _resolve_newer_project_session_id(
        self,
        session_id: str,
        *,
        cwd: str | None = None,
        pane_id: str = "",
    ) -> str | None:
        current_path = self.find_session_file(session_id, cwd=cwd)
        if current_path is None:
            return None

        current_mtime_ns = _safe_mtime_ns(current_path)
        if current_mtime_ns < 0:
            return None

        project_dir = current_path.parent
        try:
            candidates = sorted(project_dir.glob("*.jsonl"), key=_safe_mtime_ns, reverse=True)
        except OSError:
            return None

        for candidate in candidates:
            if candidate == current_path:
                continue
            if _safe_mtime_ns(candidate) <= current_mtime_ns:
                break
            meta = self.read_meta(candidate)
            if meta and meta.session_id:
                if pane_id and self._session_claimed_by_other_window_pane(
                    pane_id,
                    meta.session_id,
                ):
                    return None
                return meta.session_id
        return None

    def _session_claimed_by_other_window_pane(self, pane_id: str, session_id: str) -> bool:
        window_target = tmux.get_pane_window_target(pane_id) or ""
        if not window_target:
            return False
        for pane in tmux.list_panes_full(window_target):
            if pane.pane_id == pane_id:
                continue
            other_session_id = self._read_pid_mapped_session_id(pane.pane_id)
            if other_session_id == session_id:
                return True
        return False

    def _read_pid_mapped_session_id(self, pane_id: str) -> str | None:
        sessions_dir = _claude_home() / "sessions"
        tty = tmux.get_pane_tty(pane_id) or ""
        for process in tmux.list_tty_processes(tty):
            if not _is_claude_process(process.command, process.argv):
                continue
            payload = _read_json_file(sessions_dir / f"{process.pid}.json")
            if not payload:
                continue
            session_id = _str_or_none(payload.get("sessionId"))
            if session_id:
                return session_id
        return None


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
                message_id=_str_or_none(payload.get("uuid")),
                parent_id=_str_or_none(payload.get("parentUuid")),
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


def _normalize(value: str) -> str:
    value = (value or "").strip().lower().rsplit("/", 1)[-1]
    return value.lstrip("-")


def _is_claude_process(command: str, argv: str) -> bool:
    if _normalize(command) == "claude":
        return True
    for token in (argv or "").split():
        if _normalize(token) == "claude":
            return True
    return False


def _cwd_slug(cwd: str) -> str:
    return cwd.replace("/", "-")


def _safe_mtime(path: Path) -> float:
    try:
        return path.stat().st_mtime
    except OSError:
        return -1


def _safe_mtime_ns(path: Path) -> int:
    try:
        return path.stat().st_mtime_ns
    except OSError:
        return -1


def _str_or_none(value: Any) -> str | None:
    if value is None:
        return None
    text = str(value)
    return text or None
