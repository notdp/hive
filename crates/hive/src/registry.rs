//! The team registry: durable team truth under `$HIVE_HOME/state/teams/`.
//!
//! One JSON file per team. This store is the authoritative record of a team's
//! identity and roster — tmux windows and panes are a display layer resolved on
//! top of it, so a team survives a killed window or a tmux restart.
//!
//! Write lanes are split by authority:
//!
//! - **Roster membership belongs to the CLI**: [`record_team`],
//!   [`record_member`], [`remove_member`] and [`delete_team`] add and remove
//!   state at create/spawn/kill/delete time, under the store lock.
//! - **The hived only backfills**: [`backfill_members`] refreshes fields of
//!   names already in the roster and never adds one — an observation racing a
//!   kill must not resurrect the killed member.
//!
//! Schema (deliberately minimal): `team`, `workspace`, `createdAt` (instance
//! identity — a recycled name is a new instance), `display` (the tmux window
//! id currently rendering the team; a cache, never authority), and `members`
//! rows of `name` / `cli` / `model` / `sessionId` (the engine identity: claude
//! jobId, codex threadId, grok session id) / `cwd`.

use std::fs;
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use serde_json::{Map, Value};

pub const MEMBER_FIELDS: [&str; 5] = ["name", "cli", "model", "sessionId", "cwd"];

/// Serializes env-mutating tests across this crate's test modules.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `^[A-Za-z0-9][A-Za-z0-9._-]*$`
fn name_ok(team: &str) -> bool {
    let mut chars = team.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

pub fn store_dir() -> PathBuf {
    let home = std::env::var("HIVE_HOME")
        .unwrap_or_else(|_| format!("{}/.hive", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(home).join("state").join("teams")
}

/// The team's registry file, or None when the name could escape the store.
pub fn entry_path(team: &str) -> Option<PathBuf> {
    if team.is_empty() || !name_ok(team) || team.contains("..") {
        return None;
    }
    Some(store_dir().join(format!("{team}.json")))
}

/// Python truthiness for a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map_or(true, |f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `str(...)` of an optional JSON value, for `createdAt` comparisons.
fn py_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::Bool(false)) => "False".to_string(),
        Some(other) => other.to_string(),
    }
}

fn _valid(entry: &Value) -> bool {
    let obj = match entry.as_object() {
        Some(o) => o,
        None => return false,
    };
    if !obj.get("team").map_or(false, truthy) {
        return false;
    }
    let members = match obj.get("members").and_then(Value::as_array) {
        Some(m) => m,
        None => return false,
    };
    members.iter().all(|m| {
        m.as_object()
            .map_or(false, |mo| mo.get("name").map_or(false, truthy))
    })
}

/// The valid registry entry for *team*, or None (missing/corrupt/unsafe).
pub fn load(team: &str) -> Option<Map<String, Value>> {
    let path = entry_path(team)?;
    if !path.is_file() {
        return None;
    }
    let text = fs::read_to_string(&path).ok()?;
    let entry: Value = serde_json::from_str(&text).ok()?;
    if !_valid(&entry) {
        return None;
    }
    match entry {
        Value::Object(o) => Some(o),
        _ => None,
    }
}

/// Every valid registry entry; unreadable files surface as corrupt markers.
pub fn list_entries() -> Vec<Map<String, Value>> {
    let root = store_dir();
    if !root.is_dir() {
        return Vec::new();
    }
    let mut paths: Vec<PathBuf> = match fs::read_dir(&root) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                // glob("*.json"): case-sensitive, hidden files excluded
                name.ends_with(".json") && !name.starts_with('.')
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if entry_path(&stem).is_none() {
            continue;
        }
        let entry = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok());
        match entry {
            Some(v) if _valid(&v) => {
                if let Value::Object(o) = v {
                    out.push(o);
                }
            }
            _ => {
                let mut marker = Map::new();
                marker.insert("team".to_string(), Value::String(stem));
                marker.insert("corrupt".to_string(), Value::Bool(true));
                out.push(marker);
            }
        }
    }
    out
}

/// Exclusive store lock for read-merge-write cycles, released on drop.
///
/// One fcntl lock for the whole store: writers are a handful of CLI calls
/// and one hived tick per team every 30s, so contention is nil and a
/// single lock keeps the kill-vs-backfill race closed by construction.
pub struct StoreLock {
    file: fs::File,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub fn locked() -> Result<StoreLock> {
    let root = store_dir();
    fs::create_dir_all(&root)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(root.join(".lock"))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        bail!("flock failed: {}", std::io::Error::last_os_error());
    }
    Ok(StoreLock { file })
}

/// `str(member.get(field, "") or "")`.
// ponytail: string fields only — real member rows are all-string payloads;
// a non-string value normalizes to "" instead of Python's str() repr.
fn field_str(member: &Map<String, Value>, field: &str) -> String {
    member
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn _member_row(member: &Map<String, Value>) -> Map<String, Value> {
    let mut row = Map::new();
    for field in MEMBER_FIELDS {
        row.insert(field.to_string(), Value::String(field_str(member, field)));
    }
    row
}

/// Register a team at creation time (CLI write lane), overwriting any
/// predecessor a recycled name left behind. Returns `written`/`rejected`.
pub fn record_team(
    team: &str,
    workspace: &str,
    created_at: &str,
    members: &[Map<String, Value>],
    display: &str,
) -> Result<&'static str> {
    let path = match entry_path(team) {
        Some(p) => p,
        None => return Ok("rejected"),
    };
    let mut entry = Map::new();
    entry.insert("team".to_string(), Value::String(team.to_string()));
    entry.insert(
        "workspace".to_string(),
        Value::String(workspace.to_string()),
    );
    entry.insert(
        "createdAt".to_string(),
        Value::String(created_at.to_string()),
    );
    entry.insert("display".to_string(), Value::String(display.to_string()));
    let rows: Vec<Value> = members
        .iter()
        .filter(|m| m.get("name").map_or(false, truthy))
        .map(|m| Value::Object(_member_row(m)))
        .collect();
    entry.insert("members".to_string(), Value::Array(rows));
    let _lock = locked()?;
    _write_atomic(&path, &entry)?;
    Ok("written")
}

/// Add or replace one member row in the team's roster (CLI write lane).
///
/// *created_at*, when given, must match the stored instance — a stale entry
/// left by a recycled name is never edited into (returns `missing` so the
/// caller can seed a fresh entry).
pub fn record_member(
    team: &str,
    member: &Map<String, Value>,
    created_at: &str,
) -> Result<&'static str> {
    let name = field_str(member, "name");
    if name.is_empty() {
        return Ok("rejected");
    }
    let path = match entry_path(team) {
        Some(p) => p,
        None => return Ok("rejected"),
    };
    let _lock = locked()?;
    let mut entry = match load(team) {
        Some(e) => e,
        None => return Ok("missing"),
    };
    if !created_at.is_empty() && py_str(entry.get("createdAt")) != created_at {
        return Ok("missing");
    }
    let mut rows: Vec<Value> = entry
        .get("members")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    rows.retain(|m| m.get("name").and_then(Value::as_str) != Some(name.as_str()));
    rows.push(Value::Object(_member_row(member)));
    entry.insert("members".to_string(), Value::Array(rows));
    _write_atomic(&path, &entry)?;
    Ok("written")
}

/// Drop one member row from the team's roster (CLI write lane).
pub fn remove_member(team: &str, name: &str, created_at: &str) -> Result<&'static str> {
    let path = match entry_path(team) {
        Some(p) => p,
        None => return Ok("rejected"),
    };
    let _lock = locked()?;
    let mut entry = match load(team) {
        Some(e) => e,
        None => return Ok("missing"),
    };
    if !created_at.is_empty() && py_str(entry.get("createdAt")) != created_at {
        return Ok("missing");
    }
    let mut rows: Vec<Value> = entry
        .get("members")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    rows.retain(|m| m.get("name").and_then(Value::as_str) != Some(name));
    entry.insert("members".to_string(), Value::Array(rows));
    _write_atomic(&path, &entry)?;
    Ok("written")
}

/// Update the display cache (the tmux window id rendering the team).
pub fn set_display(team: &str, display: &str) -> Result<&'static str> {
    let path = match entry_path(team) {
        Some(p) => p,
        None => return Ok("rejected"),
    };
    let _lock = locked()?;
    let mut entry = match load(team) {
        Some(e) => e,
        None => return Ok("missing"),
    };
    if entry.get("display").and_then(Value::as_str) == Some(display) {
        return Ok("unchanged");
    }
    entry.insert("display".to_string(), Value::String(display.to_string()));
    _write_atomic(&path, &entry)?;
    Ok("written")
}

/// Remove the team's registry entry (delete is the team's end of life).
pub fn delete_team(team: &str) -> Result<()> {
    let path = match entry_path(team) {
        Some(p) => p,
        None => return Ok(()),
    };
    let _lock = locked()?;
    let _ = fs::remove_file(&path);
    Ok(())
}

/// Refresh fields of members already in *existing* from *observed*.
///
/// The hived's write lane: observation updates what a known member looks
/// like (model switch, cwd change, a sessionId learned late) but never adds
/// or removes a name — roster membership belongs to the CLI writers, and an
/// observation racing a `hive kill` must not resurrect the killed member.
/// Observed non-empty fields win; empty observations never erase state.
pub fn backfill_members(
    existing: &[Map<String, Value>],
    observed: &[Map<String, Value>],
) -> Vec<Map<String, Value>> {
    // ponytail: linear scans — rosters are a handful of rows
    let mut rows: Vec<(String, Map<String, Value>)> = Vec::new();
    for m in existing {
        if !m.get("name").map_or(false, truthy) {
            continue;
        }
        let name = field_str(m, "name");
        let row = _member_row(m);
        match rows.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = row,
            None => rows.push((name, row)),
        }
    }
    for obs in observed {
        let name = field_str(obs, "name");
        let entry = match rows.iter_mut().find(|(n, _)| *n == name) {
            Some((_, row)) => row,
            None => continue,
        };
        for field in ["cli", "model", "sessionId", "cwd"] {
            let value = field_str(obs, field);
            if !value.is_empty() {
                entry.insert(field.to_string(), Value::String(value));
            }
        }
    }
    rows.into_iter().map(|(_, row)| row).collect()
}

/// The hived's whole read-merge-write, under the store lock.
///
/// Refuses a missing entry (the CLI writer owns creation) and a
/// foreign-instance entry (a recycled name's predecessor must not be
/// overwritten from observation). Returns `written`/`missing`/`unchanged`.
pub fn backfill(
    team: &str,
    observed: &[Map<String, Value>],
    created_at: &str,
    display: &str,
    workspace: &str,
) -> Result<&'static str> {
    let path = match entry_path(team) {
        Some(p) => p,
        None => return Ok("missing"),
    };
    let _lock = locked()?;
    let entry = match load(team) {
        Some(e) => e,
        None => return Ok("missing"),
    };
    if py_str(entry.get("createdAt")) != created_at {
        return Ok("missing");
    }
    let existing: Vec<Map<String, Value>> = entry
        .get("members")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|m| m.as_object().cloned()).collect())
        .unwrap_or_default();
    let mut updated = entry.clone();
    updated.insert(
        "members".to_string(),
        Value::Array(
            backfill_members(&existing, observed)
                .into_iter()
                .map(Value::Object)
                .collect(),
        ),
    );
    if !display.is_empty() {
        updated.insert("display".to_string(), Value::String(display.to_string()));
    }
    if !workspace.is_empty() {
        updated.insert(
            "workspace".to_string(),
            Value::String(workspace.to_string()),
        );
    }
    if updated == entry {
        return Ok("unchanged");
    }
    _write_atomic(&path, &updated)?;
    Ok("written")
}

/// `tempfile.mkstemp(prefix=…, suffix=…, dir=…)`: exclusive 0600 temp file.
pub(crate) fn mkstemp_in(dir: &Path, prefix: &str, suffix: &str) -> Result<(fs::File, PathBuf)> {
    for attempt in 0..128u32 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let candidate = dir.join(format!(
            "{prefix}{}-{nanos:x}-{attempt}{suffix}",
            std::process::id()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(f) => return Ok((f, candidate)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("could not create temp file in {}", dir.display());
}

/// `json.dump(..., sort_keys=True)`: recursively rebuild maps in sorted key
/// order — this build's serde_json (`preserve_order`) keeps insertion order.
pub(crate) fn sort_keys(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), sort_keys(&o[k])))
                    .collect(),
            )
        }
        Value::Array(a) => Value::Array(a.iter().map(sort_keys).collect()),
        other => other.clone(),
    }
}

fn _write_atomic(path: &Path, entry: &Map<String, Value>) -> Result<()> {
    let parent = path.parent().context("registry path has no parent")?;
    fs::create_dir_all(parent)?;
    let (mut file, tmp) = mkstemp_in(parent, ".reg.", ".tmp")?;
    // json.dump(..., ensure_ascii=False, indent=2, sort_keys=True)
    let mut text = serde_json::to_string_pretty(&sort_keys(&Value::Object(entry.clone())))?;
    text.push('\n');
    // Python swallows OSError from the write/replace step (tmp cleaned up).
    let result = file
        .write_all(text.as_bytes())
        .and_then(|_| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashMap};
    use std::sync::MutexGuard;

    fn store() -> (tempfile::TempDir, PathBuf, MutexGuard<'static, ()>) {
        let guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path().join(".hive"));
        let store = tmp.path().join(".hive").join("state").join("teams");
        (tmp, store, guard)
    }

    fn m(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), Value::String((*v).to_string())))
            .collect()
    }

    fn member_names(entry: &Map<String, Value>) -> BTreeSet<String> {
        entry["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap().to_string())
            .collect()
    }

    fn names(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_record_team_round_trip_and_atomic_file() {
        let (_tmp, store, _guard) = store();
        assert_eq!(
            record_team(
                "honey",
                "/ws",
                "100.0",
                &[m(&[
                    ("name", "worker"),
                    ("cli", "claude"),
                    ("sessionId", "sid-w")
                ])],
                "@3",
            )
            .unwrap(),
            "written"
        );

        let entry = load("honey").unwrap();
        assert_eq!(entry["workspace"], "/ws");
        assert_eq!(entry["display"], "@3");
        assert_eq!(member_names(&entry), names(&["worker"]));
        // json.dump(sort_keys=True): keys are sorted on disk at every level
        let raw = fs::read_to_string(entry_path("honey").unwrap()).unwrap();
        assert!(raw.starts_with("{\n  \"createdAt\""));
        assert!(raw.find("\"cli\"").unwrap() < raw.find("\"name\"").unwrap());
        // no temp files left behind
        let leftovers: Vec<String> = fs::read_dir(&store)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".reg."))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn test_corrupt_entries_are_tolerated_and_marked() {
        let (_tmp, store, _guard) = store();
        fs::create_dir_all(&store).unwrap();
        fs::write(store.join("bad.json"), "{not json").unwrap();

        assert!(load("bad").is_none());
        let listed: HashMap<String, Map<String, Value>> = list_entries()
            .into_iter()
            .map(|e| (e["team"].as_str().unwrap().to_string(), e))
            .collect();
        assert_eq!(listed["bad"].get("corrupt"), Some(&Value::Bool(true)));
    }

    #[test]
    fn test_unsafe_names_cannot_escape_store() {
        let (_tmp, _store, _guard) = store();
        for name in ["../evil", "a/b", "", ".hidden", "a..b"] {
            assert!(entry_path(name).is_none());
            assert_eq!(record_team(name, "", "1.0", &[], "").unwrap(), "rejected");
        }
    }

    #[test]
    fn test_record_team_overwrites_a_recycled_names_predecessor() {
        let (_tmp, _store, _guard) = store();
        assert_eq!(
            record_team(
                "honey",
                "/old",
                "100.0",
                &[m(&[("name", "worker"), ("sessionId", "OLD-SID")])],
                "",
            )
            .unwrap(),
            "written"
        );
        assert_eq!(
            record_team("honey", "/new", "200.0", &[], "").unwrap(),
            "written"
        );

        let entry = load("honey").unwrap();
        assert_eq!(entry["createdAt"], "200.0");
        // nothing inherited from the predecessor
        assert_eq!(entry["members"], Value::Array(vec![]));
    }

    #[test]
    fn test_record_and_remove_member_guard_the_instance() {
        let (_tmp, _store, _guard) = store();
        assert_eq!(
            record_team(
                "honey",
                "/ws",
                "123.0",
                &[m(&[("name", "worker"), ("cli", "claude")])],
                "",
            )
            .unwrap(),
            "written"
        );
        let row = m(&[
            ("name", "validator"),
            ("cli", "codex"),
            ("sessionId", "sid-v"),
        ]);
        assert_eq!(record_member("honey", &row, "999.0").unwrap(), "missing");
        assert_eq!(record_member("honey", &row, "123.0").unwrap(), "written");
        assert_eq!(
            member_names(&load("honey").unwrap()),
            names(&["worker", "validator"])
        );
        assert_eq!(
            remove_member("honey", "validator", "999.0").unwrap(),
            "missing"
        );
        assert_eq!(
            remove_member("honey", "validator", "123.0").unwrap(),
            "written"
        );
        assert_eq!(member_names(&load("honey").unwrap()), names(&["worker"]));
    }

    #[test]
    fn test_delete_team_removes_the_entry() {
        let (_tmp, _store, _guard) = store();
        assert_eq!(
            record_team("honey", "/ws", "1.0", &[], "").unwrap(),
            "written"
        );
        delete_team("honey").unwrap();
        assert!(load("honey").is_none());
        let path = entry_path("honey").unwrap();
        assert!(!path.is_file());
    }

    #[test]
    fn test_backfill_members_keeps_dead_updates_observed_never_adds() {
        let (_tmp, _store, _guard) = store();
        let existing = vec![
            m(&[
                ("name", "worker"),
                ("cli", "claude"),
                ("model", "m1"),
                ("sessionId", "sid-w"),
                ("cwd", "/repo"),
            ]),
            m(&[
                ("name", "validator"),
                ("cli", "codex"),
                ("model", "m2"),
                ("sessionId", "sid-val"),
                ("cwd", "/repo"),
            ]),
        ];
        let merged = backfill_members(
            &existing,
            &[m(&[("name", "worker"), ("sessionId", "sid-w2")])],
        );
        let by_name: HashMap<String, &Map<String, Value>> = merged
            .iter()
            .map(|row| (row["name"].as_str().unwrap().to_string(), row))
            .collect();
        assert_eq!(by_name["worker"]["sessionId"], "sid-w2");
        assert_eq!(by_name["worker"]["cli"], "claude"); // empty observation didn't erase
        assert_eq!(by_name["validator"]["sessionId"], "sid-val"); // dead member survives

        // membership belongs to the CLI writers: an observed stranger (e.g. a
        // kill racing this observation) is never added back to the roster.
        let merged2 = backfill_members(&merged, &[m(&[("name", "ghost"), ("cli", "claude")])]);
        let names2: BTreeSet<String> = merged2
            .iter()
            .map(|row| row["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names2, names(&["worker", "validator"]));
    }

    #[test]
    fn test_backfill_refuses_missing_and_foreign_instance() {
        let (_tmp, _store, _guard) = store();
        let observed = vec![m(&[
            ("name", "worker"),
            ("cli", "claude"),
            ("sessionId", "sid-w"),
        ])];
        assert_eq!(
            backfill("honey", &observed, "123.0", "", "").unwrap(),
            "missing"
        );

        assert_eq!(
            record_team("honey", "/ws", "123.0", &[m(&[("name", "worker")])], "").unwrap(),
            "written"
        );
        assert_eq!(
            backfill("honey", &observed, "999.0", "", "").unwrap(),
            "missing"
        );
        assert_eq!(load("honey").unwrap()["members"][0]["sessionId"], "");

        assert_eq!(
            backfill("honey", &observed, "123.0", "", "").unwrap(),
            "written"
        );
        assert_eq!(load("honey").unwrap()["members"][0]["sessionId"], "sid-w");
        assert_eq!(
            backfill("honey", &observed, "123.0", "", "").unwrap(),
            "unchanged"
        );
    }

    #[test]
    fn test_backfill_observation_never_resurrects_a_killed_member() {
        // The kill-vs-backfill race, closed by construction.
        let (_tmp, _store, _guard) = store();
        assert_eq!(
            record_team(
                "honey",
                "/ws",
                "123.0",
                &[m(&[("name", "worker")]), m(&[("name", "victim")])],
                "",
            )
            .unwrap(),
            "written"
        );
        let observed = vec![
            m(&[
                ("name", "worker"),
                ("cli", "claude"),
                ("sessionId", "sid-w"),
            ]),
            m(&[("name", "victim"), ("cli", "codex"), ("sessionId", "sid-v")]),
        ];
        // hive kill removed the member between the observation and the write
        assert_eq!(
            remove_member("honey", "victim", "123.0").unwrap(),
            "written"
        );

        assert_eq!(
            backfill("honey", &observed, "123.0", "", "").unwrap(),
            "written"
        );

        assert_eq!(member_names(&load("honey").unwrap()), names(&["worker"]));
    }
}
