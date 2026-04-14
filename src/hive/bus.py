"""Workspace-backed agent collaboration primitives."""

from __future__ import annotations

from datetime import UTC, datetime
import json
from pathlib import Path
import shutil
import time


WORKSPACE_DIRS = (
    "events",
    "artifacts",
    "state",
    "cursors",
)
LEGACY_WORKSPACE_DIRS = ("status", "presence")


def _now_iso() -> str:
    return datetime.now(UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def init_workspace(workspace: str | Path) -> Path:
    ws = Path(workspace).expanduser()
    for name in WORKSPACE_DIRS:
        (ws / name).mkdir(parents=True, exist_ok=True)
    return ws


def reset_workspace(workspace: str | Path) -> Path:
    ws = Path(workspace).expanduser()
    ws.mkdir(parents=True, exist_ok=True)
    for name in (*WORKSPACE_DIRS, *LEGACY_WORKSPACE_DIRS):
        root = ws / name
        if root.exists():
            shutil.rmtree(root)
        if name in WORKSPACE_DIRS:
            root.mkdir(parents=True, exist_ok=True)
    return ws


def parse_key_value(entries: tuple[str, ...] | list[str]) -> dict[str, str]:
    data: dict[str, str] = {}
    for entry in entries:
        if "=" not in entry:
            raise ValueError(f"invalid KEY=VALUE entry '{entry}'")
        key, value = entry.split("=", 1)
        key = key.strip()
        if not key:
            raise ValueError(f"invalid KEY=VALUE entry '{entry}', empty key")
        data[key] = value
    return data


def write_event(
    workspace: str | Path,
    *,
    from_agent: str,
    to_agent: str,
    intent: str,
    body: str = "",
    artifact: str = "",
    metadata: dict[str, str] | None = None,
    message_id: str = "",
    reply_to: str = "",
) -> Path:
    path = Path(workspace).expanduser() / "events" / f"{time.time_ns()}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "from": from_agent,
        "to": to_agent,
        "intent": intent,
        "metadata": metadata or {},
        "createdAt": _now_iso(),
    }
    if message_id:
        payload["id"] = message_id
    if reply_to:
        payload["inReplyTo"] = reply_to
    normalized_body = body.strip()
    if normalized_body:
        payload["body"] = normalized_body
    if artifact:
        payload["artifact"] = artifact
    path.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n")
    return path


def read_all_events(workspace: str | Path) -> list[dict[str, object]]:
    root = Path(workspace).expanduser() / "events"
    if not root.is_dir():
        return []
    rows: list[dict[str, object]] = []
    for path in sorted(root.glob("*.json")):
        rows.append(json.loads(path.read_text()))
    return rows


def read_events_with_ns(workspace: str | Path) -> list[tuple[int, dict[str, object]]]:
    """Return sorted list of (ns_timestamp, event_data) tuples."""
    root = Path(workspace).expanduser() / "events"
    if not root.is_dir():
        return []
    rows: list[tuple[int, dict[str, object]]] = []
    for path in sorted(root.glob("*.json")):
        try:
            ns = int(path.stem)
        except ValueError:
            continue
        rows.append((ns, json.loads(path.read_text())))
    return rows


def get_latest_event_ns(workspace: str | Path) -> int:
    """Return the ns timestamp of the latest event, or 0 if no events."""
    root = Path(workspace).expanduser() / "events"
    if not root.is_dir():
        return 0
    files = sorted(root.glob("*.json"))
    if not files:
        return 0
    try:
        return int(files[-1].stem)
    except ValueError:
        return 0


def read_cursor(workspace: str | Path, agent_name: str) -> int:
    """Read the cursor value for an agent. Returns 0 if no cursor."""
    path = Path(workspace).expanduser() / "cursors" / agent_name
    try:
        return int(path.read_text().strip())
    except (OSError, ValueError):
        return 0


def write_cursor(workspace: str | Path, agent_name: str, ns_value: int) -> None:
    """Write the cursor value for an agent."""
    path = Path(workspace).expanduser() / "cursors" / agent_name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(str(ns_value) + "\n")


