//! Workspace-backed agent collaboration primitives.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{Map, Value};

pub const WORKSPACE_DIRS: [&str; 3] = ["artifacts", "state", "run"];
pub const DB_FILENAME: &str = "hive.db";

pub(crate) fn now_iso() -> String {
    format!("{}Z", crate::clock::utc_now_iso_seconds())
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

/// A bus event as read back from a row. Field order is the JSON output
/// order; empty optional fields are omitted from JSON. The ledger knows
/// three things about a message — its `seq`, who sent it, who it went to —
/// and nothing about what it answers; a "reply" is whatever the reader
/// derives from that order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    pub seq: i64,
    pub from: String,
    pub to: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub body: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub artifact: String,
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS messages (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            from_agent TEXT NOT NULL,
            to_agent TEXT NOT NULL,
            body TEXT NOT NULL DEFAULT '',
            artifact TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_messages_route
            ON messages(from_agent, to_agent, seq);",
    )?;
    Ok(())
}

fn row_to_event(row: &rusqlite::Row) -> rusqlite::Result<Event> {
    Ok(Event {
        seq: row.get("seq")?,
        from: row.get("from_agent")?,
        to: row.get("to_agent")?,
        created_at: row.get("created_at")?,
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
    for name in WORKSPACE_DIRS.iter() {
        let root = ws.join(name);
        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        fs::create_dir_all(&root)?;
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

/// Append a send; returns its seq.
pub fn write_send_event(
    workspace: impl AsRef<Path>,
    from_agent: &str,
    to_agent: &str,
    body: &str,
    artifact: &str,
) -> Result<i64> {
    let created_at = now_iso();
    let conn = connect(workspace.as_ref())?;
    conn.execute(
        "INSERT INTO messages (from_agent, to_agent, body, artifact, created_at)
        VALUES (?1, ?2, ?3, ?4, ?5)",
        params![from_agent, to_agent, body.trim(), artifact, created_at],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn read_all_events(workspace: impl AsRef<Path>) -> Result<Vec<Event>> {
    let conn = connect(workspace.as_ref())?;
    let mut stmt = conn.prepare("SELECT * FROM messages ORDER BY seq ASC")?;
    let rows = stmt.query_map([], row_to_event)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn count_events(workspace: impl AsRef<Path>) -> Result<i64> {
    let conn = connect(workspace.as_ref())?;
    let count: i64 = conn.query_row("SELECT COUNT(*) AS count FROM messages", [], |row| {
        row.get(0)
    })?;
    Ok(count)
}

/// The newest `limit` events, newest first (the status bar's ticker).
pub fn latest_send_events(workspace: impl AsRef<Path>, limit: usize) -> Result<Vec<Event>> {
    let conn = connect(workspace.as_ref())?;
    let mut stmt = conn.prepare("SELECT * FROM messages ORDER BY seq DESC LIMIT ?1")?;
    let rows = stmt.query_map(params![limit as i64], row_to_event)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The first `from_agent` → `to_agent` row written after `seq`, or None.
///
/// This is what "the reply to `seq`" means on the bus: order, not a link.
/// Nothing else is filtered — a clarifying question sent after the
/// dispatch is "the reply" as far as this query knows.
pub fn first_send_after(
    workspace: impl AsRef<Path>,
    seq: i64,
    from_agent: &str,
    to_agent: &str,
) -> Result<Option<Event>> {
    let conn = connect(workspace.as_ref())?;
    let event = conn
        .query_row(
            "SELECT * FROM messages
            WHERE from_agent = ?1
              AND to_agent = ?2
              AND seq > ?3
            ORDER BY seq ASC
            LIMIT 1",
            params![from_agent, to_agent, seq],
            row_to_event,
        )
        .optional()?;
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
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
        write_send_event(&workspace, "orch", "claude", "old", "").unwrap();
        fs::write(workspace.join("artifacts").join("note.txt"), "artifact").unwrap();
        fs::write(workspace.join("state").join("mode"), "busy").unwrap();
        fs::write(workspace.join("keep.txt"), "keep").unwrap();

        reset_workspace(&workspace).unwrap();

        for name in WORKSPACE_DIRS {
            let root = workspace.join(name);
            assert!(root.is_dir());
            assert!(root.read_dir().unwrap().next().is_none());
        }
        assert!(workspace.join(DB_FILENAME).is_file());
        assert!(read_all_events(&workspace).unwrap().is_empty());
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
    fn test_write_send_event_round_trip() {
        let tmp = TempDir::new().unwrap();
        let workspace = init_workspace(tmp.path().join("ws")).unwrap();

        let seq = write_send_event(
            &workspace,
            "claude",
            "orch",
            " review complete ",
            "/tmp/review.md",
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
                "seq": 1,
                "from": "claude",
                "to": "orch",
                "createdAt": created_at,
                "body": "review complete",
                "artifact": "/tmp/review.md",
            }])
        );
    }

    #[test]
    fn test_write_send_event_seq_is_monotonic() {
        let tmp = TempDir::new().unwrap();
        let workspace = init_workspace(tmp.path().join("ws")).unwrap();

        let first = write_send_event(&workspace, "orch", "claude", "review this", "").unwrap();
        let second = write_send_event(&workspace, "claude", "orch", "done", "").unwrap();
        assert_eq!((first, second), (1, 2));

        let events = read_all_events(&workspace).unwrap();
        let seqs: Vec<i64> = events.iter().map(|event| event.seq).collect();
        assert_eq!(seqs, [1, 2]);
        assert_eq!(events[1].body, "done");
    }

    #[test]
    fn test_latest_send_events_returns_newest_first() {
        let tmp = TempDir::new().unwrap();
        let workspace = init_workspace(tmp.path().join("ws")).unwrap();

        write_send_event(&workspace, "a", "b", "first", "").unwrap();
        write_send_event(&workspace, "b", "a", "second", "").unwrap();
        write_send_event(&workspace, "a", "b", "third", "").unwrap();

        let bodies: Vec<String> = latest_send_events(&workspace, 2)
            .unwrap()
            .into_iter()
            .map(|e| e.body)
            .collect();

        assert_eq!(bodies, vec!["third".to_string(), "second".to_string()]);
    }

    #[test]
    fn test_first_send_after_is_the_reply_by_order() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        init_workspace(&ws).unwrap();
        let root = write_send_event(&ws, "flow", "impl", "task", "").unwrap();
        assert!(first_send_after(&ws, root, "impl", "flow")
            .unwrap()
            .is_none());
        // Traffic on other routes, or older than the dispatch, is not it.
        write_send_event(&ws, "bystander", "flow", "not mine", "").unwrap();
        write_send_event(&ws, "impl", "orch", "aside", "").unwrap();
        assert!(first_send_after(&ws, root, "impl", "flow")
            .unwrap()
            .is_none());

        write_send_event(&ws, "impl", "flow", "done", "/tmp/a.md").unwrap();
        write_send_event(&ws, "impl", "flow", "ps", "").unwrap();
        let row = first_send_after(&ws, root, "impl", "flow")
            .unwrap()
            .unwrap();
        assert_eq!(row.body, "done");
        assert_eq!(row.artifact, "/tmp/a.md");
        assert_eq!(row.from, "impl");
        // A later dispatch only sees what comes after it.
        let next = write_send_event(&ws, "flow", "impl", "again", "").unwrap();
        assert!(first_send_after(&ws, next, "impl", "flow")
            .unwrap()
            .is_none());
    }
}
