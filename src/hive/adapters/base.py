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


# --- ACK helpers ---
# These operate on raw JSONL lines to detect whether a sent message was
# accepted by the receiver's CLI session transcript.  The _is_user_turn
# matcher knows the raw record shapes of all three supported CLIs
# (droid, claude, codex) so the wait helper can stay CLI-agnostic.


def get_transcript_baseline(path: Path) -> int:
    """Return current file size in bytes, or 0 if the file does not exist."""
    try:
        return path.stat().st_size
    except OSError:
        return 0


def _is_user_turn(payload: dict[str, Any]) -> bool:
    """Check whether a raw JSONL record represents a user turn.

    Checks all three CLI formats; only one will match for any given file.
    """
    record_type = payload.get("type", "")
    # droid: {"type": "message", "message": {"role": "user", ...}}
    if record_type == "message":
        msg = payload.get("message")
        return isinstance(msg, dict) and msg.get("role") == "user"
    # claude: {"type": "user", ...}
    if record_type == "user":
        return True
    # codex: {"type": "response_item", "payload": {"type": "message", "role": "user", ...}}
    if record_type == "response_item":
        inner = payload.get("payload")
        return isinstance(inner, dict) and inner.get("type") == "message" and inner.get("role") == "user"
    return False


def _poll_interval(elapsed: float) -> float:
    if elapsed < 5.0:
        return 0.2
    if elapsed < 15.0:
        return 0.5
    return 1.0


def wait_for_id_in_transcript(
    path: Path,
    message_id: str,
    baseline: int,
    timeout: float = 45.0,
) -> bool:
    """Block until *message_id* appears in a new user turn after *baseline* bytes.

    Returns True if confirmed, False on timeout.
    """
    import time

    deadline = time.monotonic() + timeout
    handle = None
    remainder = ""

    while time.monotonic() < deadline:
        # (Re)open file if needed — it may not exist yet at baseline time.
        if handle is None:
            try:
                handle = path.open("r")
                handle.seek(baseline)
            except OSError:
                time.sleep(_poll_interval(time.monotonic() - (deadline - timeout)))
                continue

        chunk = handle.read()
        if chunk:
            data = remainder + chunk
            lines = data.split("\n")
            # Last element is either "" (if data ended with \n) or a partial line.
            remainder = lines.pop()
            for line in lines:
                if not line:
                    continue
                if message_id not in line:
                    continue
                parsed = safe_json_loads(line)
                if parsed is not None and _is_user_turn(parsed):
                    handle.close()
                    return True
        else:
            elapsed = time.monotonic() - (deadline - timeout)
            time.sleep(_poll_interval(elapsed))

    if handle is not None:
        handle.close()
    return False
