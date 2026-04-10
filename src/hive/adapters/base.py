"""Base types and Protocol for agent CLI session adapters.

Adapters normalize the three CLIs (droid/claude/codex) around a single
interface so callers can discover, locate, and read session JSONL files
without knowing the per-CLI on-disk layout.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any, Iterable, Iterator, Protocol, runtime_checkable


@dataclass(frozen=True)
class SessionMeta:
    session_id: str
    cli_name: str
    cwd: str | None
    title: str | None
    started_at: datetime | None
    jsonl_path: Path


@dataclass(frozen=True)
class MessagePart:
    kind: str  # "text" | "tool_use" | "tool_result" | "thinking" | "image" | "unknown"
    text: str | None = None
    tool_name: str | None = None
    tool_input: dict[str, Any] | None = None
    tool_output: str | None = None
    raw: dict[str, Any] | None = None


@dataclass(frozen=True)
class Message:
    message_id: str | None
    parent_id: str | None
    role: str  # "user" | "assistant" | "system" | "developer" | "tool"
    parts: tuple[MessagePart, ...]
    timestamp: datetime | None
    raw: dict[str, Any] = field(default_factory=dict)


@runtime_checkable
class SessionAdapter(Protocol):
    name: str

    def resolve_current_session_id(self, pane_id: str) -> str | None:
        """Return the id of the session currently running in *pane_id*."""

    def find_session_file(self, session_id: str, *, cwd: str | None = None) -> Path | None:
        """Locate the JSONL file backing *session_id*.

        *cwd* is an optional hint; droid/claude store files under a cwd-slug
        directory while codex partitions by date, so the hint speeds up the
        former and is ignored by the latter.
        """

    def list_sessions(
        self,
        *,
        cwd: str | None = None,
        limit: int | None = None,
    ) -> Iterable[SessionMeta]:
        """Enumerate known sessions, optionally filtered by *cwd*."""

    def read_meta(self, path: Path) -> SessionMeta | None:
        """Parse the meta header of a JSONL session file."""

    def iter_messages(self, path: Path) -> Iterator[Message]:
        """Yield normalized :class:`Message` records from a JSONL session file."""


def parse_iso_timestamp(value: Any) -> datetime | None:
    if not isinstance(value, str) or not value:
        return None
    raw = value.replace("Z", "+00:00") if value.endswith("Z") else value
    try:
        return datetime.fromisoformat(raw)
    except ValueError:
        return None


def safe_json_loads(line: str) -> dict[str, Any] | None:
    import json

    try:
        payload = json.loads(line)
    except json.JSONDecodeError:
        return None
    return payload if isinstance(payload, dict) else None
