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


# --- Send gate helpers ---
# Detect whether the target agent is waiting for a user answer
# (AskUserQuestion) before allowing message injection.

_ASK_TOOL_NAMES = frozenset({"AskUserQuestion", "request_user_input"})

_MAX_TAIL_BYTES = 128 * 1024  # 128KB upper bound for tail reads


@dataclass(frozen=True)
class GateResult:
    status: str   # "waiting" | "clear" | "unknown"
    reason: str = ""


def _extract_content_blocks(payload: dict[str, Any]) -> list[dict[str, Any]]:
    """Extract content blocks from a JSONL record, handling droid/claude/codex."""
    msg = payload.get("message")
    if isinstance(msg, dict):
        content = msg.get("content")
        if isinstance(content, list):
            return content
    return []


def _is_assistant_ask(payload: dict[str, Any]) -> bool:
    """Check whether a raw JSONL record is an assistant turn with AskUserQuestion.

    Handles all three CLI formats:
    - droid: {"type": "message", "message": {"role": "assistant", "content": [...]}}
    - claude: {"type": "assistant", "message": {"role": "assistant", "content": [...]}}
    - codex: {"type": "response_item", "payload": {"type": "function_call", "name": ...}}
    """
    record_type = payload.get("type", "")

    # droid: type == "message", message.role == "assistant"
    if record_type == "message":
        msg = payload.get("message")
        if isinstance(msg, dict) and msg.get("role") == "assistant":
            for block in _extract_content_blocks(payload):
                if block.get("type") == "tool_use" and block.get("name") in _ASK_TOOL_NAMES:
                    return True
        return False

    # claude: type == "assistant"
    if record_type == "assistant":
        for block in _extract_content_blocks(payload):
            if block.get("type") == "tool_use" and block.get("name") in _ASK_TOOL_NAMES:
                return True
        return False

    # codex: type == "response_item", payload.type == "function_call"
    if record_type == "response_item":
        inner = payload.get("payload")
        if isinstance(inner, dict) and inner.get("type") == "function_call":
            if inner.get("name") in _ASK_TOOL_NAMES:
                return True
        return False

    return False


def _is_function_call_output(payload: dict[str, Any]) -> bool:
    """Check whether a raw JSONL record is a function_call_output (codex tool result)."""
    if payload.get("type") == "response_item":
        inner = payload.get("payload")
        if isinstance(inner, dict) and inner.get("type") == "function_call_output":
            return True
    return False


def check_input_gate(path: Path) -> GateResult:
    """Check if the agent owning *path* is waiting for a user answer.

    Reads the tail of the JSONL file, expanding the window if no relevant
    record is found (8KB → 16KB → ... → 128KB).

    Returns GateResult with status:
      - "waiting": last relevant record is an unanswered AskUserQuestion
      - "clear": last relevant record is a user turn (question answered)
      - "unknown": could not determine (file missing, empty, parse issues)
    """
    try:
        file_size = path.stat().st_size
    except OSError as e:
        return GateResult("unknown", f"cannot stat file: {e}")
    if file_size == 0:
        return GateResult("unknown", "empty transcript")

    chunk = 8192
    while chunk <= _MAX_TAIL_BYTES:
        offset = max(0, file_size - chunk)
        try:
            with path.open("rb") as f:
                f.seek(offset)
                raw = f.read()
            data = raw.decode("utf-8", errors="replace")
        except OSError as e:
            return GateResult("unknown", f"read error: {e}")

        lines = data.split("\n")
        # First line may be partial if we seeked mid-line; skip it unless offset == 0
        if offset > 0:
            lines = lines[1:]

        # Parse all lines, collect relevant records
        records: list[dict[str, Any]] = []
        for line in lines:
            line = line.strip()
            if not line:
                continue
            parsed = safe_json_loads(line)
            if parsed is not None:
                records.append(parsed)

        # Scan in reverse for the last relevant record
        for record in reversed(records):
            if _is_user_turn(record) or _is_function_call_output(record):
                return GateResult("clear", "last record is user response")
            if _is_assistant_ask(record):
                return GateResult("waiting", "AskUserQuestion pending")

        # No relevant record found — expand window if possible
        if offset == 0:
            break  # Already read the entire file
        chunk *= 2

    return GateResult("unknown", "no relevant record found")


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
