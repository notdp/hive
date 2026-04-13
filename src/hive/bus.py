"""Workspace-backed agent collaboration primitives."""

from __future__ import annotations

from datetime import UTC, datetime
import json
from pathlib import Path
import shutil
import time


WORKSPACE_DIRS = (
    "presence",
    "events",
    "artifacts",
    "state",
)
LEGACY_WORKSPACE_DIRS = ("status",)


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
    state: str = "",
    task: str = "",
    waiting_on: str = "",
    waiting_for: str = "",
    blocked_by: str = "",
    metadata: dict[str, str] | None = None,
    message_id: str = "",
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
    normalized_body = body.strip()
    if normalized_body:
        payload["body"] = normalized_body
    if artifact:
        payload["artifact"] = artifact
    if state:
        payload["state"] = state
    if task:
        payload["task"] = task
    if waiting_on:
        payload["waitingOn"] = waiting_on
    if waiting_for:
        payload["waitingFor"] = waiting_for
    if blocked_by:
        payload["blockedBy"] = blocked_by
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


def _event_summary(event: dict[str, object]) -> str:
    body = str(event.get("body", "")).strip()
    return body


def _project_event_status(event: dict[str, object]) -> tuple[str, dict[str, object]] | None:
    intent = str(event.get("intent", "")).strip()
    if intent not in {"send", "ask", "reply"}:
        return None

    metadata = {str(k): str(v) for k, v in dict(event.get("metadata", {})).items()}
    created_at = str(event.get("createdAt", ""))
    summary = _event_summary(event)
    artifact = str(event.get("artifact", "")).strip()

    if intent == "reply":
        agent_name = str(event.get("from", "")).strip()
        state = str(event.get("state", "")).strip() or "done"
        if not agent_name:
            return None
        payload: dict[str, object] = {
            "agent": agent_name,
            "state": state,
            "metadata": metadata,
            "updatedAt": created_at,
        }
        if summary:
            payload["summary"] = summary
        if artifact:
            payload["artifact"] = artifact
        for event_key, payload_key in (
            ("task", "task"),
            ("waitingOn", "waitingOn"),
            ("waitingFor", "waitingFor"),
            ("blockedBy", "blockedBy"),
        ):
            value = str(event.get(event_key, "")).strip()
            if value:
                payload[payload_key] = value
        return agent_name, payload

    agent_name = str(event.get("to", "")).strip()
    if not agent_name:
        return None
    payload = {
        "agent": agent_name,
        "state": "busy",
        "metadata": metadata,
        "updatedAt": created_at,
    }
    if summary:
        payload["summary"] = summary
    if artifact:
        payload["artifact"] = artifact
    return agent_name, payload


def read_status(workspace: str | Path, agent_name: str) -> dict[str, object] | None:
    return read_all_statuses(workspace).get(agent_name)


def read_all_statuses(workspace: str | Path) -> dict[str, dict[str, object]]:
    rows: dict[str, dict[str, object]] = {}
    for event in read_all_events(workspace):
        projected = _project_event_status(event)
        if projected is None:
            continue
        agent_name, payload = projected
        rows[agent_name] = payload
    return {name: rows[name] for name in sorted(rows)}


def write_presence_snapshot(workspace: str | Path, team_status: dict[str, object]) -> None:
    root = Path(workspace).expanduser() / "presence"
    root.mkdir(parents=True, exist_ok=True)

    team_snapshot = {
        "updatedAt": _now_iso(),
        "team": team_status.get("name"),
        "description": team_status.get("description"),
        "workspace": team_status.get("workspace"),
        "tmuxSession": team_status.get("tmuxSession", ""),
        "tmuxWindow": team_status.get("tmuxWindow", ""),
        "members": team_status.get("members", []),
    }
    (root / "team.json").write_text(json.dumps(team_snapshot, indent=2, ensure_ascii=False) + "\n")

    for member in list(team_status.get("members", [])):
        member_name = str(member.get("name", ""))
        if not member_name:
            continue
        payload = {
            "updatedAt": _now_iso(),
            "agent": member_name,
            **dict(member),
        }
        (root / f"{member_name}.json").write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n")
