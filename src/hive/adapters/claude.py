"""Claude Code session adapter.

Claude stores session history under ``$CLAUDE_HOME/projects/<cwd-slug>/<id>.jsonl``.
Every record carries ``sessionId``, ``cwd``, ``parentUuid``, ``timestamp`` and
``gitBranch``; the ``message.content`` field is an Anthropic-style list of blocks
or (rarely) a plain string.

Claude's ``$CLAUDE_HOME/sessions/<pid>.json`` PID map can become stale after
``/clear``. Current-session resolution therefore uses PID-anchored open jsonl
file handles and returns unresolved when no live handle is observed.
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
    return Path(os.environ.get("CLAUDE_HOME", str(Path.home() / ".claude")))


class ClaudeAdapter:
    name = "claude"

    # --- discovery ---

    def resolve_current_session_id(self, pane_id: str) -> str | None:
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
# Calibration from the cvim /clear regression data:
# - normal Claude idle pidfile/transcript gaps cluster around [-2m, 0s]
# - stale pidfiles after /clear were 19h+ apart from their transcripts
# The 15-minute bound sits in that bimodal gap. This is a workaround for
# Claude binary heartbeat drift, not current-session authority.
PIDFILE_TRANSCRIPT_STALE_AFTER_SECONDS = 15 * 60


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


def resolve_session_id_from_pidfile(pid: str | int, *, cwd: str | None = None) -> str | None:
    payload = _read_json_file(_claude_home() / "sessions" / f"{pid}.json")
    if not payload:
        return None
    session_id = str_or_none(payload.get("sessionId"))
    if not session_id:
        return None
    updated_at = _timestamp_seconds(payload.get("updatedAt"))
    if updated_at is None:
        return None
    transcript = ClaudeAdapter().find_session_file(
        session_id,
        cwd=cwd or str_or_none(payload.get("cwd")),
    )
    if not transcript:
        return None
    try:
        transcript_mtime = transcript.stat().st_mtime
    except OSError:
        return None
    # Claude can advance sessions/<pid>.json heartbeats while an idle
    # transcript is not changing, so small positive drift is normal. Large
    # drift is treated only as a bounded seed rejection, not as current-state
    # authority.
    if updated_at - transcript_mtime > PIDFILE_TRANSCRIPT_STALE_AFTER_SECONDS:
        return None
    return session_id


def _timestamp_seconds(value: Any) -> float | None:
    if isinstance(value, (int, float)):
        timestamp = float(value)
        return timestamp / 1000.0 if timestamp > 100_000_000_000 else timestamp
    if isinstance(value, str):
        stripped = value.strip()
        if not stripped:
            return None
        try:
            return _timestamp_seconds(float(stripped))
        except ValueError:
            parsed = parse_iso_timestamp(stripped)
            return parsed.timestamp() if parsed is not None else None
    return None


def _read_json_file(path: Path) -> dict[str, Any] | None:
    try:
        data = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return None
    return data if isinstance(data, dict) else None


def _is_claude_process(command: str, argv: str) -> bool:
    for token in (command, *(argv or "").split()):
        if normalize_command_token(token) in {"claude", "claude.exe"}:
            return True
    return False


def _cwd_slug(cwd: str) -> str:
    return cwd.replace("/", "-")
