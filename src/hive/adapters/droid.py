"""Droid (Factory) session adapter.

Droid writes one JSONL per session under ``$FACTORY_HOME/sessions/<cwd-slug>/<id>.jsonl``.
The first line is a ``session_start`` record carrying the session id, title and cwd.
Subsequent lines are ``message`` records whose payload mirrors the Anthropic message
schema (``content`` is a list of blocks with ``type`` in ``text``/``tool_use``/
``tool_result``/``thinking``).
"""

from __future__ import annotations

import os
import shlex
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


class DroidAdapter:
    name = "droid"

    # --- discovery ---

    def resolve_current_session_id(self, pane_id: str) -> str | None:
        tty = tmux.get_pane_tty(pane_id) or ""
        for process in tmux.list_tty_processes(tty):
            if not _is_droid_process(process.command, process.argv):
                continue
            session_id = _extract_session_id_from_args(process.argv)
            if session_id:
                return session_id

        cwd = tmux.display_value(pane_id, "#{pane_current_path}") or ""
        if not cwd:
            return None
        session_dir = self._sessions_root() / _cwd_slug(cwd)
        if not session_dir.is_dir():
            return None
        for path in sorted(session_dir.glob("*.jsonl"), key=safe_mtime, reverse=True):
            meta = self.read_meta(path)
            if meta and meta.session_id:
                return meta.session_id
        return None

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
            cwd=str_or_none(payload.get("cwd")),
            title=str_or_none(payload.get("sessionTitle") or payload.get("title")),
            started_at=None,
            jsonl_path=path,
            model=_read_settings_model(path),
        )

    def iter_messages(self, path: Path) -> Iterator[Message]:
        try:
            handle = path.open()
        except OSError:
            return iter(())
        return _droid_message_iter(handle)

    def message_from_record(self, payload: dict[str, Any]) -> Message | None:
        if payload.get("type") != "message":
            return None
        msg = payload.get("message")
        if not isinstance(msg, dict):
            return None
        return Message(
            message_id=str_or_none(payload.get("id")),
            parent_id=str_or_none(payload.get("parentId")),
            role=str(msg.get("role") or "unknown"),
            parts=tuple(_iter_droid_parts(msg.get("content"))),
            timestamp=parse_iso_timestamp(payload.get("timestamp")),
            raw=payload,
        )


def _read_settings_model(jsonl_path: Path) -> str | None:
    """Read model from the companion ``<id>.settings.json`` next to the jsonl.

    Droid writes the active model to a sidecar JSON file rather than into the
    transcript itself, so the model isn't visible from the jsonl alone.
    """
    settings_path = jsonl_path.with_suffix(".settings.json")
    try:
        text = settings_path.read_text()
    except OSError:
        return None
    payload = safe_json_loads(text)
    if not isinstance(payload, dict):
        return None
    return str_or_none(payload.get("model"))


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
                message_id=str_or_none(payload.get("id")),
                parent_id=str_or_none(payload.get("parentId")),
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
            yield MessagePart(
                kind="tool_result",
                tool_output=output_text,
                raw=block,
            )
        elif kind == "image":
            yield MessagePart(kind="image", raw=block)
        else:
            yield MessagePart(kind="unknown", raw=block)


def _extract_session_id_from_args(argv: str) -> str | None:
    try:
        tokens = shlex.split(argv or "")
    except ValueError:
        tokens = (argv or "").split()
    for index, token in enumerate(tokens):
        if token.startswith("--resume=") or token.startswith("--fork="):
            _, _, value = token.partition("=")
            return value or None
        if token in {"--resume", "--fork"} and index + 1 < len(tokens):
            return tokens[index + 1] or None
    return None


def _is_droid_process(command: str, argv: str) -> bool:
    if normalize_command_token(command) == "droid":
        return True
    try:
        tokens = shlex.split(argv or "")
    except ValueError:
        tokens = (argv or "").split()
    for token in tokens:
        if normalize_command_token(token) == "droid":
            return True
    return False


def _cwd_slug(cwd: str) -> str:
    return cwd.replace("/", "-")
