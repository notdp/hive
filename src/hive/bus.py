"""Workspace-backed agent collaboration primitives."""

from __future__ import annotations

from dataclasses import dataclass
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
_LEGACY_MESSAGE_RUNTIME_COLUMNS = (
    "inject_status",
    "turn_observed",
    "runtime_queue_state",
    "queue_source",
)
_MSG_ID_ALPHABET = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"
_MSG_ID_WIDTH = 4
_MSG_ID_SPACE = len(_MSG_ID_ALPHABET) ** _MSG_ID_WIDTH
# Keep short IDs non-obvious without introducing collisions inside the 4-char space.
_MSG_ID_MULTIPLIER = 131071
_MSG_ID_OFFSET = 8191


def _now_iso() -> str:
    return datetime.now(UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def _db_path(workspace: str | Path) -> Path:
    return Path(workspace).expanduser() / DB_FILENAME


def _encode_base62(value: int) -> str:
    if value < 0:
        raise ValueError("value must be non-negative")
    if value == 0:
        return _MSG_ID_ALPHABET[0]
    base = len(_MSG_ID_ALPHABET)
    encoded: list[str] = []
    current = value
    while current > 0:
        current, digit = divmod(current, base)
        encoded.append(_MSG_ID_ALPHABET[digit])
    return "".join(reversed(encoded))


def format_msg_id(event_seq: int) -> str:
    """Derive a short deterministic msgId from the durable row sequence."""
    if event_seq <= 0:
        raise ValueError("event_seq must be positive")
    if event_seq < _MSG_ID_SPACE:
        mixed = (event_seq * _MSG_ID_MULTIPLIER + _MSG_ID_OFFSET) % _MSG_ID_SPACE
        return _encode_base62(mixed).rjust(_MSG_ID_WIDTH, _MSG_ID_ALPHABET[0])
    return _encode_base62(event_seq)


def _connect(workspace: str | Path) -> sqlite3.Connection:
    ws = Path(workspace).expanduser()
    ws.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(_db_path(ws), timeout=30)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=NORMAL")
    _init_schema(conn)
    return conn


@dataclass(frozen=True)
class EventWriteResult:
    seq: int
    msg_id: str = ""


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
            created_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_messages_msg_intent_seq
            ON messages(msg_id, intent, seq);
        """
    )
    _migrate_messages_table(conn)
    conn.commit()


def _table_columns(conn: sqlite3.Connection, table_name: str) -> tuple[str, ...]:
    rows = conn.execute(f"PRAGMA table_info({table_name})").fetchall()
    return tuple(str(row["name"]) for row in rows)


def _migrate_messages_table(conn: sqlite3.Connection) -> None:
    columns = _table_columns(conn, "messages")
    if not columns:
        return
    if not any(column in columns for column in _LEGACY_MESSAGE_RUNTIME_COLUMNS):
        return

    conn.execute("BEGIN IMMEDIATE")
    conn.execute("DROP INDEX IF EXISTS idx_messages_msg_intent_seq")
    conn.execute("ALTER TABLE messages RENAME TO messages_legacy")
    conn.execute(
        """
        CREATE TABLE messages (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            msg_id TEXT NOT NULL DEFAULT '',
            from_agent TEXT NOT NULL,
            to_agent TEXT NOT NULL,
            intent TEXT NOT NULL,
            body TEXT NOT NULL DEFAULT '',
            artifact TEXT NOT NULL DEFAULT '',
            in_reply_to TEXT NOT NULL DEFAULT '',
            metadata_json TEXT NOT NULL DEFAULT '{}',
            created_at TEXT NOT NULL
        )
        """
    )
    conn.execute(
        """
        INSERT INTO messages (
            seq, msg_id, from_agent, to_agent, intent, body, artifact,
            in_reply_to, metadata_json, created_at
        )
        SELECT
            seq, msg_id, from_agent, to_agent, intent, body, artifact,
            in_reply_to, metadata_json, created_at
        FROM messages_legacy
        ORDER BY seq ASC
        """
    )
    conn.execute("DROP TABLE messages_legacy")
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_messages_msg_intent_seq ON messages(msg_id, intent, seq)"
    )


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
    for path in (
        db_path,
        Path(str(db_path) + "-wal"),
        Path(str(db_path) + "-shm"),
    ):
        if path.exists():
            path.unlink()
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


def write_send_event(
    workspace: str | Path,
    *,
    from_agent: str,
    to_agent: str,
    body: str = "",
    artifact: str = "",
    metadata: dict[str, str] | None = None,
    reply_to: str = "",
) -> EventWriteResult:
    """Write a send event with its deterministic msgId in one transaction."""
    normalized_body = body.strip()
    metadata_json = json.dumps(metadata or {}, ensure_ascii=False)
    created_at = _now_iso()
    with _connect(workspace) as conn:
        conn.execute("BEGIN IMMEDIATE")
        row = conn.execute("SELECT COALESCE(MAX(seq), 0) + 1 AS seq FROM messages").fetchone()
        assert row is not None
        event_seq = int(row["seq"])
        msg_id = format_msg_id(event_seq)
        conn.execute(
            """
            INSERT INTO messages (
                seq, msg_id, from_agent, to_agent, intent, body, artifact,
                in_reply_to, metadata_json, created_at
            ) VALUES (?, ?, ?, ?, 'send', ?, ?, ?, ?, ?)
            """,
            (
                event_seq,
                msg_id,
                from_agent,
                to_agent,
                normalized_body,
                artifact,
                reply_to,
                metadata_json,
                created_at,
            ),
        )
        conn.commit()
        return EventWriteResult(seq=event_seq, msg_id=msg_id)

def read_all_events(workspace: str | Path) -> list[dict[str, object]]:
    with _connect(workspace) as conn:
        rows = conn.execute("SELECT * FROM messages ORDER BY seq ASC").fetchall()
    return [_row_to_event(row) for row in rows]


def read_events_with_ns(workspace: str | Path) -> list[tuple[int, dict[str, object]]]:
    """Return sorted list of (monotonic sequence, event_data) tuples."""
    with _connect(workspace) as conn:
        rows = conn.execute("SELECT * FROM messages ORDER BY seq ASC").fetchall()
    return [(int(row["seq"]), _row_to_event(row)) for row in rows]


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
