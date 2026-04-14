"""Workspace-backed agent collaboration primitives."""

from __future__ import annotations

from datetime import UTC, datetime
import json
from pathlib import Path
import shutil
import sqlite3


WORKSPACE_DIRS = (
    "artifacts",
    "state",
    "run",
)
LEGACY_WORKSPACE_DIRS = ("status", "presence", "events", "cursors")
DB_FILENAME = "hive.db"


def _now_iso() -> str:
    return datetime.now(UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def _db_path(workspace: str | Path) -> Path:
    return Path(workspace).expanduser() / DB_FILENAME


def _connect(workspace: str | Path) -> sqlite3.Connection:
    ws = Path(workspace).expanduser()
    ws.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(_db_path(ws), timeout=30)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=NORMAL")
    _init_schema(conn)
    return conn


def _init_schema(conn: sqlite3.Connection) -> None:
    conn.executescript(
        """
        CREATE TABLE IF NOT EXISTS messages (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            msg_id TEXT NOT NULL DEFAULT '',
            from_agent TEXT NOT NULL,
            to_agent TEXT NOT NULL,
            intent TEXT NOT NULL,
            body TEXT NOT NULL DEFAULT '',
            artifact TEXT NOT NULL DEFAULT '',
            in_reply_to TEXT NOT NULL DEFAULT '',
            metadata_json TEXT NOT NULL DEFAULT '{}',
            created_at TEXT NOT NULL,
            inject_status TEXT NOT NULL DEFAULT '',
            turn_observed TEXT NOT NULL DEFAULT '',
            runtime_queue_state TEXT NOT NULL DEFAULT '',
            queue_source TEXT NOT NULL DEFAULT ''
        );

        CREATE INDEX IF NOT EXISTS idx_messages_msg_intent_seq
            ON messages(msg_id, intent, seq);

        CREATE TABLE IF NOT EXISTS cursors (
            agent_name TEXT PRIMARY KEY,
            last_seen_seq INTEGER NOT NULL
        );
        """
    )
    conn.commit()


def _row_to_event(row: sqlite3.Row) -> dict[str, object]:
    metadata_raw = row["metadata_json"] or "{}"
    try:
        metadata = json.loads(metadata_raw)
    except json.JSONDecodeError:
        metadata = {}
    if not isinstance(metadata, dict):
        metadata = {}

    event: dict[str, object] = {
        "from": row["from_agent"],
        "to": row["to_agent"],
        "intent": row["intent"],
        "metadata": metadata,
        "createdAt": row["created_at"],
    }
    if row["msg_id"]:
        event["msgId"] = row["msg_id"]
    if row["in_reply_to"]:
        event["inReplyTo"] = row["in_reply_to"]
    if row["body"]:
        event["body"] = row["body"]
    if row["artifact"]:
        event["artifact"] = row["artifact"]
    if row["inject_status"]:
        event["injectStatus"] = row["inject_status"]
    if row["turn_observed"]:
        event["turnObserved"] = row["turn_observed"]
    if row["runtime_queue_state"]:
        event["runtimeQueueState"] = row["runtime_queue_state"]
    if row["queue_source"]:
        event["queueSource"] = row["queue_source"]
    return event


def init_workspace(workspace: str | Path) -> Path:
    ws = Path(workspace).expanduser()
    for name in WORKSPACE_DIRS:
        (ws / name).mkdir(parents=True, exist_ok=True)
    conn = _connect(ws)
    conn.close()
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
    db_path = _db_path(ws)
    if db_path.exists():
        db_path.unlink()
    conn = _connect(ws)
    conn.close()
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
) -> int:
    normalized_body = body.strip()
    metadata_json = json.dumps(metadata or {}, ensure_ascii=False)
    created_at = _now_iso()
    with _connect(workspace) as conn:
        cur = conn.execute(
            """
            INSERT INTO messages (
                msg_id, from_agent, to_agent, intent, body, artifact,
                in_reply_to, metadata_json, created_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                message_id,
                from_agent,
                to_agent,
                intent,
                normalized_body,
                artifact,
                reply_to,
                metadata_json,
                created_at,
            ),
        )
        conn.commit()
        return int(cur.lastrowid)


def patch_event(workspace: str | Path, event_seq: int, **fields: object) -> None:
    if not event_seq:
        return
    column_map = {
        "injectStatus": "inject_status",
        "turnObserved": "turn_observed",
        "runtimeQueueState": "runtime_queue_state",
        "queueSource": "queue_source",
    }
    assignments: list[str] = []
    values: list[object] = []
    for key, column in column_map.items():
        value = fields.get(key)
        if value is None:
            continue
        assignments.append(f"{column} = ?")
        values.append(value)
    if not assignments:
        return
    values.append(event_seq)
    with _connect(workspace) as conn:
        conn.execute(
            f"UPDATE messages SET {', '.join(assignments)} WHERE seq = ?",
            values,
        )
        conn.commit()


def read_all_events(workspace: str | Path) -> list[dict[str, object]]:
    with _connect(workspace) as conn:
        rows = conn.execute("SELECT * FROM messages ORDER BY seq ASC").fetchall()
    return [_row_to_event(row) for row in rows]


def read_events_with_ns(workspace: str | Path) -> list[tuple[int, dict[str, object]]]:
    """Return sorted list of (monotonic sequence, event_data) tuples."""
    with _connect(workspace) as conn:
        rows = conn.execute("SELECT * FROM messages ORDER BY seq ASC").fetchall()
    return [(int(row["seq"]), _row_to_event(row)) for row in rows]


def get_latest_event_ns(workspace: str | Path) -> int:
    """Return the latest durable sequence number, or 0 if no events."""
    with _connect(workspace) as conn:
        row = conn.execute("SELECT COALESCE(MAX(seq), 0) AS seq FROM messages").fetchone()
    return int(row["seq"]) if row is not None else 0


def count_events(workspace: str | Path) -> int:
    with _connect(workspace) as conn:
        row = conn.execute("SELECT COUNT(*) AS count FROM messages").fetchone()
    return int(row["count"]) if row is not None else 0


def find_send_event(workspace: str | Path, message_id: str) -> dict[str, object] | None:
    with _connect(workspace) as conn:
        row = conn.execute(
            """
            SELECT * FROM messages
            WHERE msg_id = ? AND intent = 'send'
            ORDER BY seq ASC
            LIMIT 1
            """,
            (message_id,),
        ).fetchone()
    return _row_to_event(row) if row is not None else None


def find_latest_observation(workspace: str | Path, message_id: str) -> dict[str, object] | None:
    with _connect(workspace) as conn:
        row = conn.execute(
            """
            SELECT * FROM messages
            WHERE intent = 'observation' AND msg_id = ?
            ORDER BY seq DESC
            LIMIT 1
            """,
            (message_id,),
        ).fetchone()
    return _row_to_event(row) if row is not None else None


def read_cursor(workspace: str | Path, agent_name: str) -> int:
    with _connect(workspace) as conn:
        row = conn.execute(
            "SELECT last_seen_seq FROM cursors WHERE agent_name = ?",
            (agent_name,),
        ).fetchone()
    return int(row["last_seen_seq"]) if row is not None else 0


def write_cursor(workspace: str | Path, agent_name: str, ns_value: int) -> None:
    with _connect(workspace) as conn:
        conn.execute(
            """
            INSERT INTO cursors(agent_name, last_seen_seq)
            VALUES (?, ?)
            ON CONFLICT(agent_name) DO UPDATE SET last_seen_seq = excluded.last_seen_seq
            """,
            (agent_name, ns_value),
        )
        conn.commit()
