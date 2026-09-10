//! Team retirement and reclamation: the trash, the cold clock and the
//! collector behind `hive gc` and the tail of every mutating verb.
//!
//! Files are truth and processes are desk. A team nobody displays, whose
//! engines are gone and whose hived owes nothing is *cold*; cold for
//! `COLD_AFTER_SECONDS` it is archived — its directory moved whole to
//! `$HIVE_HOME/trash/<archive-id>/payload/` beside a `manifest.json` — and
//! an archive that is not kept is purged `TRASH_AFTER_SECONDS` later.
//! `hive delete` is the same archive without the wait (`--delete-workspace`
//! purges at once, `--keep-workspace` archives with no purge date). The
//! name is free the moment the entry leaves the registry: the trash
//! reserves nothing, and `hive gc restore` brings an archive back as a new
//! team instance under its old name or another.
//!
//! The collector archives only what it has positively seen idle, and only
//! what it has closed first: an expired team gets a *close intent* on its
//! entry (`gc.closing`, under the store lock), which every hive writer
//! refuses to admit work into (`Team::load`, the registry's write lane);
//! its hived is asked to stop gracefully (a pending node result declines);
//! the team is classified once more; and the archive commits under the
//! lock only if the entry is still the instance the intent was written on.
//! tmux not answering, a ledger call failing, a hived that listens and
//! stays silent, an unreadable or unfinished node record — each blocks the
//! team until it clears. Every manifest transition (quarantine, keep,
//! restore, purge) re-reads the manifest under the same lock before it
//! moves anything. Manifests and entries are written by atomic rename
//! without fsync, like every registry write: a power cut can lose the last
//! transition, and the next run repairs what a manifest then says.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};

use crate::json_fields::{is_set, map_str};

/// A team idle this long is archived.
pub const COLD_AFTER_SECONDS: f64 = 30.0 * 86400.0;
/// An archive not kept is purged this long after it was quarantined.
pub const TRASH_AFTER_SECONDS: f64 = 30.0 * 86400.0;
/// The tail of a mutating verb runs the collector at most this often.
pub const AUTO_INTERVAL_SECONDS: f64 = 86400.0;
/// A close intent older than this is a crashed collector's, not a gate.
pub const CLOSING_TTL_SECONDS: f64 = 120.0;
/// The tail of a verb spends at most this long collecting; what it did not
/// reach waits for the next run.
pub const AUTO_BUDGET_SECONDS: f64 = 20.0;
/// A payload written this long after it was quarantined is in use: its
/// purge date moves out by `TRASH_AFTER_SECONDS` from the write.
const RECENT_WRITE_SLACK_SECONDS: f64 = 60.0;
const EVENTS_MAX_BYTES: u64 = 1 << 20;
const EVENTS_KEEP_LINES: usize = 1000;

const MANIFEST: &str = "manifest.json";
const PAYLOAD: &str = "payload";
const SCHEMA: u64 = 1;

/// The verbs whose success is a use of the team and a chance to collect.
const USE_VERBS: &[&str] = &[
    "create", "join", "spawn", "send", "kill", "delete", "attach", "workflow", "fork",
];

/// The wall clock, for the callers that archive on their own account.
pub(crate) fn epoch_now() -> f64 {
    now()
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `YYYY-MM-DD` (UTC) of an epoch time, for the human-facing lines.
pub(crate) fn date(epoch: f64) -> String {
    crate::clock::utc_iso_seconds(epoch.max(0.0) as u64)
        .chars()
        .take(10)
        .collect()
}

pub(crate) fn trash_dir() -> PathBuf {
    crate::paths::hive_home().join("trash")
}

fn state_dir() -> PathBuf {
    crate::paths::hive_home().join("state").join("gc")
}

fn stamp_path() -> PathBuf {
    state_dir().join("last-attempt")
}

fn events_path() -> PathBuf {
    state_dir().join("events.jsonl")
}

/// The team's workspace: the entry's, else its own directory.
fn workspace_of(entry: &Map<String, Value>) -> String {
    let ws = map_str(entry, "workspace");
    if !ws.is_empty() {
        return crate::paths::expanduser(&ws);
    }
    crate::registry::team_dir(&map_str(entry, "team"))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn created_at_string(entry: &Map<String, Value>) -> String {
    match entry.get("createdAt") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn member_rows(entry: &Map<String, Value>) -> Vec<Map<String, Value>> {
    entry
        .get("members")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(Value::as_object).cloned().collect())
        .unwrap_or_default()
}

fn member_names(entry: &Map<String, Value>) -> Vec<String> {
    member_rows(entry)
        .iter()
        .map(|m| map_str(m, "name"))
        .filter(|n| !n.is_empty())
        .collect()
}

fn gc_object(entry: &Map<String, Value>) -> Map<String, Value> {
    entry
        .get("gc")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    fs::create_dir_all(parent)?;
    let (mut file, tmp) = crate::paths::mkstemp_in(parent, ".gc.", ".tmp")?;
    let result = file
        .write_all(text.as_bytes())
        .and_then(|_| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    Ok(())
}

/// A real directory at *path* — not a symlink to one, which a destructive
/// step must never follow out of the trash.
fn real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir())
}

/// The newest modification time under *path* (symlinks not followed);
/// Err when any of it cannot be read — a payload the collector cannot see
/// whole is not one it may purge.
fn newest_mtime(path: &Path) -> Result<Option<f64>, String> {
    let mut newest: Option<f64> = None;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in read {
            let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
            let meta = fs::symlink_metadata(entry.path())
                .map_err(|e| format!("{}: {e}", entry.path().display()))?;
            if let Ok(modified) = meta.modified() {
                let secs = modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                newest = Some(newest.map_or(secs, |n: f64| n.max(secs)));
            }
            if meta.file_type().is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(newest)
}

/// The trash root, refused when it is a symlink: no destructive step
/// follows a link out of the hive home.
fn trash_root() -> Result<PathBuf> {
    let dir = trash_dir();
    if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("{} is a symlink; the trash is left alone", dir.display());
    }
    Ok(dir)
}

// ---------------------------------------------------------------------------
// archives
// ---------------------------------------------------------------------------

/// One trash entry, as its manifest records it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Archive {
    pub id: String,
    pub team: String,
    /// The archived instance's `createdAt`, as the registry stored it.
    pub created_at: String,
    /// The external workspace the entry recorded; empty for the default,
    /// which travels inside the payload.
    pub workspace: String,
    /// `delete` or `expired`.
    pub origin: String,
    /// `preparing` (manifest written, directory not yet moved),
    /// `quarantined`, `restoring` (a restore under way; `restore_as` names
    /// its target), `purging`.
    pub state: String,
    pub quarantined_at: f64,
    /// None keeps the archive.
    pub purge_after: Option<f64>,
    pub members: Vec<String>,
    pub restore_as: String,
}

impl Archive {
    fn dir(&self) -> PathBuf {
        trash_dir().join(&self.id)
    }

    fn payload(&self) -> PathBuf {
        self.dir().join(PAYLOAD)
    }

    pub(crate) fn kept(&self) -> bool {
        self.purge_after.is_none()
    }

    fn to_value(&self) -> Value {
        let mut doc = json!({
            "schema": SCHEMA,
            "archiveId": self.id,
            "team": self.team,
            "createdAt": self.created_at,
            "workspace": self.workspace,
            "origin": self.origin,
            "state": self.state,
            "quarantinedAt": self.quarantined_at,
            "purgeAfter": self.purge_after,
            "members": self.members,
        });
        if !self.restore_as.is_empty() {
            doc["restoreAs"] = Value::from(self.restore_as.as_str());
        }
        doc
    }

    fn from_value(doc: &Value) -> Option<Archive> {
        let doc = doc.as_object()?;
        if doc.get("schema").and_then(Value::as_u64) != Some(SCHEMA) {
            return None;
        }
        let text = |key: &str| map_str(doc, key);
        let id = text("archiveId");
        if !archive_id_ok(&id) {
            return None;
        }
        Some(Archive {
            id,
            team: text("team"),
            created_at: text("createdAt"),
            workspace: text("workspace"),
            origin: text("origin"),
            state: text("state"),
            quarantined_at: doc
                .get("quarantinedAt")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            purge_after: doc.get("purgeAfter").and_then(Value::as_f64),
            members: doc
                .get("members")
                .and_then(Value::as_array)
                .map(|m| {
                    m.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            restore_as: text("restoreAs"),
        })
    }

    fn write(&self) -> Result<()> {
        let mut text = serde_json::to_string_pretty(&self.to_value())?;
        text.push('\n');
        write_atomic(&self.dir().join(MANIFEST), &text)
    }
}

fn archive_id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn read_archive(dir: &Path) -> Option<Archive> {
    if !real_dir(dir) {
        return None;
    }
    let manifest = dir.join(MANIFEST);
    if fs::symlink_metadata(&manifest).is_ok_and(|m| m.file_type().is_symlink()) {
        return None;
    }
    let text = fs::read_to_string(&manifest).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    let archive = Archive::from_value(&doc)?;
    (dir.file_name().and_then(|n| n.to_str()) == Some(archive.id.as_str())).then_some(archive)
}

/// Every readable archive, oldest first, and the names of the trash
/// entries that are not one (a symlink, a directory without a readable
/// manifest): reported, never touched. Err for a trash root that is
/// itself a symlink.
fn list_trash() -> Result<(Vec<Archive>, Vec<String>)> {
    let Ok(read) = fs::read_dir(trash_root()?) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let mut archives = Vec::new();
    let mut corrupt = Vec::new();
    for entry in read.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        match read_archive(&path) {
            Some(archive) => archives.push(archive),
            None => corrupt.push(name),
        }
    }
    archives.sort_by(|a, b| {
        a.quarantined_at
            .partial_cmp(&b.quarantined_at)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    corrupt.sort();
    Ok((archives, corrupt))
}

/// Every readable archive, oldest first.
#[cfg(test)]
pub(crate) fn list_archives() -> Vec<Archive> {
    list_trash().map(|trash| trash.0).unwrap_or_default()
}

pub(crate) fn archive(id: &str) -> Option<Archive> {
    if !archive_id_ok(id) {
        return None;
    }
    read_archive(&trash_root().ok()?.join(id))
}

fn new_archive_id() -> String {
    crate::naming::os_random_bytes(6)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Append one event line; a log past `EVENTS_MAX_BYTES` keeps its last
/// `EVENTS_KEEP_LINES` lines — the manifests are the record, this is the
/// narrative.
fn append_event(event: &str, archive: &Archive, extra: &[(&str, Value)]) {
    let mut row = Map::new();
    row.insert("event".to_string(), Value::from(event));
    row.insert("archiveId".to_string(), Value::from(archive.id.as_str()));
    row.insert("team".to_string(), Value::from(archive.team.as_str()));
    row.insert(
        "createdAt".to_string(),
        Value::from(archive.created_at.as_str()),
    );
    row.insert("at".to_string(), Value::from(now()));
    for (key, value) in extra {
        row.insert(key.to_string(), value.clone());
    }
    let path = events_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::metadata(&path).is_ok_and(|m| m.len() > EVENTS_MAX_BYTES) {
        if let Ok(text) = fs::read_to_string(&path) {
            let lines: Vec<&str> = text.lines().collect();
            let keep = lines.len().saturating_sub(EVENTS_KEEP_LINES);
            let mut trimmed = lines[keep..].join("\n");
            trimmed.push('\n');
            let _ = write_atomic(&path, &trimmed);
        }
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{}", Value::Object(row));
    }
}

/// What an entry must still be for a collector's archive to commit: the
/// instance it classified, the cold clock it saw expired, and the close
/// intent it wrote, still fresh.
pub(crate) struct Expected {
    pub created_at: String,
    pub cold_since: f64,
    pub closing_by: String,
}

/// Move *team* out of the registry into the trash: its directory becomes
/// the archive's payload (the entry with it — that is what ends the team),
/// an external workspace is only recorded, never moved. The name is free
/// on return. *keep* gives the archive no purge date, else it is *at* plus
/// `TRASH_AFTER_SECONDS`. With *expected*, the entry must still be that
/// instance, its cold clock unchanged and still expired at *at*, under
/// that close intent still fresh, and not kept, or nothing moves — a use
/// between the scan and the commit (a renewed clock) keeps the team. The
/// caller has stopped what ran for the team; nothing here does.
pub(crate) fn archive_team(
    team: &str,
    origin: &str,
    keep: bool,
    at: f64,
    expected: Option<&Expected>,
) -> Result<Archive> {
    let dir = crate::registry::team_dir(team).ok_or_else(|| anyhow!("unsafe team name"))?;
    trash_root()?;
    let _lock = crate::registry::locked()?;
    let entry = crate::registry::load(team).ok_or_else(|| anyhow!("team '{team}' not found"))?;
    if let Some(expected) = expected {
        let same_instance = created_at_string(&entry) == expected.created_at;
        let same_clock = cold_since(&entry) == Some(expected.cold_since)
            && at - expected.cold_since >= COLD_AFTER_SECONDS;
        let our_intent = is_closing(&entry, at)
            && closing_of(&entry).is_some_and(|(_, by)| by == expected.closing_by);
        if !same_instance || !same_clock || !our_intent || is_kept(&entry) {
            bail!("team '{team}' changed while the collector was closing it; left alone");
        }
    }
    let ws = map_str(&entry, "workspace");
    let external = if ws.is_empty() || Path::new(&crate::paths::expanduser(&ws)) == dir.as_path() {
        String::new()
    } else {
        ws
    };
    let mut archive = Archive {
        id: new_archive_id(),
        team: team.to_string(),
        created_at: created_at_string(&entry),
        workspace: external,
        origin: origin.to_string(),
        state: "preparing".to_string(),
        quarantined_at: at,
        purge_after: (!keep).then_some(at + TRASH_AFTER_SECONDS),
        members: member_names(&entry),
        restore_as: String::new(),
    };
    fs::create_dir_all(archive.dir())?;
    archive.write()?;
    if let Err(e) = fs::rename(&dir, archive.payload()) {
        let _ = fs::remove_dir_all(archive.dir());
        return Err(anyhow!("cannot move {} into the trash: {e}", dir.display()));
    }
    archive.state = "quarantined".to_string();
    archive.write()?;
    append_event("quarantined", &archive, &[("origin", Value::from(origin))]);
    Ok(archive)
}

/// What one purge attempt did.
#[derive(Debug, PartialEq)]
pub(crate) enum Purge {
    Purged,
    /// Something wrote into the payload after it was quarantined: the
    /// purge date moved to the new time.
    Deferred(f64),
    /// The manifest no longer says what the caller saw.
    Skipped(&'static str),
}

/// Remove the archive for good, from what the manifest says now: under
/// the store lock it must be `quarantined` past its purge date (or a
/// `purging` left by a crash, finished here); the manifest says `purging`
/// before anything goes, so a crash mid-way is finished by the next run
/// and never restored. A payload written since it was quarantined is in
/// use: deferred, not purged.
pub(crate) fn purge_archive(id: &str, now: f64) -> Result<Purge> {
    let archive = {
        trash_root()?;
        let _lock = crate::registry::locked()?;
        let Some(mut archive) = self::archive(id) else {
            return Ok(Purge::Skipped("no such archive"));
        };
        match archive.state.as_str() {
            "purging" => {}
            "quarantined" => {
                if archive.purge_after.is_none_or(|due| due > now) {
                    return Ok(Purge::Skipped("not due"));
                }
                // A payload written into after it was quarantined earns the
                // full trash window from its last write, whenever that fell.
                let newest = newest_mtime(&archive.payload())
                    .map_err(|e| anyhow!("archive {id}: payload unreadable ({e}); left alone"))?
                    .unwrap_or(0.0);
                if newest > archive.quarantined_at + RECENT_WRITE_SLACK_SECONDS
                    && newest + TRASH_AFTER_SECONDS > now
                {
                    let until = newest + TRASH_AFTER_SECONDS;
                    archive.purge_after = Some(until);
                    archive.write()?;
                    append_event("deferred", &archive, &[("writtenAt", Value::from(newest))]);
                    return Ok(Purge::Deferred(until));
                }
                archive.state = "purging".to_string();
                archive.write()?;
            }
            _ => return Ok(Purge::Skipped("not quarantined")),
        }
        archive
    };
    let payload = archive.payload();
    if fs::symlink_metadata(&payload).is_ok() {
        if !real_dir(&payload) {
            bail!("archive {id}: payload is not a directory; left alone");
        }
        fs::remove_dir_all(&payload)?;
    }
    let _lock = crate::registry::locked()?;
    if real_dir(&archive.dir()) {
        fs::remove_dir_all(archive.dir())?;
    }
    append_event("purged", &archive, &[]);
    Ok(Purge::Purged)
}

/// An archive a crash left mid-transition, under the store lock: a
/// `preparing` whose directory moved is committed (its clock restarted
/// from now — the interrupted transaction earns the full window), one
/// whose directory never moved is dropped; a `restoring` whose target
/// published is finished, one whose payload is still here goes back to
/// `quarantined`.
fn repair(id: &str, now: f64) -> Result<&'static str> {
    let _lock = crate::registry::locked()?;
    let Some(mut archive) = archive(id) else {
        return Ok("gone");
    };
    match archive.state.as_str() {
        "preparing" => {
            if real_dir(&archive.payload()) {
                archive.state = "quarantined".to_string();
                archive.quarantined_at = now;
                if !archive.kept() {
                    archive.purge_after = Some(now + TRASH_AFTER_SECONDS);
                }
                archive.write()?;
                append_event("quarantined", &archive, &[("repaired", Value::Bool(true))]);
                Ok("quarantined")
            } else {
                fs::remove_dir_all(archive.dir())?;
                Ok("dropped")
            }
        }
        "restoring" => {
            let published = crate::registry::team_dir(&archive.restore_as)
                .and_then(|dir| crate::registry::load_at(&dir.join("team.json")))
                .is_some_and(|entry| entry["restoredFrom"]["archiveId"] == archive.id.as_str());
            if published {
                fs::remove_dir_all(archive.dir())?;
                append_event(
                    "restored",
                    &archive,
                    &[
                        ("as", Value::from(archive.restore_as.as_str())),
                        ("repaired", Value::Bool(true)),
                    ],
                );
                Ok("restored")
            } else if real_dir(&archive.payload()) {
                archive.state = "quarantined".to_string();
                archive.restore_as = String::new();
                archive.write()?;
                Ok("quarantined")
            } else {
                fs::remove_dir_all(archive.dir())?;
                Ok("dropped")
            }
        }
        _ => Ok("unchanged"),
    }
}

/// What `restore_archive` did.
#[derive(Debug)]
pub(crate) struct Restored {
    pub team: String,
    pub dir: PathBuf,
    pub archive: Archive,
}

/// Bring an archive back as a new team instance — a new `createdAt`, so
/// a callback the old instance left behind never lands on it — under its
/// old name or *as_name*. Refused when the name is in use (a live team, or
/// a leftover directory) or a member's engine session is bound to a live
/// team: nothing is taken from anyone. Under the store lock throughout:
/// the manifest is re-read and must be `quarantined`; it says `restoring`
/// and the new entry is written into the payload before the directory
/// moves, so a failure publishes nothing (`repair` finishes or reverts
/// what a crash leaves). Data only: no engine starts, the display is built
/// by the next attach. The arrangement the archive kept follows the team
/// to its new instance.
pub(crate) fn restore_archive(id: &str, as_name: Option<&str>) -> Result<Restored> {
    let _lock = crate::registry::locked()?;
    let mut archive =
        archive(id).ok_or_else(|| anyhow!("no archive '{id}' (see `hive gc run --dry-run`)"))?;
    if archive.state != "quarantined" {
        bail!("archive {id} is {}; not restorable", archive.state);
    }
    let name = as_name.filter(|n| !n.is_empty()).unwrap_or(&archive.team);
    let error = crate::team::validate_team_name(name);
    if !error.is_empty() {
        bail!("cannot restore as '{name}': {error}");
    }
    let target = crate::registry::team_dir(name).ok_or_else(|| anyhow!("unsafe team name"))?;
    let payload = archive.payload();
    if !real_dir(&payload) {
        bail!("archive {id} has no payload directory");
    }
    let entry_path = payload.join("team.json");
    let text = fs::read_to_string(&entry_path)
        .map_err(|e| anyhow!("archive {id} has no readable team.json: {e}"))?;
    let Value::Object(mut entry) = serde_json::from_str::<Value>(&text)? else {
        bail!("archive {id}: team.json is not an object");
    };
    if fs::symlink_metadata(&target).is_ok() {
        bail!(
            "name '{name}' is in use ({}); pass --as <name>",
            if crate::registry::load(name).is_some() {
                "a live team"
            } else {
                "a directory in the store"
            }
        );
    }
    for member in member_rows(&entry) {
        let sid = map_str(&member, "sessionId");
        if sid.is_empty() {
            continue;
        }
        let cli = map_str(&member, "cli");
        if let Some((other_team, other)) =
            crate::registry::member_for_session(&sid, Some(cli.as_str()))
        {
            bail!(
                "member '{}' rides session {sid}, which is bound to {other_team}.{other}; \
                 restore refuses to take it",
                map_str(&member, "name")
            );
        }
    }
    let original_entry = entry.clone();
    let at = now();
    // The archived instance as the manifest has it: a retry after a failed
    // move must not mistake an entry a previous attempt rewrote for it.
    let old_instance = archive.created_at.clone();
    let mut restored_from = Map::new();
    restored_from.insert("archiveId".to_string(), Value::from(archive.id.as_str()));
    restored_from.insert("team".to_string(), Value::from(archive.team.as_str()));
    restored_from.insert(
        "createdAt".to_string(),
        Value::from(archive.created_at.as_str()),
    );
    restored_from.insert("restoredAt".to_string(), Value::from(at));
    let new_instance = format!("{at}");
    entry.insert("team".to_string(), Value::from(name));
    entry.insert("createdAt".to_string(), Value::from(new_instance.as_str()));
    entry.insert("display".to_string(), Value::from(""));
    if archive.workspace.is_empty() {
        entry.insert(
            "workspace".to_string(),
            Value::from(target.to_string_lossy().into_owned()),
        );
    }
    entry.insert("gc".to_string(), json!({"keep": false, "coldSince": null}));
    entry.insert("restoredFrom".to_string(), Value::Object(restored_from));
    // The intent, then the new entry inside the payload, then the move:
    // a failure anywhere before the move publishes nothing.
    archive.state = "restoring".to_string();
    archive.restore_as = name.to_string();
    archive.write()?;
    let revert = |archive: &mut Archive| {
        archive.state = "quarantined".to_string();
        archive.restore_as = String::new();
        let _ = archive.write();
    };
    if let Err(e) = crate::registry::write_entry_file(&entry_path, &entry) {
        revert(&mut archive);
        return Err(anyhow!("cannot write the restored entry: {e}"));
    }
    if let Err(e) = fs::rename(&payload, &target) {
        // A failed rollback must leave a complete entry for the next retry.
        let _ = crate::registry::write_entry_file(&entry_path, &original_entry);
        revert(&mut archive);
        return Err(anyhow!(
            "cannot move the archive back to {}: {e}",
            target.display()
        ));
    }
    let workspace = if archive.workspace.is_empty() {
        target.to_string_lossy().into_owned()
    } else {
        archive.workspace.clone()
    };
    crate::layout::rebind_arrangement(
        &workspace,
        (&archive.team, &old_instance),
        (name, &new_instance),
    );
    let _ = fs::remove_dir_all(archive.dir());
    append_event("restored", &archive, &[("as", Value::from(name))]);
    Ok(Restored {
        team: name.to_string(),
        dir: target,
        archive,
    })
}

/// `hive gc keep TARGET [--off]`: a live team is exempt from the cold
/// clock (and its clock cleared when the exemption lifts); an archive
/// loses its purge date, or gets one `TRASH_AFTER_SECONDS` from now.
pub(crate) fn set_keep(target: &str, on: bool) -> Result<String> {
    if crate::registry::load(target).is_some() {
        let written = crate::registry::update_entry(target, |entry| {
            let mut gc = gc_object(entry);
            gc.insert("keep".to_string(), Value::Bool(on));
            if !on {
                gc.insert("coldSince".to_string(), Value::Null);
            }
            entry.insert("gc".to_string(), Value::Object(gc));
            true
        })?;
        if !written {
            bail!("team '{target}' vanished while writing");
        }
        return Ok(format!(
            "team '{target}': keep {}",
            if on {
                "on"
            } else {
                "off (the cold clock starts over)"
            }
        ));
    }
    let _lock = crate::registry::locked()?;
    let Some(mut archive) = archive(target) else {
        bail!("no team or archive named '{target}' (see `hive ls`, `hive gc run --dry-run`)");
    };
    if archive.state != "quarantined" {
        bail!("archive {target} is {}", archive.state);
    }
    archive.purge_after = (!on).then_some(now() + TRASH_AFTER_SECONDS);
    archive.write()?;
    append_event("kept", &archive, &[("keep", Value::Bool(on))]);
    Ok(match archive.purge_after {
        None => format!("archive {target} ({}): kept, no purge date", archive.team),
        Some(at) => format!(
            "archive {target} ({}): purge after {}",
            archive.team,
            date(at)
        ),
    })
}

// ---------------------------------------------------------------------------
// the close intent
// ---------------------------------------------------------------------------

/// `gc.closing` on an entry: `(at, by)`.
fn closing_of(entry: &Map<String, Value>) -> Option<(f64, String)> {
    let closing = gc_object(entry).get("closing")?.as_object()?.clone();
    Some((
        closing.get("at").and_then(Value::as_f64)?,
        map_str(&closing, "by"),
    ))
}

/// Whether a collector is closing this team right now: a fresh
/// `gc.closing`. Every hive writer refuses to admit work into such a team
/// (`Team::load`, the registry's write lane); an intent older than
/// `CLOSING_TTL_SECONDS` is a crashed collector's and gates nothing.
pub(crate) fn is_closing(entry: &Map<String, Value>, now: f64) -> bool {
    closing_of(entry).is_some_and(|(at, _)| now - at < CLOSING_TTL_SECONDS && at - now < 1.0)
}

/// Write the close intent under the store lock, on the instance the
/// collector classified with the cold clock it saw, still expired at
/// *now*, unless the team is kept or already closing. A clock renewed by a
/// use since the scan is a different team from the one classified.
fn open_close_intent(
    team: &str,
    expected_created: &str,
    expected_since: f64,
    by: &str,
    now: f64,
) -> Result<bool> {
    crate::registry::update_entry(team, |entry| {
        if created_at_string(entry) != expected_created
            || cold_since(entry) != Some(expected_since)
            || now - expected_since < COLD_AFTER_SECONDS
            || is_kept(entry)
            || is_closing(entry, now)
        {
            return false;
        }
        let mut gc = gc_object(entry);
        gc.insert("closing".to_string(), json!({"at": now, "by": by}));
        entry.insert("gc".to_string(), Value::Object(gc));
        true
    })
}

/// Take back a close intent this collector wrote, and the cold clock with
/// it: whatever stopped the archive (activity, missing evidence, a change
/// under the collector) is not idle time.
fn drop_close_intent(team: &str, by: &str) {
    let _ = crate::registry::update_entry(team, |entry| {
        match closing_of(entry) {
            Some((_, owner)) if owner == by => {}
            _ => return false,
        }
        let mut gc = gc_object(entry);
        gc.remove("closing");
        gc.insert("coldSince".to_string(), Value::Null);
        entry.insert("gc".to_string(), Value::Object(gc));
        true
    });
}

// ---------------------------------------------------------------------------
// activity
// ---------------------------------------------------------------------------

/// What the collector saw of a team.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Activity {
    Active(String),
    Inactive,
    /// Evidence missing, never guessed around: the team is left alone.
    Unknown(String),
}

/// What `hive delete` asks before ending a team without `--down`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Turns {
    Idle,
    /// The members mid-turn.
    Open(Vec<String>),
    Unverified(String),
}

/// Teams with a window, or why tmux could not say. Same listing shape as
/// `team::find_team_window`.
fn displayed_teams() -> Result<HashSet<String>, &'static str> {
    let fmt = format!(
        "#{{session_name}}:#{{window_index}}\t#{{window_id}}\t{}\t#{{@hive-workspace}}\t#{{@hive-desc}}\t#{{@hive-created}}",
        crate::tmux::WINDOW_TEAM_FMT
    );
    let run = crate::tmux::run(&["list-windows", "-a", "-F", &fmt], false, 5)
        .map_err(|_| "tmux did not answer")?;
    if run.returncode != 0 {
        if crate::tmux::stderr_means_no_server(&run.stderr) {
            return Ok(HashSet::new());
        }
        return Err("tmux did not answer");
    }
    Ok(run
        .stdout
        .lines()
        .filter_map(|line| line.split('\t').nth(2))
        .filter(|team| !team.is_empty())
        .map(str::to_string)
        .collect())
}

/// A node operation the hived journaled and never brought to a terminal
/// state (`run/operations/<incarnation>/<dispatchId>.json`): Ok(Some) with
/// its id, Err when a record or the journal cannot be read — which is not
/// "no obligation".
fn unfinished_operation(workspace: &str) -> Result<Option<String>, String> {
    let root = Path::new(workspace).join("run").join("operations");
    let incarnations = match fs::read_dir(&root) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("operations journal unreadable: {e}")),
    };
    for incarnation in incarnations {
        let incarnation = incarnation.map_err(|e| format!("operations journal unreadable: {e}"))?;
        if !incarnation.path().is_dir() {
            continue;
        }
        let records = fs::read_dir(incarnation.path())
            .map_err(|e| format!("operations journal unreadable: {e}"))?;
        for record in records {
            let record = record.map_err(|e| format!("operations journal unreadable: {e}"))?;
            let name = record.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !name.ends_with(".json") {
                continue; // a writer's temp file, not a record
            }
            let text = fs::read_to_string(record.path())
                .map_err(|e| format!("operation record {name} unreadable: {e}"))?;
            let doc: Value = serde_json::from_str(&text)
                .map_err(|_| format!("operation record {name} is corrupt"))?;
            if doc["state"] != "terminal" {
                return Ok(Some(
                    doc["dispatchId"]
                        .as_str()
                        .unwrap_or(name.trim_end_matches(".json"))
                        .to_string(),
                ));
            }
        }
    }
    Ok(None)
}

/// A test's stand-in for the hived's `team-runtime` answer.
#[cfg(test)]
pub(crate) fn fake_hived_runtime() -> &'static Mutex<Option<Map<String, Value>>> {
    static CELL: OnceLock<Mutex<Option<Map<String, Value>>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// The runtime the team's hived reports, when there is a hived: Ok(None)
/// with no socket, or a socket file nobody listens on (a hived that died
/// without cleaning up); Err when one listens and does not answer, or the
/// socket cannot be reached for a reason that is not "nobody there".
fn hived_runtime(workspace: &str, team: &str) -> Result<Option<Map<String, Value>>, String> {
    #[cfg(test)]
    if let Some(runtime) = fake_hived_runtime().lock().ok().and_then(|r| r.clone()) {
        return Ok(Some(runtime));
    }
    if !crate::hived::socket_path(workspace).exists() {
        return Ok(None);
    }
    match crate::hived::request_team_runtime_answer(workspace, team) {
        Ok(runtime) => Ok(Some(runtime)),
        Err(crate::hived::RequestFailure::NoListener) => Ok(None),
        Err(crate::hived::RequestFailure::NotSent(reason)) => {
            Err(format!("hived socket unreachable ({reason})"))
        }
        Err(crate::hived::RequestFailure::AnswerLost(reason)) => {
            Err(format!("hived listens but did not answer ({reason})"))
        }
    }
}

fn runtime_members(runtime: &Map<String, Value>) -> Vec<(String, &Map<String, Value>)> {
    runtime
        .get("members")
        .and_then(Value::as_object)
        .map(|members| {
            members
                .iter()
                .filter_map(|(name, fields)| fields.as_object().map(|f| (name.clone(), f)))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a turn is open on the codex thread, asked of the shared daemon:
/// Ok(None) when the daemon is not there (no turn can be open), Err when
/// it is and will not say.
///
/// `thread/read` is read-only and covers the waiting states too: codex
/// reports `waitingOnApproval` / `waitingOnUserInput` only as flags of an
/// *active* thread (`codex_app_server::client::apply_status`), a turn
/// still in progress, which is what `turn_open_for_thread` reads. A thread
/// with no turn in progress has nobody waiting on it.
fn codex_turn_open(thread_id: &str) -> Result<Option<bool>, String> {
    if !crate::adapters::codex_app_server::daemon_alive() {
        return Ok(None);
    }
    if !crate::adapters::codex_app_server::connect() {
        return Err("codex daemon alive but unreachable".to_string());
    }
    crate::adapters::codex_app_server::turn_open_for_thread(thread_id)
        .map(Some)
        .ok_or_else(|| "codex daemon did not answer".to_string())
}

/// Whether a grok leader listens on *socket*: Ok(false) when the path is
/// gone or refuses (a leader that died), Err for any other failure (a
/// timeout, a permission error — a socket the collector cannot judge).
fn grok_leader_listens(socket: &Path) -> Result<bool, String> {
    if !socket.exists() {
        return Ok(false);
    }
    match crate::adapters::grok_leader::probe_connect(socket) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("grok leader socket {}: {e}", socket.display())),
    }
}

/// Everything one run of the collector reads once and shares: the tmux
/// window listing, the claude job ledger and the live claude sessions
/// (each one call, only when a team without a display has a claude
/// member).
struct Observations {
    displayed: Result<HashSet<String>, &'static str>,
    claude_jobs: OnceLock<Option<Vec<Map<String, Value>>>>,
    claude_sessions: OnceLock<Vec<crate::adapters::claude_sessions::ClaudeSession>>,
}

impl Observations {
    fn gather() -> Observations {
        Observations {
            displayed: displayed_teams(),
            claude_jobs: OnceLock::new(),
            claude_sessions: OnceLock::new(),
        }
    }

    fn claude_jobs(&self) -> Option<&Vec<Map<String, Value>>> {
        self.claude_jobs
            .get_or_init(|| crate::adapters::claude_bg::jobs_ledger("claude"))
            .as_ref()
    }

    fn claude_sessions(&self) -> &Vec<crate::adapters::claude_sessions::ClaudeSession> {
        self.claude_sessions
            .get_or_init(crate::adapters::claude_sessions::list_sessions)
    }

    /// Where the team stands. Display counts as use; a hived that answers
    /// speaks for its members; without one, each member's engine is asked
    /// in its own way — the claude job ledger and the live claude sessions
    /// (a desktop conversation's CLI is one), the codex daemon's turn state
    /// for the thread, the grok leader's socket. A member whose engine
    /// cannot be asked is unknown, not idle.
    fn classify(&self, entry: &Map<String, Value>) -> Activity {
        let team = map_str(entry, "team");
        match &self.displayed {
            Err(reason) => return Activity::Unknown((*reason).to_string()),
            Ok(displayed) if displayed.contains(&team) => {
                return Activity::Active("displayed".to_string())
            }
            Ok(_) => {}
        }
        let workspace = workspace_of(entry);
        match hived_runtime(&workspace, &team) {
            Err(reason) => return Activity::Unknown(reason),
            Ok(Some(runtime)) => {
                for (name, fields) in runtime_members(&runtime) {
                    if fields.get("busy") == Some(&Value::Bool(true)) {
                        return Activity::Active(format!("{name} is mid-turn"));
                    }
                    if fields.get("alive") == Some(&Value::Bool(true)) {
                        return Activity::Active(format!("{name}'s engine is alive"));
                    }
                }
            }
            Ok(None) => {}
        }
        match unfinished_operation(&workspace) {
            Err(reason) => return Activity::Unknown(reason),
            Ok(Some(id)) => return Activity::Unknown(format!("unfinished operation {id}")),
            Ok(None) => {}
        }
        for member in member_rows(entry) {
            let name = map_str(&member, "name");
            let sid = map_str(&member, "sessionId");
            if sid.is_empty() {
                continue; // no engine was ever bound to this row
            }
            match map_str(&member, "cli").as_str() {
                "claude" => {
                    let Some(jobs) = self.claude_jobs() else {
                        return Activity::Unknown("claude job ledger unavailable".to_string());
                    };
                    if jobs
                        .iter()
                        .any(|row| map_str(row, "id") == sid && is_set(row.get("pid")))
                    {
                        return Activity::Active(format!("{name}'s claude job is running"));
                    }
                    if self
                        .claude_sessions()
                        .iter()
                        .any(|session| session.session_id == sid)
                    {
                        return Activity::Active(format!("{name}'s claude session is live"));
                    }
                    let host = map_str(&member, "hostSessionId");
                    if !host.is_empty() {
                        use crate::adapters::claude_desktop::{
                            desktop_record, record_presence, RecordPresence,
                        };
                        match record_presence(&host) {
                            RecordPresence::Unknown => {
                                return Activity::Unknown(format!(
                                    "{name}'s desktop conversation cannot be read"
                                ))
                            }
                            // The conversation may run a CLI session the
                            // roster does not name yet (`succession` moves
                            // the row later): that session live is this
                            // member live.
                            RecordPresence::Present => {
                                if let Some(record) = desktop_record(&host) {
                                    let current = record.cli_session_id;
                                    if current != sid
                                        && self
                                            .claude_sessions()
                                            .iter()
                                            .any(|session| session.session_id == current)
                                    {
                                        return Activity::Active(format!(
                                            "{name}'s desktop conversation runs a live session"
                                        ));
                                    }
                                }
                            }
                            RecordPresence::Absent => {}
                        }
                    }
                }
                "codex" => match codex_turn_open(&sid) {
                    Err(reason) => return Activity::Unknown(format!("{name}: {reason}")),
                    Ok(Some(true)) => return Activity::Active(format!("{name} has a turn open")),
                    Ok(_) => {}
                },
                "grok" => {
                    let socket = crate::adapters::grok_leader::socket_path_for_key(&format!(
                        "m-{team}.{name}"
                    ));
                    match grok_leader_listens(&socket) {
                        Err(reason) => return Activity::Unknown(format!("{name}: {reason}")),
                        Ok(true) => {
                            return Activity::Active(format!("{name}'s grok leader is alive"))
                        }
                        Ok(false) => {}
                    }
                }
                other => {
                    return Activity::Unknown(format!(
                        "{name} rides a '{other}' engine the collector cannot ask"
                    ))
                }
            }
        }
        Activity::Inactive
    }
}

/// Members mid-turn, for `hive delete` without `--down`: the hived's word
/// when there is one, the codex daemon's for codex members otherwise.
pub(crate) fn busy_members(entry: &Map<String, Value>) -> Turns {
    let team = map_str(entry, "team");
    let workspace = workspace_of(entry);
    match hived_runtime(&workspace, &team) {
        Err(reason) => return Turns::Unverified(reason),
        Ok(Some(runtime)) => {
            let busy: Vec<String> = runtime_members(&runtime)
                .into_iter()
                .filter(|(_, fields)| fields.get("busy") == Some(&Value::Bool(true)))
                .map(|(name, _)| name)
                .collect();
            return if busy.is_empty() {
                Turns::Idle
            } else {
                Turns::Open(busy)
            };
        }
        Ok(None) => {}
    }
    let mut busy = Vec::new();
    for member in member_rows(entry) {
        let sid = map_str(&member, "sessionId");
        if sid.is_empty() || map_str(&member, "cli") != "codex" {
            continue;
        }
        match codex_turn_open(&sid) {
            Err(reason) => {
                return Turns::Unverified(format!("{}: {reason}", map_str(&member, "name")))
            }
            Ok(Some(true)) => busy.push(map_str(&member, "name")),
            Ok(_) => {}
        }
    }
    if busy.is_empty() {
        Turns::Idle
    } else {
        Turns::Open(busy)
    }
}

// ---------------------------------------------------------------------------
// the collector
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Report only: no file is written, no clock started.
    DryRun,
    /// `hive gc run`.
    Manual,
    /// The tail of a mutating verb: at most once per `AUTO_INTERVAL_SECONDS`,
    /// within `AUTO_BUDGET_SECONDS`.
    Auto,
}

/// One run's findings: a row per registry team and per archive, and the
/// lines for what was done or could not be.
#[derive(Debug, Default)]
pub(crate) struct Report {
    pub throttled: bool,
    pub teams: Vec<Value>,
    pub archives: Vec<Value>,
    pub actions: Vec<String>,
}

impl Report {
    /// Whether any row ended in an error.
    pub(crate) fn failed(&self) -> bool {
        self.teams
            .iter()
            .chain(self.archives.iter())
            .any(|row| row["state"] == "error")
    }
}

fn cold_since(entry: &Map<String, Value>) -> Option<f64> {
    gc_object(entry).get("coldSince").and_then(Value::as_f64)
}

fn is_kept(entry: &Map<String, Value>) -> bool {
    gc_object(entry).get("keep").and_then(Value::as_bool) == Some(true)
}

fn set_cold_since(team: &str, at: Option<f64>) -> Result<()> {
    crate::registry::update_entry(team, |entry| {
        let mut gc = gc_object(entry);
        gc.entry("keep".to_string()).or_insert(Value::Bool(false));
        gc.insert(
            "coldSince".to_string(),
            at.map(Value::from).unwrap_or(Value::Null),
        );
        entry.insert("gc".to_string(), Value::Object(gc));
        true
    })?;
    Ok(())
}

/// A use of the team (any mutating verb that resolved it): the cold clock
/// stops. Cheap when it was not running.
pub(crate) fn clear_cold_since(team: &str) -> Result<()> {
    match crate::registry::load(team) {
        Some(entry) if cold_since(&entry).is_some() => set_cold_since(team, None),
        _ => Ok(()),
    }
}

fn stamp_due(now: f64) -> bool {
    match fs::read_to_string(stamp_path()) {
        Ok(text) => text
            .trim()
            .parse::<f64>()
            .map(|last| now - last >= AUTO_INTERVAL_SECONDS || now < last)
            .unwrap_or(true),
        Err(_) => true,
    }
}

fn write_stamp(now: f64) -> Result<()> {
    write_atomic(&stamp_path(), &format!("{now}\n"))
}

fn team_row(team: &str, state: &str) -> Map<String, Value> {
    let mut row = Map::new();
    row.insert("team".to_string(), Value::from(team));
    row.insert("state".to_string(), Value::from(state));
    row
}

fn with_reason(mut row: Map<String, Value>, reason: String) -> Map<String, Value> {
    row.insert("reason".to_string(), Value::from(reason));
    row
}

fn cooling_row(team: &str, since: f64) -> Map<String, Value> {
    let mut row = team_row(team, "cooling");
    row.insert("coldSince".to_string(), Value::from(since));
    row.insert(
        "archiveAfter".to_string(),
        Value::from(since + COLD_AFTER_SECONDS),
    );
    row
}

fn archive_row(archive: &Archive, state: &str) -> Value {
    json!({
        "archiveId": archive.id,
        "team": archive.team,
        "origin": archive.origin,
        "quarantinedAt": archive.quarantined_at,
        "purgeAfter": archive.purge_after,
        "state": state,
    })
}

/// Directories in the store that are nobody's team: no `team.json`
/// (an older delete left them). Reported, never touched.
fn unmanaged_team_dirs() -> Vec<String> {
    let Ok(read) = fs::read_dir(crate::registry::store_dir()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .filter(|name| !matches!(crate::registry::entry_path(name), Some(p) if p.is_file()))
        .collect();
    names.sort();
    names
}

pub(crate) fn run(mode: Mode) -> Result<Report> {
    run_at(mode, now())
}

/// One expired team: close it, stop its hived gracefully, look again,
/// archive if it is still what the collector saw. Returns the row.
fn collect_expired(
    entry: &Map<String, Value>,
    since: f64,
    now: f64,
    actions: &mut Vec<String>,
) -> Map<String, Value> {
    let team = map_str(entry, "team");
    let created = created_at_string(entry);
    let by = format!("gc-{}-{}", std::process::id(), new_archive_id());
    match open_close_intent(&team, &created, since, &by, now) {
        Ok(true) => {}
        Ok(false) => {
            return with_reason(
                team_row(&team, "skipped"),
                "changed, renewed or already closing".to_string(),
            )
        }
        Err(e) => return with_reason(team_row(&team, "error"), e.to_string()),
    }
    let workspace = workspace_of(entry);
    // The hived, when one listens, is asked to retire; a socket nobody
    // listens on is no hived, and one that will not answer blocks.
    match hived_runtime(&workspace, &team) {
        Err(reason) => {
            drop_close_intent(&team, &by);
            return with_reason(team_row(&team, "blocked"), reason);
        }
        Ok(Some(_)) => {
            if !crate::hived::stop_hived_graceful(&workspace) {
                drop_close_intent(&team, &by);
                return with_reason(
                    team_row(&team, "active"),
                    "the hived declined to stop (work pending)".to_string(),
                );
            }
        }
        Ok(None) => {}
    }
    let Some(fresh) = crate::registry::load(&team) else {
        return with_reason(team_row(&team, "skipped"), "entry vanished".to_string());
    };
    // A second look from fresh observations: nothing the scan cached.
    match Observations::gather().classify(&fresh) {
        Activity::Active(reason) => {
            drop_close_intent(&team, &by);
            with_reason(team_row(&team, "active"), reason)
        }
        Activity::Unknown(reason) => {
            drop_close_intent(&team, &by);
            with_reason(team_row(&team, "blocked"), reason)
        }
        Activity::Inactive => {
            let expected = Expected {
                created_at: created,
                cold_since: since,
                closing_by: by.clone(),
            };
            match archive_team(&team, "expired", false, now, Some(&expected)) {
                Ok(archive) => {
                    actions.push(format!(
                        "archived team '{team}' as {} (idle since {}; purge after {})",
                        archive.id,
                        date(since),
                        archive.purge_after.map(date).unwrap_or_default()
                    ));
                    let mut row = team_row(&team, "archived");
                    row.insert("archiveId".to_string(), Value::from(archive.id));
                    row.insert(
                        "purgeAfter".to_string(),
                        archive.purge_after.map(Value::from).unwrap_or(Value::Null),
                    );
                    row
                }
                Err(e) => {
                    drop_close_intent(&team, &by);
                    with_reason(team_row(&team, "error"), e.to_string())
                }
            }
        }
    }
}

/// The collector at *now*: every registry team is classified, the cold
/// ones clocked and the expired ones closed and archived; every archive
/// past its purge date is purged, the ones a crash left mid-way repaired.
/// A team that fails is reported, not fatal to the run. The tail of a verb
/// (`Mode::Auto`) does nothing destructive once the scan has run over
/// `AUTO_BUDGET_SECONDS`: the clocks are written, the rest is `deferred`.
pub(crate) fn run_at(mode: Mode, now: f64) -> Result<Report> {
    let write = mode != Mode::DryRun;
    let mut report = Report::default();
    if mode == Mode::Auto {
        // One collector a day per hive home: the check and the stamp are
        // one critical section.
        let _lock = crate::registry::locked()?;
        if !stamp_due(now) {
            report.throttled = true;
            return Ok(report);
        }
        write_stamp(now)?;
    }
    let started = Instant::now();
    let over_budget =
        || mode == Mode::Auto && started.elapsed().as_secs_f64() > AUTO_BUDGET_SECONDS;
    let observations = Observations::gather();
    // The expired teams, collected after the whole scan: their row index,
    // the entry as scanned and the clock it showed.
    let mut expired: Vec<(usize, Map<String, Value>, f64)> = Vec::new();
    for entry in crate::registry::list_entries() {
        let team = map_str(&entry, "team");
        if is_set(entry.get("corrupt")) {
            report.teams.push(Value::Object(team_row(&team, "corrupt")));
            continue;
        }
        if is_kept(&entry) {
            report.teams.push(Value::Object(team_row(&team, "kept")));
            continue;
        }
        if over_budget() {
            report
                .teams
                .push(Value::Object(team_row(&team, "deferred")));
            continue;
        }
        let since = cold_since(&entry);
        let mut clock_error: Option<String> = None;
        let mut clock = |at: Option<f64>| {
            if write {
                if let Err(e) = set_cold_since(&team, at) {
                    clock_error = Some(e.to_string());
                }
            }
        };
        let row = match observations.classify(&entry) {
            Activity::Active(reason) => {
                if since.is_some() {
                    clock(None);
                }
                with_reason(team_row(&team, "active"), reason)
            }
            Activity::Unknown(reason) => {
                // Not evidence of idleness: whatever clock was running stops.
                if since.is_some() {
                    clock(None);
                }
                with_reason(team_row(&team, "blocked"), reason)
            }
            Activity::Inactive => match since {
                None => {
                    clock(Some(now));
                    cooling_row(&team, now)
                }
                Some(since) if since > now + 1.0 => {
                    // The clock went backwards: start over rather than trust a
                    // deadline from the future.
                    clock(Some(now));
                    cooling_row(&team, now)
                }
                Some(since) if now - since >= COLD_AFTER_SECONDS => {
                    let mut row = team_row(&team, "expired");
                    row.insert("coldSince".to_string(), Value::from(since));
                    if write {
                        expired.push((report.teams.len(), entry.clone(), since));
                    }
                    row
                }
                Some(since) => cooling_row(&team, since),
            },
        };
        let row = match clock_error {
            Some(e) => with_reason(
                team_row(&team, "error"),
                format!("cold clock not written: {e}"),
            ),
            None => row,
        };
        report.teams.push(Value::Object(row));
    }
    // Destructive work only with the scan complete and within budget.
    for (index, entry, since) in expired {
        let team = map_str(&entry, "team");
        let row = if over_budget() {
            team_row(&team, "deferred")
        } else {
            collect_expired(&entry, since, now, &mut report.actions)
        };
        report.teams[index] = Value::Object(row);
    }
    for name in unmanaged_team_dirs() {
        report
            .teams
            .push(Value::Object(team_row(&name, "unmanaged")));
    }
    let (archives, corrupt) = match list_trash() {
        Ok(trash) => trash,
        Err(e) => {
            report.archives.push(json!({
                "archiveId": "trash",
                "state": "error",
                "reason": e.to_string(),
            }));
            (Vec::new(), Vec::new())
        }
    };
    for archive in archives {
        if over_budget() {
            report.archives.push(archive_row(&archive, "deferred"));
            continue;
        }
        let row = match archive.state.as_str() {
            "preparing" | "restoring" => {
                if write {
                    match repair(&archive.id, now) {
                        Ok(verdict) => archive_row(&archive, verdict),
                        Err(e) => {
                            report.actions.push(format!("archive {}: {e}", archive.id));
                            archive_row(&archive, "error")
                        }
                    }
                } else {
                    archive_row(&archive, &archive.state)
                }
            }
            "purging" => {
                if write {
                    match purge_archive(&archive.id, now) {
                        Ok(Purge::Purged) => {
                            report.actions.push(format!(
                                "finished purging archive {} ('{}')",
                                archive.id, archive.team
                            ));
                            archive_row(&archive, "purged")
                        }
                        Ok(_) => archive_row(&archive, "purging"),
                        Err(e) => {
                            report.actions.push(format!("archive {}: {e}", archive.id));
                            archive_row(&archive, "error")
                        }
                    }
                } else {
                    archive_row(&archive, "purging")
                }
            }
            "quarantined" => match archive.purge_after {
                None => archive_row(&archive, "kept"),
                Some(at) if at <= now => {
                    if write {
                        match purge_archive(&archive.id, now) {
                            Ok(Purge::Purged) => {
                                report.actions.push(format!(
                                    "purged archive {} ('{}', quarantined {})",
                                    archive.id,
                                    archive.team,
                                    date(archive.quarantined_at)
                                ));
                                archive_row(&archive, "purged")
                            }
                            Ok(Purge::Deferred(until)) => {
                                report.actions.push(format!(
                                    "archive {} ('{}') was written into; purge moved to {}",
                                    archive.id,
                                    archive.team,
                                    date(until)
                                ));
                                let mut row = archive_row(&archive, "deferred");
                                row["purgeAfter"] = Value::from(until);
                                row
                            }
                            Ok(Purge::Skipped(reason)) => {
                                let mut row = archive_row(&archive, "skipped");
                                row["reason"] = Value::from(reason);
                                row
                            }
                            Err(e) => {
                                report.actions.push(format!("archive {}: {e}", archive.id));
                                archive_row(&archive, "error")
                            }
                        }
                    } else {
                        archive_row(&archive, "expired")
                    }
                }
                Some(_) => archive_row(&archive, "quarantined"),
            },
            other => {
                let mut row = archive_row(&archive, "unknown-state");
                row["reason"] = Value::from(format!("manifest state '{other}'"));
                row
            }
        };
        report.archives.push(row);
    }
    for name in corrupt {
        report
            .archives
            .push(json!({"archiveId": name, "state": "corrupt"}));
    }
    Ok(report)
}

/// The report as `hive gc run` prints it.
pub(crate) fn render_text(report: &Report) -> String {
    let mut out = String::new();
    if report.throttled {
        out.push_str("gc: ran within the last day; nothing to do\n");
        return out;
    }
    out.push_str("teams:\n");
    if report.teams.is_empty() {
        out.push_str("  (none)\n");
    }
    for row in &report.teams {
        let Some(row) = row.as_object() else {
            continue;
        };
        let team = map_str(row, "team");
        let state = map_str(row, "state");
        let detail = match state.as_str() {
            "active" | "blocked" | "error" | "skipped" => map_str(row, "reason"),
            "cooling" => format!(
                "idle since {}, archive after {}",
                date(row.get("coldSince").and_then(Value::as_f64).unwrap_or(0.0)),
                date(
                    row.get("archiveAfter")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0)
                )
            ),
            "expired" => format!(
                "idle since {}; `hive gc run` archives it",
                date(row.get("coldSince").and_then(Value::as_f64).unwrap_or(0.0))
            ),
            "archived" => format!(
                "→ {} (purge after {})",
                map_str(row, "archiveId"),
                row.get("purgeAfter")
                    .and_then(Value::as_f64)
                    .map(date)
                    .unwrap_or_else(|| "never".to_string())
            ),
            "unmanaged" => "a directory without team.json; not a team, left alone".to_string(),
            _ => String::new(),
        };
        if detail.is_empty() {
            out.push_str(&format!("  {team:<16} {state}\n"));
        } else {
            out.push_str(&format!("  {team:<16} {state:<9} {detail}\n"));
        }
    }
    out.push_str("archives:\n");
    if report.archives.is_empty() {
        out.push_str("  (none)\n");
    }
    for row in &report.archives {
        let Some(row) = row.as_object() else {
            continue;
        };
        if map_str(row, "state") == "corrupt" {
            out.push_str(&format!(
                "  {} unreadable manifest; left alone\n",
                map_str(row, "archiveId")
            ));
            continue;
        }
        let purge = match row.get("purgeAfter").and_then(Value::as_f64) {
            Some(at) => format!("purge after {}", date(at)),
            None => "kept".to_string(),
        };
        out.push_str(&format!(
            "  {} {:<16} {:<11} quarantined {} ({}), {purge}\n",
            map_str(row, "archiveId"),
            map_str(row, "team"),
            map_str(row, "state"),
            date(
                row.get("quarantinedAt")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
            ),
            map_str(row, "origin"),
        ));
    }
    for action in &report.actions {
        out.push_str(&format!("gc: {action}\n"));
    }
    out
}

pub(crate) fn render_json(report: &Report) -> String {
    let doc = json!({
        "throttled": report.throttled,
        "teams": report.teams,
        "archives": report.archives,
        "actions": report.actions,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// the tail of a verb
// ---------------------------------------------------------------------------

fn touched() -> &'static Mutex<Option<String>> {
    static CELL: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// `Team::load` found *team* in the registry: the verb running is using
/// it. Read back by `after_verb`.
pub(crate) fn note_team_use(team: &str) {
    if let Ok(mut touched) = touched().lock() {
        *touched = Some(team.to_string());
    }
}

/// The tail of a successful verb: a use clears the team's cold clock, and
/// a mutating verb runs the collector once a day. Never changes the verb's
/// own result; what the collector did is said on stderr.
pub(crate) fn after_verb(invoked: &str) {
    if !USE_VERBS.contains(&invoked) {
        return;
    }
    let team = touched().lock().ok().and_then(|mut t| t.take());
    if let Some(team) = team {
        if let Err(e) = clear_cold_since(&team) {
            eprintln!("hive gc: {e}");
        }
    }
    match run(Mode::Auto) {
        Ok(report) => {
            for action in report.actions {
                eprintln!("hive gc: {action}");
            }
        }
        Err(e) => eprintln!("hive gc: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;
    use crate::testkit::{fake_tmux_sessions, member_row};

    const T0: f64 = 1_800_000_000.0;
    const DAY: f64 = 86400.0;

    /// A private hive home with every engine home beside it, tmux with no
    /// server, and a claude ledger that answers an empty list.
    fn home() -> (
        tempfile::TempDir,
        EnvGuard,
        crate::adapters::claude_bg::testhook::Guard,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = EnvGuard::cleared(&crate::testenv::IDENTITY_VARS);
        env.set("HIVE_HOME", tmp.path().join(".hive"));
        env.set("CLAUDE_HOME", tmp.path().join(".claude"));
        env.set("CLAUDE_CONFIG_DIR", tmp.path().join(".claude"));
        env.set("CODEX_HOME", tmp.path().join(".codex"));
        env.set("GROK_HOME", tmp.path().join(".grok"));
        let _argv = fake_tmux_sessions("", &[], &[], &[]);
        let ledger = crate::adapters::claude_bg::testhook::install(
            crate::adapters::claude_bg::testhook::Hook {
                list_jobs_rows: Some(Some(Vec::new())),
                ..Default::default()
            },
        );
        *fake_hived_runtime().lock().unwrap() = None;
        (tmp, env, ledger)
    }

    fn ledger(rows: Option<Vec<Value>>) -> crate::adapters::claude_bg::testhook::Guard {
        crate::adapters::claude_bg::testhook::install(crate::adapters::claude_bg::testhook::Hook {
            list_jobs_rows: Some(rows.map(|rows| {
                rows.into_iter()
                    .filter_map(|r| r.as_object().cloned())
                    .collect()
            })),
            ..Default::default()
        })
    }

    fn team(name: &str, members: &[Map<String, Value>]) -> PathBuf {
        crate::registry::record_team(name, "", "100.0", members, "").unwrap();
        let dir = crate::registry::team_dir(name).unwrap();
        fs::create_dir_all(dir.join("artifacts")).unwrap();
        fs::write(dir.join("artifacts").join("report.md"), "# r").unwrap();
        fs::write(dir.join("hive.db"), "bus").unwrap();
        dir
    }

    fn state_of(report: &Report, team: &str) -> String {
        report
            .teams
            .iter()
            .find(|row| row["team"] == team)
            .map(|row| row["state"].as_str().unwrap_or_default().to_string())
            .unwrap_or_default()
    }

    fn events() -> Vec<Value> {
        fs::read_to_string(events_path())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn fixture(id: &str, state: &str, quarantined_at: f64, purge_after: Option<f64>) -> Archive {
        Archive {
            id: id.to_string(),
            team: "honey".to_string(),
            created_at: "100.0".to_string(),
            workspace: String::new(),
            origin: "delete".to_string(),
            state: state.to_string(),
            quarantined_at,
            purge_after,
            members: Vec::new(),
            restore_as: String::new(),
        }
    }

    /// Every file and directory under *dir* last modified at *at*.
    fn touch_all(dir: &Path, at: f64) {
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(at);
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in fs::read_dir(&d).unwrap().filter_map(|e| e.ok()) {
                let path = entry.path();
                fs::File::open(&path).unwrap().set_modified(time).unwrap();
                if path.is_dir() {
                    stack.push(path);
                }
            }
        }
    }

    /// Every path under the trash with each manifest's text, for a
    /// failure message.
    fn trash_tree() -> String {
        let mut out = String::new();
        fn walk(dir: &Path, out: &mut String) {
            if let Ok(read) = fs::read_dir(dir) {
                for entry in read.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    out.push_str(&format!("{}\n", path.display()));
                    if path.file_name().and_then(|n| n.to_str()) == Some(MANIFEST) {
                        out.push_str(&fs::read_to_string(&path).unwrap_or_default());
                    }
                    if path.is_dir() {
                        walk(&path, out);
                    }
                }
            }
        }
        walk(&trash_dir(), &mut out);
        out
    }

    #[test]
    fn test_a_cold_team_is_archived_after_thirty_days_and_purged_thirty_days_later() {
        let (_tmp, _env, _ledger) = home();
        let dir = team("honey", &[member_row("sage", "grok", "sid-sage")]);

        // first sight of a cold team starts its clock
        let first = run_at(Mode::Manual, T0).unwrap();
        assert_eq!(state_of(&first, "honey"), "cooling");
        assert_eq!(
            cold_since(&crate::registry::load("honey").unwrap()),
            Some(T0)
        );
        // still cold a day short of the deadline: nothing moves
        let early = run_at(Mode::Manual, T0 + 29.0 * DAY).unwrap();
        assert_eq!(state_of(&early, "honey"), "cooling");
        assert!(dir.join("team.json").is_file());

        // the deadline: the team directory goes to the trash whole
        let due = run_at(Mode::Manual, T0 + 30.0 * DAY).unwrap();
        assert_eq!(state_of(&due, "honey"), "archived", "{:?}", due.teams);
        assert!(crate::registry::load("honey").is_none());
        assert!(!dir.exists());
        let archives = list_archives();
        assert_eq!(archives.len(), 1, "{}", trash_tree());
        let archive = &archives[0];
        assert_eq!(archive.team, "honey");
        assert_eq!(archive.created_at, "100.0");
        assert_eq!(archive.origin, "expired");
        assert_eq!(archive.state, "quarantined");
        assert_eq!(archive.purge_after, Some(T0 + 60.0 * DAY));
        assert_eq!(archive.members, vec!["sage".to_string()]);
        assert!(archive.payload().join("team.json").is_file());
        assert_eq!(
            fs::read_to_string(archive.payload().join("artifacts").join("report.md")).unwrap(),
            "# r"
        );
        assert_eq!(due.actions.len(), 1, "{:?}", due.actions);
        // the close intent went with the entry; no trace of it in the payload
        // gates a restore
        let entry = crate::registry::load_at(&archive.payload().join("team.json")).unwrap();
        assert!(is_closing(&entry, T0 + 30.0 * DAY));

        // the trash keeps it a day short of its own deadline…
        let kept = run_at(Mode::Manual, T0 + 59.0 * DAY).unwrap();
        assert_eq!(kept.archives[0]["state"], "quarantined");
        assert!(archive.payload().is_dir());
        // …and purges it after
        let purged = run_at(Mode::Manual, T0 + 60.0 * DAY).unwrap();
        assert_eq!(purged.archives[0]["state"], "purged");
        assert!(!archive.dir().exists());
        assert!(list_archives().is_empty());
        let events = events();
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0]["event"], "quarantined");
        assert_eq!(events[0]["archiveId"], Value::from(archive.id.as_str()));
        assert_eq!(events[1]["event"], "purged");
    }

    #[test]
    fn test_activity_resets_the_clock_and_missing_evidence_blocks_and_clears_it() {
        let (_tmp, _env, ledger_guard) = home();
        team("honey", &[member_row("orch", "claude", "job-1")]);
        run_at(Mode::Manual, T0).unwrap();
        assert!(cold_since(&crate::registry::load("honey").unwrap()).is_some());

        // a running claude job is activity: the clock stops
        drop(ledger_guard);
        let running = ledger(Some(vec![json!({"id": "job-1", "pid": 4242})]));
        let report = run_at(Mode::Manual, T0 + DAY).unwrap();
        assert_eq!(state_of(&report, "honey"), "active");
        assert_eq!(cold_since(&crate::registry::load("honey").unwrap()), None);
        drop(running);

        // an asleep job (no pid) is not activity: the clock starts again…
        let asleep = ledger(Some(vec![json!({"id": "job-1"})]));
        let report = run_at(Mode::Manual, T0 + 2.0 * DAY).unwrap();
        assert_eq!(state_of(&report, "honey"), "cooling");
        assert_eq!(
            cold_since(&crate::registry::load("honey").unwrap()),
            Some(T0 + 2.0 * DAY)
        );
        drop(asleep);
        // …and a ledger that does not answer blocks and stops the clock:
        // nothing unseen counts as idle time
        let silent = ledger(None);
        let report = run_at(Mode::Manual, T0 + 3.0 * DAY).unwrap();
        assert_eq!(state_of(&report, "honey"), "blocked");
        assert_eq!(cold_since(&crate::registry::load("honey").unwrap()), None);
        drop(silent);

        // an unfinished node operation blocks even a cold team; so does a
        // record the collector cannot read
        let _empty = ledger(Some(Vec::new()));
        let ops = crate::registry::team_dir("honey")
            .unwrap()
            .join("run")
            .join("operations")
            .join("100");
        fs::create_dir_all(&ops).unwrap();
        fs::write(
            ops.join("nd-1.json"),
            r#"{"dispatchId":"nd-1","state":"running"}"#,
        )
        .unwrap();
        set_cold_since("honey", Some(T0 - 40.0 * DAY)).unwrap();
        let report = run_at(Mode::Manual, T0 + 40.0 * DAY).unwrap();
        assert_eq!(state_of(&report, "honey"), "blocked");
        assert!(crate::registry::load("honey").is_some());
        fs::write(ops.join("nd-1.json"), "{").unwrap();
        set_cold_since("honey", Some(T0 - 40.0 * DAY)).unwrap();
        let report = run_at(Mode::Manual, T0 + 40.0 * DAY).unwrap();
        assert_eq!(state_of(&report, "honey"), "blocked", "{:?}", report.teams);
        assert!(crate::registry::load("honey").is_some());
        // a writer's temp file beside the records is not a record
        fs::remove_file(ops.join("nd-1.json")).unwrap();
        fs::write(ops.join(".nd-2.json.tmp"), "{").unwrap();
        let report = run_at(Mode::Manual, T0 + 41.0 * DAY).unwrap();
        assert_eq!(state_of(&report, "honey"), "cooling", "{:?}", report.teams);
    }

    #[test]
    fn test_a_live_claude_session_and_an_unaskable_engine_are_not_idle() {
        let (tmp, _env, _ledger) = home();
        // a desktop orch whose CLI is running: registered in the sessions
        // directory with this very process's pid, in no job ledger
        let sessions = tmp.path().join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("me.json"),
            json!({
                "name": "me",
                "pid": std::process::id(),
                "messagingSocketPath": tmp.path().join("me.sock"),
                "sessionId": "s-me",
                "kind": "interactive",
            })
            .to_string(),
        )
        .unwrap();
        team("honey", &[member_row("orch", "claude", "s-me")]);
        let report = run_at(Mode::Manual, T0).unwrap();
        assert_eq!(state_of(&report, "honey"), "active", "{:?}", report.teams);

        // a member on an engine the collector has no probe for is unknown
        team("comb", &[member_row("x", "bash", "sid-x")]);
        let report = run_at(Mode::Manual, T0).unwrap();
        assert_eq!(state_of(&report, "comb"), "blocked", "{:?}", report.teams);
        // a row that never got an engine is no evidence either way
        team("wax", &[member_row("y", "codex", "")]);
        let report = run_at(Mode::Manual, T0).unwrap();
        assert_eq!(state_of(&report, "wax"), "cooling", "{:?}", report.teams);
    }

    #[test]
    fn test_a_dead_hiveds_socket_is_not_evidence_of_anything() {
        let (_tmp, _env, _ledger) = home();
        let dir = team("honey", &[member_row("sage", "grok", "sid-sage")]);
        // the hived was killed -9: its socket is still there, nobody listens
        let socket = crate::hived::socket_path(dir.to_str().unwrap());
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        assert!(socket.exists());

        let report = run_at(Mode::Manual, T0).unwrap();

        assert_eq!(state_of(&report, "honey"), "cooling", "{:?}", report.teams);
        // nor does it stand for a hived that declines to stop once the team
        // has expired
        set_cold_since("honey", Some(T0 - 40.0 * DAY)).unwrap();
        let report = run_at(Mode::Manual, T0).unwrap();
        assert_eq!(state_of(&report, "honey"), "archived", "{:?}", report.teams);
        assert!(crate::registry::load("honey").is_none());
    }

    #[test]
    fn test_a_displayed_team_is_active_and_tmux_silence_blocks_everyone() {
        let (_tmp, _env, _ledger) = home();
        team("honey", &[]);
        let _argv = fake_tmux_sessions("dev:1\t@7\thoney\t\t\t\n", &[], &[], &["dev"]);
        let report = run_at(Mode::Manual, T0).unwrap();
        assert_eq!(state_of(&report, "honey"), "active");
        assert_eq!(cold_since(&crate::registry::load("honey").unwrap()), None);

        crate::tmux::set_run_override(|_, _, _| Err(crate::tmux::TmuxError::Timeout));
        let report = run_at(Mode::Manual, T0).unwrap();
        assert_eq!(state_of(&report, "honey"), "blocked");
    }

    #[test]
    fn test_the_collector_closes_a_team_before_archiving_and_a_change_under_it_aborts() {
        let (_tmp, _env, _ledger) = home();
        team("honey", &[]);
        // the writers gate on the wall clock, so the intent is dated by it
        let now = epoch_now();
        set_cold_since("honey", Some(now - 40.0 * DAY)).unwrap();
        // a fresh close intent written by another collector: hive writers are
        // refused and this collector leaves the team alone
        let mut entry = crate::registry::load("honey").unwrap();
        let mut gc = gc_object(&entry);
        gc.insert(
            "closing".to_string(),
            json!({"at": now - 5.0, "by": "gc-other"}),
        );
        entry.insert("gc".to_string(), Value::Object(gc));
        assert!(is_closing(&entry, now));
        crate::registry::write_entry_file(
            &crate::registry::team_dir("honey")
                .unwrap()
                .join("team.json"),
            &entry,
        )
        .unwrap();
        let err = crate::team::Team::load("honey", "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("being archived"), "{err}");
        assert_eq!(
            crate::registry::reserve_member("honey", &member_row("late", "grok", ""), "100.0")
                .unwrap(),
            "closing"
        );
        let report = run_at(Mode::Manual, now).unwrap();
        assert_eq!(state_of(&report, "honey"), "skipped", "{:?}", report.teams);
        assert!(crate::registry::load("honey").is_some());
        // a stale intent (a crashed collector's) gates nothing, and one dated
        // from the future is not fresh either
        assert!(!is_closing(&entry, now + CLOSING_TTL_SECONDS + 1.0));
        assert!(!is_closing(&entry, now - 60.0));

        // a succession does not land on a closing team either
        team(
            "comb",
            &[{
                let mut row = member_row("orch", "claude", "old");
                row.insert("hostSessionId".to_string(), Value::from("local_h"));
                row
            }],
        );
        let mut comb = crate::registry::load("comb").unwrap();
        let mut gc = gc_object(&comb);
        gc.insert(
            "closing".to_string(),
            json!({"at": now - 5.0, "by": "gc-other"}),
        );
        comb.insert("gc".to_string(), Value::Object(gc));
        crate::registry::write_entry_file(
            &crate::registry::team_dir("comb").unwrap().join("team.json"),
            &comb,
        )
        .unwrap();
        assert_eq!(
            crate::registry::commit_succession("comb", "orch", "old", "local_h", "new", "100.0")
                .unwrap(),
            "closing"
        );
        assert_eq!(
            crate::registry::load("comb").unwrap()["members"][0]["sessionId"],
            "old"
        );

        // the archive commits only on the instance the intent was written on,
        // with the cold clock it saw still there…
        let since = now - 40.0 * DAY;
        let expected = Expected {
            created_at: "999".to_string(),
            cold_since: since,
            closing_by: "gc-other".to_string(),
        };
        let err = archive_team("honey", "expired", false, now, Some(&expected))
            .unwrap_err()
            .to_string();
        assert!(err.contains("changed"), "{err}");
        assert!(crate::registry::load("honey").is_some());
        assert!(list_archives().is_empty());
        // …so a team used between the scan and the commit (its clock
        // renewed) stays, though nothing else about it changed
        set_cold_since("honey", Some(now)).unwrap();
        let expected = Expected {
            created_at: "100.0".to_string(),
            cold_since: since,
            closing_by: "gc-other".to_string(),
        };
        assert!(archive_team("honey", "expired", false, now, Some(&expected)).is_err());
        assert!(crate::registry::load("honey").is_some());
        // and the intent itself is written only on the clock the scan saw
        assert!(!open_close_intent("honey", "100.0", since, "gc-me", now).unwrap());
        set_cold_since("honey", Some(since)).unwrap();
        assert!(archive_team("honey", "expired", false, now, Some(&expected)).is_ok());
        assert!(crate::registry::load("honey").is_none());
    }

    #[test]
    fn test_a_desktop_conversation_on_a_successor_session_is_live() {
        let (tmp, mut env, _ledger) = home();
        env.set("HOME", tmp.path());
        let mut orch = member_row("orch", "claude", "old");
        orch.insert("hostSessionId".to_string(), Value::from("local_conv"));
        team("honey", &[orch]);
        // the desktop restarted its CLI: the record names a new session the
        // roster does not know yet, and that session is live
        let records = tmp
            .path()
            .join("Library/Application Support/Claude/claude-code-sessions/account/org");
        fs::create_dir_all(&records).unwrap();
        fs::write(
            records.join("local_conv.json"),
            json!({"cliSessionId": "new", "priorCliSessionIds": ["old"]}).to_string(),
        )
        .unwrap();
        let sessions = tmp.path().join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("new.json"),
            json!({
                "name": "new",
                "pid": std::process::id(),
                "messagingSocketPath": tmp.path().join("new.sock"),
                "sessionId": "new",
                "kind": "interactive",
            })
            .to_string(),
        )
        .unwrap();
        let report = run_at(Mode::Manual, T0).unwrap();
        assert_eq!(state_of(&report, "honey"), "active", "{:?}", report.teams);

        // the conversation's CLI gone: idle, the clock runs
        fs::remove_file(sessions.join("new.json")).unwrap();
        let report = run_at(Mode::Manual, T0).unwrap();
        assert_eq!(state_of(&report, "honey"), "cooling", "{:?}", report.teams);

        // a record that cannot be read blocks
        fs::remove_file(records.join("local_conv.json")).unwrap();
        fs::create_dir_all(records.join("local_conv.json")).unwrap();
        let report = run_at(Mode::Manual, T0).unwrap();
        assert_eq!(state_of(&report, "honey"), "blocked", "{:?}", report.teams);
    }

    #[test]
    fn test_a_symlinked_trash_root_stops_the_trash_side_of_a_run() {
        let (tmp, _env, _ledger) = home();
        team("honey", &[]);
        let elsewhere = tmp.path().join("elsewhere");
        let planted = fixture(
            "planted00000",
            "quarantined",
            T0 - 100.0 * DAY,
            Some(T0 - 60.0 * DAY),
        );
        let payload = elsewhere.join(&planted.id).join(PAYLOAD);
        fs::create_dir_all(&payload).unwrap();
        fs::write(payload.join("valuable"), "keep me").unwrap();
        let mut text = serde_json::to_string_pretty(&planted.to_value()).unwrap();
        text.push('\n');
        fs::write(elsewhere.join(&planted.id).join(MANIFEST), &text).unwrap();
        fs::create_dir_all(crate::paths::hive_home()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, trash_dir()).unwrap();

        let report = run_at(Mode::Manual, T0).unwrap();

        assert!(payload.join("valuable").is_file());
        assert!(report.failed(), "{:?}", report.archives);
        assert!(list_archives().is_empty());
        assert!(archive(&planted.id).is_none());
        assert!(archive_team("honey", "delete", false, T0, None).is_err());
        assert!(crate::registry::load("honey").is_some());
    }

    #[test]
    fn test_a_restore_retried_after_a_failed_move_keeps_the_arrangement() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, _env, _ledger) = home();
        let dir = team("honey", &[]);
        crate::layout::remember_mirror_for_test("honey", dir.to_str().unwrap(), "100.0");
        let archive = archive_team("honey", "delete", false, T0, None).unwrap();
        let store = crate::registry::store_dir();
        fs::set_permissions(&store, fs::Permissions::from_mode(0o555)).unwrap();

        let failed = restore_archive(&archive.id, None);

        fs::set_permissions(&store, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(failed.is_err());
        let left = self::archive(&archive.id).unwrap();
        assert_eq!(left.state, "quarantined");
        // the payload's entry is as it was
        let entry = crate::registry::load_at(&archive.payload().join("team.json")).unwrap();
        assert_eq!(entry["createdAt"], "100.0");

        let restored = restore_archive(&archive.id, None).unwrap();
        let new_instance = map_str(&crate::registry::load("honey").unwrap(), "createdAt");
        assert_eq!(restored.dir, dir);
        assert_eq!(
            crate::layout::remembered_mirror("honey", dir.to_str().unwrap(), &new_instance),
            Some(false)
        );
    }

    #[test]
    fn test_delete_archives_at_once_keep_has_no_purge_date_and_the_name_is_free() {
        let (_tmp, _env, _ledger) = home();
        team("honey", &[member_row("sage", "grok", "sid-sage")]);
        let first = archive_team("honey", "delete", false, T0, None).unwrap();
        assert_eq!(first.origin, "delete");
        assert!(first.purge_after.is_some());
        assert!(crate::registry::load("honey").is_none());

        // the name is free: a new honey, its own archive later
        team("honey", &[]);
        let second = archive_team("honey", "delete", true, T0, None).unwrap();
        assert!(second.kept());
        assert_ne!(first.id, second.id);
        let ids: Vec<String> = list_archives().into_iter().map(|a| a.id).collect();
        assert!(
            ids.contains(&first.id) && ids.contains(&second.id),
            "{ids:?}"
        );

        // the kept one survives every deadline, the other goes
        let report = run_at(Mode::Manual, T0 + 400.0 * DAY).unwrap();
        assert_eq!(list_archives().len(), 1);
        assert_eq!(list_archives()[0].id, second.id);
        assert!(report
            .archives
            .iter()
            .any(|row| row["archiveId"] == second.id.as_str() && row["state"] == "kept"));
    }

    #[test]
    fn test_a_purge_waits_thirty_days_after_the_last_write_into_the_payload() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, _env, _ledger) = home();
        team("honey", &[]);
        let now = epoch_now();
        // quarantined 31 days ago, due yesterday; someone wrote into the
        // payload on day 10 — well inside the original window
        let archive = archive_team("honey", "delete", false, now - 31.0 * DAY, None).unwrap();
        let written = now - 21.0 * DAY;
        touch_all(&archive.payload(), written);

        let outcome = purge_archive(&archive.id, now).unwrap();

        let Purge::Deferred(until) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(
            (until - (written + TRASH_AFTER_SECONDS)).abs() < 1.0,
            "{until}"
        );
        assert!(archive.payload().join("hive.db").is_file());
        assert_eq!(self::archive(&archive.id).unwrap().purge_after, Some(until));
        // not due: nothing
        assert_eq!(
            purge_archive(&archive.id, now + 1.0).unwrap(),
            Purge::Skipped("not due")
        );
        // kept meanwhile: the purge sees the manifest as it is now
        set_keep(&archive.id, true).unwrap();
        assert_eq!(
            purge_archive(&archive.id, now + 900.0 * DAY).unwrap(),
            Purge::Skipped("not due")
        );
        set_keep(&archive.id, false).unwrap();
        let due = self::archive(&archive.id).unwrap().purge_after.unwrap();
        // a payload it cannot read whole is not purged
        let sealed = archive.payload().join("artifacts");
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000)).unwrap();
        let err = purge_archive(&archive.id, due);
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(err.is_err(), "{err:?}");
        assert!(archive.payload().join("hive.db").is_file());
        // due, and the last write is older than the window: purged
        assert_eq!(purge_archive(&archive.id, due).unwrap(), Purge::Purged);
        assert!(!archive.dir().exists());
        assert_eq!(
            purge_archive(&archive.id, due).unwrap(),
            Purge::Skipped("no such archive")
        );
    }

    #[test]
    fn test_restore_brings_the_archive_back_as_a_new_instance_with_its_arrangement() {
        let (_tmp, _env, _ledger) = home();
        let dir = team("honey", &[member_row("sage", "grok", "sid-sage")]);
        crate::layout::remember_mirror_for_test("honey", dir.to_str().unwrap(), "100.0");
        let archive = archive_team("honey", "delete", false, T0, None).unwrap();

        let restored = restore_archive(&archive.id, None).unwrap();

        assert_eq!(restored.team, "honey");
        assert_eq!(restored.dir, dir);
        let entry = crate::registry::load("honey").unwrap();
        let new_instance = map_str(&entry, "createdAt");
        assert_ne!(new_instance, "100.0");
        assert_eq!(
            entry["restoredFrom"]["archiveId"],
            Value::from(archive.id.as_str())
        );
        assert_eq!(entry["restoredFrom"]["createdAt"], Value::from("100.0"));
        assert_eq!(
            entry["workspace"],
            Value::from(dir.to_string_lossy().into_owned())
        );
        assert_eq!(entry["display"], Value::from(""));
        assert_eq!(entry["members"][0]["name"], Value::from("sage"));
        assert_eq!(
            fs::read_to_string(dir.join("artifacts").join("report.md")).unwrap(),
            "# r"
        );
        assert!(list_archives().is_empty());
        assert_eq!(events().last().unwrap()["event"], "restored");
        // the mirror choice the archive kept follows the new instance
        assert_eq!(
            crate::layout::remembered_mirror("honey", dir.to_str().unwrap(), &new_instance),
            Some(false)
        );
    }

    #[test]
    fn test_restore_refuses_a_taken_name_or_a_bound_session_and_takes_another_name() {
        let (_tmp, _env, _ledger) = home();
        team("honey", &[member_row("sage", "grok", "sid-sage")]);
        let archive = archive_team("honey", "delete", false, T0, None).unwrap();
        // a new honey took the name
        team("honey", &[]);
        let err = restore_archive(&archive.id, None).unwrap_err().to_string();
        assert!(err.contains("--as"), "{err}");
        assert_eq!(list_archives().len(), 1);
        assert_eq!(list_archives()[0].state, "quarantined");

        // another team rides sage's session: nothing is taken from it
        team("comb", &[member_row("rider", "grok", "sid-sage")]);
        let err = restore_archive(&archive.id, Some("honey2"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("comb.rider"), "{err}");
        assert!(crate::registry::load("honey2").is_none());
        assert_eq!(list_archives().len(), 1);

        // the session freed, another name works
        crate::registry::delete_team("comb").unwrap();
        let restored = restore_archive(&archive.id, Some("honey2")).unwrap();
        assert_eq!(restored.team, "honey2");
        let entry = crate::registry::load("honey2").unwrap();
        assert_eq!(entry["team"], Value::from("honey2"));
        assert_eq!(entry["restoredFrom"]["team"], Value::from("honey"));
        assert!(
            crate::registry::load("honey").is_some(),
            "the new honey is untouched"
        );
        assert!(list_archives().is_empty());
        assert!(restore_archive("nope", None).is_err());
    }

    #[test]
    fn test_a_restore_a_crash_interrupted_is_finished_or_reverted() {
        let (_tmp, _env, _ledger) = home();
        team("honey", &[]);
        let archive = archive_team("honey", "delete", false, T0, None).unwrap();
        // the intent landed, the move did not: back to quarantined
        let mut restoring = archive.clone();
        restoring.state = "restoring".to_string();
        restoring.restore_as = "honey".to_string();
        restoring.write().unwrap();
        assert!(
            restore_archive(&archive.id, None).is_err(),
            "not quarantined"
        );
        let report = run_at(Mode::Manual, T0 + 1.0).unwrap();
        assert_eq!(report.archives[0]["state"], "quarantined");
        assert_eq!(self::archive(&archive.id).unwrap().state, "quarantined");
        assert!(self::archive(&archive.id).unwrap().restore_as.is_empty());

        // the move landed, the archive directory did not go: finished
        let restored = restore_archive(&archive.id, Some("honey2")).unwrap();
        let mut leftover = restored.archive.clone();
        leftover.state = "restoring".to_string();
        leftover.restore_as = "honey2".to_string();
        fs::create_dir_all(leftover.dir()).unwrap();
        leftover.write().unwrap();
        let report = run_at(Mode::Manual, T0 + 2.0).unwrap();
        assert_eq!(
            report.archives[0]["state"], "restored",
            "{:?}",
            report.archives
        );
        assert!(!leftover.dir().exists());
        assert!(crate::registry::load("honey2").is_some());
    }

    #[test]
    fn test_dry_run_writes_nothing() {
        let (_tmp, _env, _ledger) = home();
        let dir = team("honey", &[]);
        set_cold_since("honey", Some(T0 - 40.0 * DAY)).unwrap();
        let before = fs::read_to_string(dir.join("team.json")).unwrap();

        let report = run_at(Mode::DryRun, T0).unwrap();

        assert_eq!(state_of(&report, "honey"), "expired");
        assert!(report.actions.is_empty());
        assert_eq!(fs::read_to_string(dir.join("team.json")).unwrap(), before);
        assert!(list_archives().is_empty());
        assert!(!stamp_path().exists());
        // a fresh team's clock is not started by a look
        team("comb", &[]);
        let report = run_at(Mode::DryRun, T0).unwrap();
        assert_eq!(state_of(&report, "comb"), "cooling");
        assert_eq!(cold_since(&crate::registry::load("comb").unwrap()), None);
        let text = render_text(&report);
        assert!(text.contains("honey"), "{text}");
        assert!(text.contains("expired"), "{text}");
    }

    #[test]
    fn test_auto_runs_at_most_once_a_day_and_a_run_that_did_nothing_counts() {
        let (_tmp, _env, _ledger) = home();
        team("honey", &[]);
        let first = run_at(Mode::Auto, T0).unwrap();
        assert!(!first.throttled);
        assert_eq!(state_of(&first, "honey"), "cooling");
        let again = run_at(Mode::Auto, T0 + 3600.0).unwrap();
        assert!(again.throttled);
        assert!(again.teams.is_empty());
        let tomorrow = run_at(Mode::Auto, T0 + DAY).unwrap();
        assert!(!tomorrow.throttled);
        // manual runs ignore the throttle
        let manual = run_at(Mode::Manual, T0 + DAY + 1.0).unwrap();
        assert!(!manual.throttled);
        // a clock that went backwards runs again rather than waiting a day
        assert!(stamp_due(T0 - 1.0));
    }

    #[test]
    fn test_a_clock_from_the_future_starts_over() {
        let (_tmp, _env, _ledger) = home();
        team("honey", &[]);
        set_cold_since("honey", Some(T0 + 10.0 * DAY)).unwrap();
        let report = run_at(Mode::Manual, T0).unwrap();
        assert_eq!(state_of(&report, "honey"), "cooling");
        assert_eq!(
            cold_since(&crate::registry::load("honey").unwrap()),
            Some(T0)
        );
    }

    #[test]
    fn test_a_preparing_archive_is_committed_with_a_fresh_clock_or_dropped() {
        let (_tmp, _env, _ledger) = home();
        let dir = team("honey", &[]);
        // the directory moved, the manifest did not follow
        let mut moved = fixture(
            "a1b2c3d4e5f6",
            "preparing",
            T0 - 100.0 * DAY,
            Some(T0 - 70.0 * DAY),
        );
        fs::create_dir_all(moved.dir()).unwrap();
        moved.write().unwrap();
        fs::rename(&dir, moved.payload()).unwrap();
        // the directory never moved
        let mut aborted = moved.clone();
        aborted.id = "0123456789ab".to_string();
        fs::create_dir_all(aborted.dir()).unwrap();
        aborted.write().unwrap();

        let report = run_at(Mode::Manual, T0).unwrap();

        // committed at the repair, not at the crash: the full trash window
        moved.state = "quarantined".to_string();
        moved.quarantined_at = T0;
        moved.purge_after = Some(T0 + TRASH_AFTER_SECONDS);
        assert_eq!(archive(&moved.id).unwrap(), moved);
        assert!(!aborted.dir().exists());
        assert_eq!(report.archives.len(), 2, "{:?}", report.archives);
    }

    #[test]
    fn test_the_trash_never_follows_a_symlink_and_reports_what_it_cannot_read() {
        let (tmp, _env, _ledger) = home();
        // an archive-shaped directory elsewhere, reached through a symlink
        // planted in the trash
        let elsewhere = tmp.path().join("elsewhere");
        let payload = elsewhere.join(PAYLOAD);
        fs::create_dir_all(&payload).unwrap();
        fs::write(payload.join("valuable"), "keep me").unwrap();
        let planted = fixture(
            "linkfixture",
            "quarantined",
            T0 - 100.0 * DAY,
            Some(T0 - 60.0 * DAY),
        );
        let mut text = serde_json::to_string_pretty(&planted.to_value()).unwrap();
        text.push('\n');
        fs::write(elsewhere.join(MANIFEST), &text).unwrap();
        fs::create_dir_all(trash_dir()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, trash_dir().join("linkfixture")).unwrap();
        // and a directory with an unreadable manifest
        fs::create_dir_all(trash_dir().join("garbled")).unwrap();
        fs::write(trash_dir().join("garbled").join(MANIFEST), "{").unwrap();

        let report = run_at(Mode::Manual, T0).unwrap();

        assert!(payload.join("valuable").is_file(), "followed the link");
        assert!(list_archives().is_empty());
        let corrupt: Vec<String> = report
            .archives
            .iter()
            .filter(|row| row["state"] == "corrupt")
            .map(|row| row["archiveId"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            corrupt,
            vec!["garbled".to_string(), "linkfixture".to_string()]
        );
        assert_eq!(
            purge_archive("linkfixture", T0).unwrap(),
            Purge::Skipped("no such archive")
        );
        assert!(render_text(&report).contains("unreadable manifest"));
    }

    #[test]
    fn test_unmanaged_store_directories_are_reported_not_touched() {
        let (_tmp, _env, _ledger) = home();
        let leftover = crate::registry::store_dir().join("oldteam");
        fs::create_dir_all(leftover.join("artifacts")).unwrap();
        let report = run_at(Mode::Manual, T0).unwrap();
        assert_eq!(state_of(&report, "oldteam"), "unmanaged");
        assert!(leftover.join("artifacts").is_dir());
        assert!(render_text(&report).contains("left alone"));
    }

    #[test]
    fn test_keep_toggles_a_team_and_an_archive() {
        let (_tmp, _env, _ledger) = home();
        team("honey", &[]);
        set_cold_since("honey", Some(T0 - 40.0 * DAY)).unwrap();
        assert!(set_keep("honey", true).unwrap().contains("keep on"));
        let report = run_at(Mode::Manual, T0).unwrap();
        assert_eq!(state_of(&report, "honey"), "kept");
        assert!(crate::registry::load("honey").is_some());
        // off: the clock starts over, not from the old date
        assert!(set_keep("honey", false).unwrap().contains("keep off"));
        assert_eq!(cold_since(&crate::registry::load("honey").unwrap()), None);

        let archive = archive_team("honey", "delete", false, T0, None).unwrap();
        let line = set_keep(&archive.id, true).unwrap();
        assert!(line.contains("kept"), "{line}");
        assert!(super::archive(&archive.id).unwrap().kept());
        let line = set_keep(&archive.id, false).unwrap();
        assert!(line.contains("purge after"), "{line}");
        assert!(!super::archive(&archive.id).unwrap().kept());
        assert!(set_keep("nobody", true).is_err());
    }

    #[test]
    fn test_busy_members_is_idle_without_a_hived_or_a_codex_daemon() {
        let (_tmp, _env, _ledger) = home();
        team(
            "honey",
            &[
                member_row("sage", "codex", "thread-1"),
                member_row("orch", "claude", "job-1"),
            ],
        );
        let entry = crate::registry::load("honey").unwrap();
        assert_eq!(busy_members(&entry), Turns::Idle);
        set_cold_since("honey", Some(T0)).unwrap();
        clear_cold_since("honey").unwrap();
        assert_eq!(cold_since(&crate::registry::load("honey").unwrap()), None);
    }

    #[test]
    fn test_the_event_log_keeps_its_tail_past_the_cap() {
        let (_tmp, _env, _ledger) = home();
        fs::create_dir_all(state_dir()).unwrap();
        let filler = format!("{{\"event\":\"x\",\"pad\":\"{}\"}}\n", "p".repeat(2000));
        fs::write(events_path(), filler.repeat(1200)).unwrap();
        assert!(fs::metadata(events_path()).unwrap().len() > EVENTS_MAX_BYTES);
        let archive = fixture("eeeeeeeeeeee", "quarantined", T0, None);
        append_event("kept", &archive, &[]);
        let lines: Vec<String> = fs::read_to_string(events_path())
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(lines.len(), EVENTS_KEEP_LINES + 1);
        assert!(lines.last().unwrap().contains("\"event\":\"kept\""));
    }

    #[test]
    fn test_dates_render_as_utc_days() {
        assert_eq!(date(0.0), "1970-01-01");
        assert_eq!(date(T0), "2027-01-15");
    }
}
