//! Workspace-backed agent collaboration primitives.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use serde_json::{Map, Value};

pub const WORKSPACE_DIRS: [&str; 3] = ["artifacts", "state", "run"];
pub const LEGACY_WORKSPACE_DIRS: [&str; 4] = ["status", "presence", "events", "cursors"];
pub const DB_FILENAME: &str = "hive.db";

const LEGACY_MESSAGE_RUNTIME_COLUMNS: [&str; 4] = [
    "inject_status",
    "turn_observed",
    "runtime_queue_state",
    "queue_source",
];
const MSG_ID_ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
const MSG_ID_WIDTH: usize = 4;
// Keep short IDs non-obvious without introducing collisions inside the 4-char space.
const MSG_ID_MULTIPLIER: i64 = 131071;
const MSG_ID_OFFSET: i64 = 8191;

fn msg_id_space() -> i64 {
    (MSG_ID_ALPHABET.len() as i64).pow(MSG_ID_WIDTH as u32)
}

pub(crate) fn now_iso() -> String {
    format!("{}Z", crate::devlog::utc_now_iso_seconds())
}

/// Expand only a leading bare `~`.
fn expanduser(path: &Path) -> PathBuf {
    if let Ok(stripped) = path.strip_prefix("~") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(stripped);
        }
    }
    path.to_path_buf()
}

fn db_path(workspace: &Path) -> PathBuf {
    expanduser(workspace).join(DB_FILENAME)
}

fn encode_base62(value: i64) -> String {
    debug_assert!(value >= 0, "value must be non-negative");
    if value == 0 {
        return (MSG_ID_ALPHABET[0] as char).to_string();
    }
    let base = MSG_ID_ALPHABET.len() as i64;
    let mut encoded: Vec<u8> = Vec::new();
    let mut current = value;
    while current > 0 {
        let digit = (current % base) as usize;
        current /= base;
        encoded.push(MSG_ID_ALPHABET[digit]);
    }
    encoded.reverse();
    String::from_utf8(encoded).unwrap()
}

/// Derive a short deterministic msgId from the durable row sequence.
pub fn format_msg_id(event_seq: i64) -> Result<String> {
    if event_seq <= 0 {
        bail!("event_seq must be positive");
    }
    if event_seq < msg_id_space() {
        let mixed = (event_seq * MSG_ID_MULTIPLIER + MSG_ID_OFFSET) % msg_id_space();
        return Ok(format!(
            "{:0>width$}",
            encode_base62(mixed),
            width = MSG_ID_WIDTH
        ));
    }
    Ok(encode_base62(event_seq))
}

/// Open a sqlite connection (schema initialized, WAL, 30s busy timeout).
fn connect(workspace: &Path) -> Result<Connection> {
    let ws = expanduser(workspace);
    fs::create_dir_all(&ws)?;
    let conn = Connection::open(db_path(&ws))?;
    conn.busy_timeout(Duration::from_secs(30))?;
    // `PRAGMA journal_mode=WAL` returns a row; query_row consumes it safely.
    conn.query_row("PRAGMA journal_mode=WAL", [], |_row| Ok(()))?;
    conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
    init_schema(&conn)?;
    Ok(conn)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventWriteResult {
    pub seq: i64,
    pub msg_id: String,
}

/// A bus event as read back from a row. Field order is the JSON output
/// order; empty optional fields are omitted from JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    pub from: String,
    pub to: String,
    pub intent: String,
    pub metadata: Map<String, Value>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "msgId", skip_serializing_if = "String::is_empty")]
    pub msg_id: String,
    #[serde(rename = "inReplyTo", skip_serializing_if = "String::is_empty")]
    pub in_reply_to: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub body: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub artifact: String,
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS messages (
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
            ON messages(msg_id, intent, seq);",
    )?;
    migrate_messages_table(conn)?;
    Ok(())
}

fn table_columns(conn: &Connection, table_name: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table_name})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>("name"))?;
    rows.collect()
}

fn migrate_messages_table(conn: &Connection) -> rusqlite::Result<()> {
    let columns = table_columns(conn, "messages")?;
    if columns.is_empty() {
        return Ok(());
    }
    if !LEGACY_MESSAGE_RUNTIME_COLUMNS
        .iter()
        .any(|legacy| columns.iter().any(|column| column == legacy))
    {
        return Ok(());
    }

    conn.execute_batch(
        "BEGIN IMMEDIATE;
        DROP INDEX IF EXISTS idx_messages_msg_intent_seq;
        ALTER TABLE messages RENAME TO messages_legacy;
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
        );
        INSERT INTO messages (
            seq, msg_id, from_agent, to_agent, intent, body, artifact,
            in_reply_to, metadata_json, created_at
        )
        SELECT
            seq, msg_id, from_agent, to_agent, intent, body, artifact,
            in_reply_to, metadata_json, created_at
        FROM messages_legacy
        ORDER BY seq ASC;
        DROP TABLE messages_legacy;
        CREATE INDEX IF NOT EXISTS idx_messages_msg_intent_seq ON messages(msg_id, intent, seq);
        COMMIT;",
    )
}

fn row_to_event(row: &rusqlite::Row) -> rusqlite::Result<Event> {
    let metadata_raw: String = row.get("metadata_json")?;
    let metadata = match serde_json::from_str::<Value>(&metadata_raw) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    Ok(Event {
        from: row.get("from_agent")?,
        to: row.get("to_agent")?,
        intent: row.get("intent")?,
        metadata,
        created_at: row.get("created_at")?,
        msg_id: row.get("msg_id")?,
        in_reply_to: row.get("in_reply_to")?,
        body: row.get("body")?,
        artifact: row.get("artifact")?,
    })
}

pub fn init_workspace(workspace: impl AsRef<Path>) -> Result<PathBuf> {
    let ws = expanduser(workspace.as_ref());
    if let Err(reason) = crate::devlog::check_socket_path_len(&ws) {
        bail!("{reason}");
    }
    for name in WORKSPACE_DIRS {
        fs::create_dir_all(ws.join(name))?;
    }
    connect(&ws)?;
    Ok(ws)
}

pub fn reset_workspace(workspace: impl AsRef<Path>) -> Result<PathBuf> {
    let ws = expanduser(workspace.as_ref());
    if let Err(reason) = crate::devlog::check_socket_path_len(&ws) {
        bail!("{reason}");
    }
    fs::create_dir_all(&ws)?;
    for name in WORKSPACE_DIRS.iter().chain(LEGACY_WORKSPACE_DIRS.iter()) {
        let root = ws.join(name);
        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        if WORKSPACE_DIRS.contains(name) {
            fs::create_dir_all(&root)?;
        }
    }
    let db = db_path(&ws);
    let mut wal = db.clone().into_os_string();
    wal.push("-wal");
    let mut shm = db.clone().into_os_string();
    shm.push("-shm");
    for path in [db, PathBuf::from(wal), PathBuf::from(shm)] {
        if path.exists() {
            fs::remove_file(&path)?;
        }
    }
    connect(&ws)?;
    Ok(ws)
}

pub fn parse_key_value(entries: &[String]) -> Result<Map<String, Value>> {
    let mut data = Map::new();
    for entry in entries {
        let Some((key, value)) = entry.split_once('=') else {
            bail!("invalid KEY=VALUE entry '{entry}'");
        };
        let key = key.trim();
        if key.is_empty() {
            bail!("invalid KEY=VALUE entry '{entry}', empty key");
        }
        data.insert(key.to_string(), Value::String(value.to_string()));
    }
    Ok(data)
}

#[allow(clippy::too_many_arguments)]
pub fn write_event(
    workspace: impl AsRef<Path>,
    from_agent: &str,
    to_agent: &str,
    intent: &str,
    body: &str,
    artifact: &str,
    metadata: Option<&Map<String, Value>>,
    message_id: &str,
    reply_to: &str,
) -> Result<i64> {
    let normalized_body = body.trim();
    let empty = Map::new();
    let metadata_json = serde_json::to_string(metadata.unwrap_or(&empty))?;
    let created_at = now_iso();
    let conn = connect(workspace.as_ref())?;
    conn.execute(
        "INSERT INTO messages (
            msg_id, from_agent, to_agent, intent, body, artifact,
            in_reply_to, metadata_json, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            message_id,
            from_agent,
            to_agent,
            intent,
            normalized_body,
            artifact,
            reply_to,
            metadata_json,
            created_at,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Write a send event with its deterministic msgId in one transaction.
pub fn write_send_event(
    workspace: impl AsRef<Path>,
    from_agent: &str,
    to_agent: &str,
    body: &str,
    artifact: &str,
    metadata: Option<&Map<String, Value>>,
    reply_to: &str,
) -> Result<EventWriteResult> {
    let normalized_body = body.trim();
    let empty = Map::new();
    let metadata_json = serde_json::to_string(metadata.unwrap_or(&empty))?;
    let created_at = now_iso();
    let mut conn = connect(workspace.as_ref())?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let event_seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 AS seq FROM messages",
        [],
        |row| row.get(0),
    )?;
    let msg_id = format_msg_id(event_seq)?;
    tx.execute(
        "INSERT INTO messages (
            seq, msg_id, from_agent, to_agent, intent, body, artifact,
            in_reply_to, metadata_json, created_at
        ) VALUES (?1, ?2, ?3, ?4, 'send', ?5, ?6, ?7, ?8, ?9)",
        params![
            event_seq,
            msg_id,
            from_agent,
            to_agent,
            normalized_body,
            artifact,
            reply_to,
            metadata_json,
            created_at,
        ],
    )?;
    tx.commit()?;
    Ok(EventWriteResult {
        seq: event_seq,
        msg_id,
    })
}

pub fn read_all_events(workspace: impl AsRef<Path>) -> Result<Vec<Event>> {
    let conn = connect(workspace.as_ref())?;
    let mut stmt = conn.prepare("SELECT * FROM messages ORDER BY seq ASC")?;
    let rows = stmt.query_map([], row_to_event)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Return sorted list of (monotonic sequence, event_data) tuples.
pub fn read_events_with_seq(workspace: impl AsRef<Path>) -> Result<Vec<(i64, Event)>> {
    let conn = connect(workspace.as_ref())?;
    let mut stmt = conn.prepare("SELECT * FROM messages ORDER BY seq ASC")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>("seq")?, row_to_event(row)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn count_events(workspace: impl AsRef<Path>) -> Result<i64> {
    let conn = connect(workspace.as_ref())?;
    let count: i64 = conn.query_row("SELECT COUNT(*) AS count FROM messages", [], |row| {
        row.get(0)
    })?;
    Ok(count)
}

/// Return the latest send event from `target` to `sender` with a msgId.
pub fn latest_inbound_send_event(
    workspace: impl AsRef<Path>,
    sender: &str,
    target: &str,
) -> Result<Option<Event>> {
    let conn = connect(workspace.as_ref())?;
    let event = conn
        .query_row(
            "SELECT * FROM messages
            WHERE intent = 'send'
              AND from_agent = ?1
              AND to_agent = ?2
              AND msg_id != ''
            ORDER BY seq DESC
            LIMIT 1",
            params![target, sender],
            row_to_event,
        )
        .optional()?;
    Ok(event)
}

/// The newest `limit` send events, newest first (the status bar's ticker).
pub fn latest_send_events(workspace: impl AsRef<Path>, limit: usize) -> Result<Vec<Event>> {
    let conn = connect(workspace.as_ref())?;
    let mut stmt =
        conn.prepare("SELECT * FROM messages WHERE intent = 'send' ORDER BY seq DESC LIMIT ?1")?;
    let rows = stmt.query_map(params![limit as i64], row_to_event)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Return the first send event anchored to `msg_id`, or None.
///
/// A non-empty `from_agent` scopes the match to one sender; empty means any
/// agent's send anchored to `msg_id` wins. Nothing else is filtered — not the
/// recipient, not what the body means. A clarifying question anchored to the
/// dispatch is "the reply" as far as this query knows.
pub fn find_reply_to(
    workspace: impl AsRef<Path>,
    msg_id: &str,
    from_agent: &str,
) -> Result<Option<Event>> {
    if msg_id.is_empty() {
        return Ok(None);
    }
    let mut sql = String::from("SELECT * FROM messages WHERE intent = 'send' AND in_reply_to = ?");
    let mut sql_params: Vec<&dyn rusqlite::ToSql> = vec![&msg_id];
    if !from_agent.is_empty() {
        sql.push_str(" AND from_agent = ?");
        sql_params.push(&from_agent);
    }
    sql.push_str(" ORDER BY seq ASC LIMIT 1");
    let conn = connect(workspace.as_ref())?;
    let event = conn
        .query_row(&sql, sql_params.as_slice(), row_to_event)
        .optional()?;
    Ok(event)
}

/// True if `sender` already wrote a send event to `target` with in_reply_to=msg_id.
pub fn has_send_reply_to(
    workspace: impl AsRef<Path>,
    msg_id: &str,
    sender: &str,
    target: &str,
) -> Result<bool> {
    if msg_id.is_empty() {
        return Ok(false);
    }
    let conn = connect(workspace.as_ref())?;
    let row = conn
        .query_row(
            "SELECT 1 FROM messages
            WHERE intent = 'send'
              AND from_agent = ?1
              AND to_agent = ?2
              AND in_reply_to = ?3
            LIMIT 1",
            params![sender, target, msg_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(row.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;
    use tempfile::TempDir;

    fn assert_created_at_shape(created_at: &str) {
        assert_eq!(created_at.len(), 20, "createdAt = {created_at}");
        assert!(created_at.ends_with('Z'), "createdAt = {created_at}");
    }

    #[test]
    fn test_init_workspace_accepts_a_path_too_long_for_an_in_tree_socket() {
        // the hived socket relocates for such a workspace; init must not
        // turn a deep scratch directory into a refusal
        let tmp = TempDir::new().unwrap();
        let long = tmp
            .path()
            .join("x".repeat(crate::devlog::max_socket_path_len()));
        let ws = init_workspace(&long).unwrap();
        assert!(ws.join("run").is_dir());
        assert!(crate::devlog::hived_socket_is_relocated(&ws.join("run")));
        reset_workspace(&long).unwrap();
    }

    #[test]
    fn test_init_workspace_creates_expected_directories() {
        let tmp = TempDir::new().unwrap();
        let workspace = init_workspace(tmp.path().join("ws")).unwrap();

        assert_eq!(workspace, tmp.path().join("ws"));
        for name in WORKSPACE_DIRS {
            assert!(workspace.join(name).is_dir());
        }
        assert!(workspace.join(DB_FILENAME).is_file());
    }

    #[test]
    fn test_init_workspace_does_not_create_cursor_table() {
        let tmp = TempDir::new().unwrap();
        let workspace = init_workspace(tmp.path().join("ws")).unwrap();

        let conn = Connection::open(workspace.join(DB_FILENAME)).unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();

        assert!(!names.iter().any(|name| name == "cursors"));
    }

    #[test]
    fn test_reset_workspace_recreates_managed_dirs_and_clears_contents() {
        let tmp = TempDir::new().unwrap();
        let workspace = init_workspace(tmp.path().join("ws")).unwrap();
        write_event(
            &workspace, "orch", "claude", "send", "old", "", None, "old1", "",
        )
        .unwrap();
        fs::write(workspace.join("artifacts").join("note.txt"), "artifact").unwrap();
        fs::write(workspace.join("state").join("mode"), "busy").unwrap();
        // Legacy dirs are cleaned up on reset.
        fs::create_dir_all(workspace.join("status")).unwrap();
        fs::write(
            workspace.join("status").join("legacy.json"),
            "{\"state\":\"done\"}",
        )
        .unwrap();
        fs::create_dir_all(workspace.join("presence")).unwrap();
        fs::write(
            workspace.join("presence").join("team.json"),
            "{\"team\":\"dev\"}",
        )
        .unwrap();
        fs::write(workspace.join("keep.txt"), "keep").unwrap();

        reset_workspace(&workspace).unwrap();

        for name in WORKSPACE_DIRS {
            let root = workspace.join(name);
            assert!(root.is_dir());
            assert!(root.read_dir().unwrap().next().is_none());
        }
        assert!(workspace.join(DB_FILENAME).is_file());
        assert!(read_all_events(&workspace).unwrap().is_empty());
        assert!(!workspace.join("status").exists());
        assert!(!workspace.join("presence").exists());
        assert!(!workspace.join("events").exists());
        assert!(!workspace.join("cursors").exists());
        assert_eq!(
            fs::read_to_string(workspace.join("keep.txt")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn test_reset_workspace_removes_sqlite_trio_before_reconnect() {
        // Garbage trio content would make
        // the reconnect fail ("file is not a database") unless the files were
        // removed first, so a clean reset proves the same regression fix.
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let db_path = workspace.join(DB_FILENAME);
        let wal_path = workspace.join(format!("{DB_FILENAME}-wal"));
        let shm_path = workspace.join(format!("{DB_FILENAME}-shm"));
        fs::write(&db_path, "db").unwrap();
        fs::write(&wal_path, "wal").unwrap();
        fs::write(&shm_path, "shm").unwrap();

        reset_workspace(&workspace).unwrap();

        assert!(read_all_events(&workspace).unwrap().is_empty());
        assert!(!wal_path.exists());
        assert!(!shm_path.exists());
    }

    #[test]
    fn test_parse_key_value_parses_and_overwrites_later_values() {
        let payload = parse_key_value(&[
            "repo=owner/repo".to_string(),
            "stage=1".to_string(),
            "stage=2".to_string(),
        ])
        .unwrap();

        assert_eq!(
            serde_json::to_value(&payload).unwrap(),
            json!({"repo": "owner/repo", "stage": "2"})
        );
    }

    #[test]
    fn test_parse_key_value_rejects_invalid_entries() {
        let err = parse_key_value(&["missing-separator".to_string()]).unwrap_err();
        assert!(err.to_string().contains("invalid KEY=VALUE entry"));

        let err = parse_key_value(&[" =value".to_string()]).unwrap_err();
        assert!(err.to_string().contains("empty key"));
    }

    #[test]
    fn test_write_event_round_trip() {
        let tmp = TempDir::new().unwrap();
        let workspace = init_workspace(tmp.path().join("ws")).unwrap();
        let mut metadata = Map::new();
        metadata.insert("verdict".to_string(), Value::String("issues".to_string()));

        let seq = write_event(
            &workspace,
            "claude",
            "orch",
            "send",
            "review complete",
            "/tmp/review.md",
            Some(&metadata),
            "ab12",
            "",
        )
        .unwrap();

        assert_eq!(seq, 1);
        let events = read_all_events(&workspace).unwrap();
        assert_eq!(events.len(), 1);
        // The actual timestamp is asserted for shape and reused in the
        // full-event comparison.
        let created_at = events[0].created_at.clone();
        assert_created_at_shape(&created_at);
        assert_eq!(
            serde_json::to_value(&events).unwrap(),
            json!([{
                "msgId": "ab12",
                "from": "claude",
                "to": "orch",
                "intent": "send",
                "body": "review complete",
                "artifact": "/tmp/review.md",
                "metadata": {"verdict": "issues"},
                "createdAt": created_at,
            }])
        );
    }

    #[test]
    fn test_write_event_multiple_events() {
        let tmp = TempDir::new().unwrap();
        let workspace = init_workspace(tmp.path().join("ws")).unwrap();

        write_event(
            &workspace,
            "orch",
            "claude",
            "send",
            "review this diff",
            "",
            None,
            "aa01",
            "",
        )
        .unwrap();
        write_event(
            &workspace,
            "orch",
            "gpt",
            "send",
            "pick a strategy",
            "",
            None,
            "bb02",
            "",
        )
        .unwrap();

        let events = read_all_events(&workspace).unwrap();
        let msg_ids: Vec<&str> = events.iter().map(|event| event.msg_id.as_str()).collect();
        assert_eq!(msg_ids, ["aa01", "bb02"]);
        assert_eq!(events[0].body, "review this diff");
        assert_eq!(events[1].body, "pick a strategy");
    }

    #[test]
    fn test_format_msg_id_is_short_and_unique_for_small_range() {
        let values: Vec<String> = (1..2000).map(|i| format_msg_id(i).unwrap()).collect();

        assert!(values.iter().all(|value| value.len() == 4));
        let unique: HashSet<&String> = values.iter().collect();
        assert_eq!(unique.len(), values.len());
    }

    #[test]
    fn test_write_send_event_assigns_msg_id_without_followup_patch() {
        let tmp = TempDir::new().unwrap();
        let workspace = init_workspace(tmp.path().join("ws")).unwrap();

        let result = write_send_event(
            &workspace,
            "claude",
            "orch",
            "review complete",
            "/tmp/review.md",
            None,
            "r1",
        )
        .unwrap();

        assert_eq!(result.seq, 1);
        assert_eq!(result.msg_id, format_msg_id(1).unwrap());
        let events = read_all_events(&workspace).unwrap();
        assert_eq!(events.len(), 1);
        let created_at = events[0].created_at.clone();
        assert_created_at_shape(&created_at);
        assert_eq!(
            serde_json::to_value(&events).unwrap(),
            json!([{
                "msgId": result.msg_id,
                "from": "claude",
                "to": "orch",
                "intent": "send",
                "body": "review complete",
                "artifact": "/tmp/review.md",
                "inReplyTo": "r1",
                "metadata": {},
                "createdAt": created_at,
            }])
        );
    }

    #[test]
    fn test_latest_inbound_send_event_returns_none_when_no_match() {
        let tmp = TempDir::new().unwrap();
        let workspace = init_workspace(tmp.path().join("ws")).unwrap();

        write_send_event(&workspace, "orch", "claude", "hi", "", None, "").unwrap();

        assert!(latest_inbound_send_event(&workspace, "orch", "claude")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_latest_inbound_send_event_picks_most_recent_matching() {
        let tmp = TempDir::new().unwrap();
        let workspace = init_workspace(tmp.path().join("ws")).unwrap();

        write_send_event(&workspace, "dodo", "orch", "first", "", None, "").unwrap();
        write_send_event(&workspace, "claude", "orch", "other", "", None, "").unwrap();
        let second = write_send_event(&workspace, "dodo", "orch", "second", "", None, "").unwrap();

        let event = latest_inbound_send_event(&workspace, "orch", "dodo")
            .unwrap()
            .unwrap();

        assert_eq!(event.msg_id, second.msg_id);
        assert_eq!(event.body, "second");
    }

    #[test]
    fn test_latest_send_events_returns_newest_first_and_only_sends() {
        let tmp = TempDir::new().unwrap();
        let workspace = init_workspace(tmp.path().join("ws")).unwrap();

        write_send_event(&workspace, "a", "b", "first", "", None, "").unwrap();
        write_event(
            &workspace,
            "_system",
            "",
            "observation",
            "",
            "",
            None,
            "",
            "",
        )
        .unwrap();
        write_send_event(&workspace, "b", "a", "second", "", None, "").unwrap();
        write_send_event(&workspace, "a", "b", "third", "", None, "").unwrap();

        let bodies: Vec<String> = latest_send_events(&workspace, 2)
            .unwrap()
            .into_iter()
            .map(|e| e.body)
            .collect();

        assert_eq!(bodies, vec!["third".to_string(), "second".to_string()]);
        assert!(latest_send_events(&workspace, 10)
            .unwrap()
            .iter()
            .all(|e| e.intent == "send"));
    }

    #[test]
    fn test_has_send_reply_to_detects_prior_reply() {
        let tmp = TempDir::new().unwrap();
        let workspace = init_workspace(tmp.path().join("ws")).unwrap();

        let inbound =
            write_send_event(&workspace, "dodo", "orch", "review?", "", None, "").unwrap();
        write_send_event(&workspace, "orch", "dodo", "fresh take", "", None, "").unwrap();
        assert!(!has_send_reply_to(&workspace, &inbound.msg_id, "orch", "dodo").unwrap());

        write_send_event(&workspace, "orch", "dodo", "ack", "", None, &inbound.msg_id).unwrap();

        assert!(has_send_reply_to(&workspace, &inbound.msg_id, "orch", "dodo").unwrap());
        // Reply in the opposite direction must not count.
        assert!(!has_send_reply_to(&workspace, &inbound.msg_id, "dodo", "orch").unwrap());
    }

    #[test]
    fn test_has_send_reply_to_returns_false_for_empty_msg_id() {
        let tmp = TempDir::new().unwrap();
        let workspace = init_workspace(tmp.path().join("ws")).unwrap();
        assert!(!has_send_reply_to(&workspace, "", "orch", "dodo").unwrap());
    }

    #[test]
    fn test_init_workspace_migrates_legacy_runtime_columns() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let db_path = workspace.join(DB_FILENAME);
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
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
            )",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (
                msg_id, from_agent, to_agent, intent, body, artifact,
                in_reply_to, metadata_json, created_at,
                inject_status, turn_observed, runtime_queue_state, queue_source
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                "a1b2",
                "orch",
                "claude",
                "send",
                "hello",
                "",
                "",
                "{}",
                "2026-03-17T10:00:00Z",
                "submitted",
                "pending",
                "queued",
                "capture",
            ],
        )
        .unwrap();
        drop(conn);

        init_workspace(&workspace).unwrap();

        let conn = Connection::open(&db_path).unwrap();
        let columns = table_columns(&conn, "messages").unwrap();
        drop(conn);
        assert!(!columns.iter().any(|column| column == "inject_status"));
        assert!(!columns.iter().any(|column| column == "turn_observed"));
        assert!(!columns.iter().any(|column| column == "runtime_queue_state"));
        assert!(!columns.iter().any(|column| column == "queue_source"));
        assert_eq!(
            serde_json::to_value(read_all_events(&workspace).unwrap()).unwrap(),
            json!([{
                "msgId": "a1b2",
                "from": "orch",
                "to": "claude",
                "intent": "send",
                "body": "hello",
                "metadata": {},
                "createdAt": "2026-03-17T10:00:00Z",
            }])
        );
    }

    #[test]
    fn test_find_reply_to_returns_first_anchored_send() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        init_workspace(&ws).unwrap();
        let root = write_send_event(&ws, "flow", "impl", "task", "", None, "").unwrap();
        assert!(find_reply_to(&ws, &root.msg_id, "").unwrap().is_none());
        write_send_event(&ws, "impl", "flow", "done", "/tmp/a.md", None, &root.msg_id).unwrap();
        let row = find_reply_to(&ws, &root.msg_id, "").unwrap().unwrap();
        assert_eq!(row.body, "done");
        assert_eq!(row.artifact, "/tmp/a.md");
        assert!(find_reply_to(&ws, "", "").unwrap().is_none());
    }

    #[test]
    fn test_find_reply_to_scopes_to_from_agent() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        init_workspace(&ws).unwrap();
        let root = write_send_event(&ws, "flow", "impl", "task", "", None, "").unwrap();
        write_send_event(&ws, "bystander", "flow", "not mine", "", None, &root.msg_id).unwrap();
        assert_eq!(
            find_reply_to(&ws, &root.msg_id, "").unwrap().unwrap().body,
            "not mine"
        );
        assert!(find_reply_to(&ws, &root.msg_id, "impl").unwrap().is_none());

        write_send_event(&ws, "impl", "flow", "done", "", None, &root.msg_id).unwrap();
        let row = find_reply_to(&ws, &root.msg_id, "impl").unwrap().unwrap();
        assert_eq!(row.body, "done");
        assert_eq!(row.from, "impl");
    }
}
