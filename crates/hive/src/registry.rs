//! The team registry: durable team truth under `$HIVE_HOME/teams/`.
//!
//! One directory per team, `$HIVE_HOME/teams/<team>/`, holding everything
//! hive owns for the team: `team.json` (the registry entry this module
//! reads and writes), and — when the team runs on its default workspace,
//! which is this same directory — the bus `hive.db`, `run/` (hived socket,
//! notify log, cvim runs) and `artifacts/`. A team exists when its
//! `team.json` does; a directory without one is a leftover workspace a
//! deleted team kept, not a team. The entry is the authoritative record of
//! a team's identity and roster — tmux windows and panes are a display
//! layer resolved on top of it, so a team survives a killed window or a
//! tmux restart.
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

use crate::pyval::truthy;

pub const MEMBER_FIELDS: [&str; 5] = ["name", "cli", "model", "sessionId", "cwd"];

/// `^[A-Za-z0-9][A-Za-z0-9._-]*$`
fn name_ok(team: &str) -> bool {
    let mut chars = team.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

pub const ENTRY_FILE: &str = "team.json";

/// `$HIVE_HOME/teams/`: one directory per team, plus the store lock.
pub fn store_dir() -> PathBuf {
    crate::team::hive_home().join("teams")
}

/// The team's directory, `$HIVE_HOME/teams/<team>/`, or None when the name
/// could escape the store. Also the team's default workspace.
pub fn team_dir(team: &str) -> Option<PathBuf> {
    if team.is_empty() || !name_ok(team) || team.contains("..") {
        return None;
    }
    Some(store_dir().join(team))
}

/// The team's registry file, `$HIVE_HOME/teams/<team>/team.json`.
pub fn entry_path(team: &str) -> Option<PathBuf> {
    team_dir(team).map(|dir| dir.join(ENTRY_FILE))
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

fn valid(entry: &Value) -> bool {
    let obj = match entry.as_object() {
        Some(o) => o,
        None => return false,
    };
    if !truthy(obj.get("team")) {
        return false;
    }
    let members = match obj.get("members").and_then(Value::as_array) {
        Some(m) => m,
        None => return false,
    };
    members
        .iter()
        .all(|m| m.as_object().is_some_and(|mo| truthy(mo.get("name"))))
}

/// The valid registry entry for *team*, or None (missing/corrupt/unsafe).
pub fn load(team: &str) -> Option<Map<String, Value>> {
    let path = entry_path(team)?;
    if !path.is_file() {
        return None;
    }
    let text = fs::read_to_string(&path).ok()?;
    let entry: Value = serde_json::from_str(&text).ok()?;
    if !valid(&entry) {
        return None;
    }
    match entry {
        Value::Object(o) => Some(o),
        _ => None,
    }
}

/// (team, member) of the roster row whose sessionId is *session_id*,
/// optionally narrowed to rows of one *cli*.
///
/// The session rung of the identity ladder: an engine's tool subprocess
/// carries the id the engine minted for itself (a codex thread id, a grok
/// session id, a Claude session's socket), and the row match *is* the
/// identity — with a cli given, a row of another cli carrying the same id
/// is a stranger. No liveness is recorded here; delivery enforces it.
pub fn member_for_session(session_id: &str, cli: Option<&str>) -> Option<(String, String)> {
    if session_id.is_empty() {
        return None;
    }
    for entry in list_entries() {
        let Some(members) = entry.get("members").and_then(Value::as_array) else {
            continue;
        };
        for m in members.iter().filter_map(Value::as_object) {
            if field_str(m, "sessionId") == session_id
                && cli.is_none_or(|cli| field_str(m, "cli") == cli)
            {
                return Some((field_str(&entry, "team"), field_str(m, "name")));
            }
        }
    }
    None
}

/// Every valid registry entry, by team name; a directory whose `team.json`
/// is unreadable surfaces as a corrupt marker, one without a `team.json` is
/// not a team (a leftover workspace) and is skipped.
pub fn list_entries() -> Vec<Map<String, Value>> {
    let root = store_dir();
    if !root.is_dir() {
        return Vec::new();
    }
    let mut dirs: Vec<PathBuf> = match fs::read_dir(&root) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
        Err(_) => return Vec::new(),
    };
    dirs.sort();
    let mut out = Vec::new();
    for dir in dirs {
        let team = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let Some(path) = entry_path(&team) else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        let entry = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok());
        match entry {
            Some(v) if valid(&v) => {
                if let Value::Object(o) = v {
                    out.push(o);
                }
            }
            _ => {
                let mut marker = Map::new();
                marker.insert("team".to_string(), Value::String(team));
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
        .truncate(false)
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

fn member_row(member: &Map<String, Value>) -> Map<String, Value> {
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
        .filter(|m| truthy(m.get("name")))
        .map(|m| Value::Object(member_row(m)))
        .collect();
    entry.insert("members".to_string(), Value::Array(rows));
    let _lock = locked()?;
    write_atomic(&path, &entry)?;
    Ok("written")
}

/// A registry entry opened for one read-merge-write cycle; the store lock
/// is held until this drops.
struct Opened {
    path: PathBuf,
    entry: Map<String, Value>,
    _lock: StoreLock,
}

enum Open {
    Ready(Opened),
    Refused(&'static str),
}

/// The write lane's shared prelude: lock the store, then load *team*'s
/// entry. Refuses with `rejected` (unsafe name), `missing` (no entry: the
/// team was deleted), or `stale` (*created_at*, when given, does not match
/// the stored instance: a recycled name's successor is never edited into).
fn open_instance(team: &str, created_at: &str) -> Result<Open> {
    let path = match entry_path(team) {
        Some(p) => p,
        None => return Ok(Open::Refused("rejected")),
    };
    let _lock = locked()?;
    let entry = match load(team) {
        Some(e) => e,
        None => return Ok(Open::Refused("missing")),
    };
    if !created_at.is_empty() && py_str(entry.get("createdAt")) != created_at {
        return Ok(Open::Refused("stale"));
    }
    Ok(Open::Ready(Opened { path, entry, _lock }))
}

fn member_rows(entry: &Map<String, Value>) -> Vec<Value> {
    entry
        .get("members")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Add or replace one member row in the team's roster (CLI write lane).
///
/// Returns `written` or one of `open_instance`'s refusals
/// (`rejected`/`missing`/`stale`). No refusal is a cue to seed an entry —
/// only `record_team` creates one.
pub fn record_member(
    team: &str,
    member: &Map<String, Value>,
    created_at: &str,
) -> Result<&'static str> {
    let name = field_str(member, "name");
    if name.is_empty() {
        return Ok("rejected");
    }
    let Opened {
        path,
        mut entry,
        _lock,
    } = match open_instance(team, created_at)? {
        Open::Ready(o) => o,
        Open::Refused(verdict) => return Ok(verdict),
    };
    let mut rows = member_rows(&entry);
    rows.retain(|m| m.get("name").and_then(Value::as_str) != Some(name.as_str()));
    rows.push(Value::Object(member_row(member)));
    entry.insert("members".to_string(), Value::Array(rows));
    write_atomic(&path, &entry)?;
    Ok("written")
}

/// Atomically claim a member name in the team's roster (CLI write lane).
///
/// The check-then-insert runs under the store lock, so two processes racing
/// the same name get one `reserved` and one `exists` — the cross-process
/// guard `Team::spawn`'s in-memory already-exists check cannot provide. The
/// claiming row is a placeholder (no pane yet); the spawn's `record_member`
/// replaces it, and a failed spawn removes it. `missing`/`stale` refuse as
/// in `record_member`.
pub fn reserve_member(
    team: &str,
    member: &Map<String, Value>,
    created_at: &str,
) -> Result<&'static str> {
    let name = field_str(member, "name");
    if name.is_empty() {
        return Ok("rejected");
    }
    let Opened {
        path,
        mut entry,
        _lock,
    } = match open_instance(team, created_at)? {
        Open::Ready(o) => o,
        Open::Refused(verdict) => return Ok(verdict),
    };
    let mut rows = member_rows(&entry);
    if rows
        .iter()
        .any(|m| m.get("name").and_then(Value::as_str) == Some(name.as_str()))
    {
        return Ok("exists");
    }
    rows.push(Value::Object(member_row(member)));
    entry.insert("members".to_string(), Value::Array(rows));
    write_atomic(&path, &entry)?;
    Ok("reserved")
}

/// Drop one member row from the team's roster (CLI write lane).
/// `missing`/`stale` refuse as in `record_member`.
pub fn remove_member(team: &str, name: &str, created_at: &str) -> Result<&'static str> {
    let Opened {
        path,
        mut entry,
        _lock,
    } = match open_instance(team, created_at)? {
        Open::Ready(o) => o,
        Open::Refused(verdict) => return Ok(verdict),
    };
    let mut rows = member_rows(&entry);
    rows.retain(|m| m.get("name").and_then(Value::as_str) != Some(name));
    entry.insert("members".to_string(), Value::Array(rows));
    write_atomic(&path, &entry)?;
    Ok("written")
}

/// Update the display cache (the tmux window id rendering the team).
pub fn set_display(team: &str, display: &str) -> Result<&'static str> {
    let Opened {
        path,
        mut entry,
        _lock,
    } = match open_instance(team, "")? {
        Open::Ready(o) => o,
        Open::Refused(verdict) => return Ok(verdict),
    };
    if entry.get("display").and_then(Value::as_str) == Some(display) {
        return Ok("unchanged");
    }
    entry.insert("display".to_string(), Value::String(display.to_string()));
    write_atomic(&path, &entry)?;
    Ok("written")
}

/// Remove the team's registry entry (delete is the team's end of life).
///
/// Only `team.json` goes: whatever else the team directory holds (its
/// default workspace — bus, run dir, artifacts) is `hive delete
/// --delete-workspace`'s to remove. A directory left empty is dropped.
pub fn delete_team(team: &str) -> Result<()> {
    let path = match entry_path(team) {
        Some(p) => p,
        None => return Ok(()),
    };
    let _lock = locked()?;
    let _ = fs::remove_file(&path);
    if let Some(dir) = path.parent() {
        let _ = fs::remove_dir(dir);
    }
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
        if !truthy(m.get("name")) {
            continue;
        }
        let name = field_str(m, "name");
        let row = member_row(m);
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
/// overwritten from observation). Returns `written`/`missing`/`unchanged`,
/// or `rejected` for an unsafe team name.
pub fn backfill(
    team: &str,
    observed: &[Map<String, Value>],
    created_at: &str,
    display: &str,
    workspace: &str,
) -> Result<&'static str> {
    let path = match entry_path(team) {
        Some(p) => p,
        None => return Ok("rejected"),
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
    write_atomic(&path, &updated)?;
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

fn write_atomic(path: &Path, entry: &Map<String, Value>) -> Result<()> {
    let parent = path.parent().context("registry path has no parent")?;
    fs::create_dir_all(parent)?;
    let (mut file, tmp) = mkstemp_in(parent, ".reg.", ".tmp")?;
    // json.dump(..., ensure_ascii=False, indent=2, sort_keys=True)
    let mut text = serde_json::to_string_pretty(&sort_keys(&Value::Object(entry.clone())))?;
    text.push('\n');
    let result = file
        .write_all(text.as_bytes())
        .and_then(|_| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;
    use std::collections::{BTreeSet, HashMap};

    fn store() -> (tempfile::TempDir, PathBuf, EnvGuard) {
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("HIVE_HOME", tmp.path().join(".hive"));
        let store = tmp.path().join(".hive").join("teams");
        (tmp, store, env)
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
        // the entry sits in the team's own directory under the store
        assert_eq!(
            entry_path("honey").unwrap(),
            store.join("honey").join("team.json")
        );
        assert_eq!(team_dir("honey").unwrap(), store.join("honey"));
        // no temp files left behind
        let leftovers: Vec<String> = fs::read_dir(store.join("honey"))
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
        fs::create_dir_all(store.join("bad")).unwrap();
        fs::write(store.join("bad").join("team.json"), "{not json").unwrap();

        assert!(load("bad").is_none());
        let listed: HashMap<String, Map<String, Value>> = list_entries()
            .into_iter()
            .map(|e| (e["team"].as_str().unwrap().to_string(), e))
            .collect();
        assert_eq!(listed["bad"].get("corrupt"), Some(&Value::Bool(true)));
    }

    #[test]
    fn test_write_failure_surfaces_as_error() {
        let (_tmp, store, _guard) = store();
        // A directory squatting on the entry path makes the tmp→entry rename fail.
        fs::create_dir_all(store.join("honey").join("team.json")).unwrap();
        assert!(record_team("honey", "/ws", "1.0", &[], "").is_err());
        assert!(load("honey").is_none());
        let leftovers: Vec<_> = fs::read_dir(store.join("honey"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".reg."))
            .collect();
        assert!(leftovers.is_empty(), "tmp file left behind: {leftovers:?}");
    }

    #[test]
    fn test_unsafe_names_cannot_escape_store() {
        let (_tmp, _store, _guard) = store();
        for name in ["../evil", "a/b", "", ".hidden", "a..b"] {
            assert!(entry_path(name).is_none());
            assert!(team_dir(name).is_none());
            assert_eq!(record_team(name, "", "1.0", &[], "").unwrap(), "rejected");
            let row = m(&[("name", "x")]);
            assert_eq!(record_member(name, &row, "").unwrap(), "rejected");
            assert_eq!(reserve_member(name, &row, "").unwrap(), "rejected");
            assert_eq!(remove_member(name, "x", "").unwrap(), "rejected");
            assert_eq!(set_display(name, "@1").unwrap(), "rejected");
            assert_eq!(backfill(name, &[], "", "", "").unwrap(), "rejected");
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
        assert_eq!(record_member("honey", &row, "999.0").unwrap(), "stale");
        assert_eq!(record_member("honey", &row, "123.0").unwrap(), "written");
        assert_eq!(
            member_names(&load("honey").unwrap()),
            names(&["worker", "validator"])
        );
        assert_eq!(
            remove_member("honey", "validator", "999.0").unwrap(),
            "stale"
        );
        assert_eq!(
            remove_member("honey", "validator", "123.0").unwrap(),
            "written"
        );
        assert_eq!(member_names(&load("honey").unwrap()), names(&["worker"]));
    }

    #[test]
    fn test_record_member_tells_a_deleted_team_from_a_recycled_one() {
        let (_tmp, _store, _guard) = store();
        let row = m(&[("name", "worker"), ("cli", "claude")]);
        assert_eq!(record_member("honey", &row, "123.0").unwrap(), "missing");
        assert_eq!(record_member("honey", &row, "").unwrap(), "missing");
        assert!(!entry_path("honey").unwrap().exists());

        record_team("honey", "/ws", "123.0", &[], "").unwrap();
        assert_eq!(record_member("honey", &row, "999.0").unwrap(), "stale");
        assert_eq!(reserve_member("honey", &row, "999.0").unwrap(), "stale");
        assert_eq!(remove_member("honey", "worker", "999.0").unwrap(), "stale");
        assert_eq!(load("honey").unwrap()["members"], Value::Array(vec![]));
        // an unchecked write (no created_at) still lands
        assert_eq!(record_member("honey", &row, "").unwrap(), "written");
    }

    #[test]
    fn test_delete_team_removes_the_entry_and_an_emptied_team_dir() {
        let (_tmp, store, _guard) = store();
        assert_eq!(
            record_team("honey", "/ws", "1.0", &[], "").unwrap(),
            "written"
        );
        delete_team("honey").unwrap();
        assert!(load("honey").is_none());
        assert!(!entry_path("honey").unwrap().is_file());
        // nothing else was in the directory, so it is gone too
        assert!(!store.join("honey").exists());
        // the store and its lock stay
        assert!(store.join(".lock").is_file());
    }

    #[test]
    fn test_delete_team_leaves_the_teams_workspace_files_in_place() {
        let (_tmp, store, _guard) = store();
        let dir = store.join("honey");
        record_team("honey", dir.to_str().unwrap(), "1.0", &[], "").unwrap();
        fs::write(dir.join("hive.db"), "bus").unwrap();
        fs::create_dir_all(dir.join("run")).unwrap();
        delete_team("honey").unwrap();
        assert!(!dir.join("team.json").exists());
        assert!(dir.join("hive.db").is_file());
        assert!(dir.join("run").is_dir());
        // a directory without team.json is not a team
        assert!(load("honey").is_none());
        assert!(list_entries().is_empty());
    }

    #[test]
    fn test_list_entries_enumerates_team_dirs_holding_team_json() {
        let (_tmp, store, _guard) = store();
        record_team("honey", "/ws", "1.0", &[], "").unwrap();
        record_team("comb", "/ws2", "2.0", &[], "").unwrap();
        // a leftover workspace dir, the lock file and an unsafe dir name are
        // not teams
        fs::create_dir_all(store.join("wasp").join("run")).unwrap();
        fs::create_dir_all(store.join(".hidden")).unwrap();
        fs::write(store.join(".hidden").join("team.json"), "{}").unwrap();
        let listed: Vec<String> = list_entries()
            .into_iter()
            .map(|e| e["team"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(listed, vec!["comb".to_string(), "honey".to_string()]);
    }

    #[test]
    fn test_store_lock_lives_beside_the_team_dirs() {
        let (_tmp, store, _guard) = store();
        let lock = locked().unwrap();
        assert!(store.join(".lock").is_file());
        drop(lock);
        assert!(list_entries().is_empty());
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

    #[test]
    fn test_reserve_member_lifecycle() {
        let (_tmp, _store, _guard) = store();
        assert_eq!(
            reserve_member("ghost", &m(&[("name", "impl")]), "").unwrap(),
            "missing"
        );
        record_team("honey", "/ws", "123.0", &[], "@1").unwrap();
        assert_eq!(
            reserve_member("honey", &m(&[("name", "impl"), ("cli", "codex")]), "123.0").unwrap(),
            "reserved"
        );
        assert_eq!(
            reserve_member("honey", &m(&[("name", "impl")]), "123.0").unwrap(),
            "exists"
        );
        // a failed spawn removes the claim; the name is reservable again
        remove_member("honey", "impl", "123.0").unwrap();
        assert_eq!(
            reserve_member("honey", &m(&[("name", "impl")]), "123.0").unwrap(),
            "reserved"
        );
    }

    #[test]
    fn test_reserve_member_admits_exactly_one_concurrent_claimer() {
        let (_tmp, _store, _guard) = store();
        record_team("honey", "/ws", "123.0", &[], "@1").unwrap();
        let barrier = std::sync::Barrier::new(8);
        let outcomes: Vec<&'static str> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        reserve_member("honey", &m(&[("name", "impl")]), "123.0").unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(outcomes.iter().filter(|v| **v == "reserved").count(), 1);
        assert_eq!(outcomes.iter().filter(|v| **v == "exists").count(), 7);
        assert_eq!(member_names(&load("honey").unwrap()), names(&["impl"]));
    }
}
