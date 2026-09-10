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
//! team instance under its old name or another. The collector archives
//! only what it has positively seen idle: tmux not answering, a claude
//! ledger call failing, a hived that holds the socket but does not answer,
//! an unfinished node operation — each blocks the team until it clears.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};

use crate::json_fields::{is_set, map_str};

/// A team idle this long is archived.
pub const COLD_AFTER_SECONDS: f64 = 30.0 * 86400.0;
/// An archive not kept is purged this long after it was quarantined.
pub const TRASH_AFTER_SECONDS: f64 = 30.0 * 86400.0;
/// The tail of a mutating verb runs the collector at most this often.
pub const AUTO_INTERVAL_SECONDS: f64 = 86400.0;

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

fn member_names(entry: &Map<String, Value>) -> Vec<String> {
    entry
        .get("members")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(Value::as_object)
                .map(|m| map_str(m, "name"))
                .filter(|n| !n.is_empty())
                .collect()
        })
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
    /// `quarantined`, `purging`.
    pub state: String,
    pub quarantined_at: f64,
    /// None keeps the archive.
    pub purge_after: Option<f64>,
    pub members: Vec<String>,
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
        json!({
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
        })
    }

    fn from_value(doc: &Value) -> Option<Archive> {
        let doc = doc.as_object()?;
        if doc.get("schema").and_then(Value::as_u64) != Some(SCHEMA) {
            return None;
        }
        let text = |key: &str| map_str(doc, key);
        let id = text("archiveId");
        if id.is_empty() || id.contains(['/', '\\', '.']) {
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
        })
    }

    fn write(&self) -> Result<()> {
        let mut text = serde_json::to_string_pretty(&self.to_value())?;
        text.push('\n');
        write_atomic(&self.dir().join(MANIFEST), &text)
    }
}

fn read_archive(dir: &Path) -> Option<Archive> {
    let text = fs::read_to_string(dir.join(MANIFEST)).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    let archive = Archive::from_value(&doc)?;
    (dir.file_name().and_then(|n| n.to_str()) == Some(archive.id.as_str())).then_some(archive)
}

/// Every readable archive, oldest first.
pub(crate) fn list_archives() -> Vec<Archive> {
    let Ok(read) = fs::read_dir(trash_dir()) else {
        return Vec::new();
    };
    let mut archives: Vec<Archive> = read
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter_map(|dir| read_archive(&dir))
        .collect();
    archives.sort_by(|a, b| {
        a.quarantined_at
            .partial_cmp(&b.quarantined_at)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    archives
}

pub(crate) fn archive(id: &str) -> Option<Archive> {
    if id.is_empty() || id.contains(['/', '\\', '.']) {
        return None;
    }
    read_archive(&trash_dir().join(id))
}

fn new_archive_id() -> String {
    crate::naming::os_random_bytes(6)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

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
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{}", Value::Object(row));
    }
}

/// Move *team* out of the registry into the trash: its directory becomes
/// the archive's payload (the entry with it — that is what ends the team),
/// an external workspace is only recorded, never moved. The name is free
/// on return. *keep* gives the archive no purge date, else it is *at* plus
/// `TRASH_AFTER_SECONDS`. The caller has stopped what ran for the team;
/// nothing here does.
pub(crate) fn archive_team(team: &str, origin: &str, keep: bool, at: f64) -> Result<Archive> {
    let dir = crate::registry::team_dir(team).ok_or_else(|| anyhow!("unsafe team name"))?;
    let _lock = crate::registry::locked()?;
    let entry = crate::registry::load(team).ok_or_else(|| anyhow!("team '{team}' not found"))?;
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

/// Remove the archive for good: the manifest says `purging` first, so a
/// crash mid-way is finished by the next run and never restored.
pub(crate) fn purge_archive(archive: &Archive) -> Result<()> {
    let mut purging = archive.clone();
    purging.state = "purging".to_string();
    purging.write()?;
    let payload = archive.payload();
    if payload.symlink_metadata().is_ok() {
        fs::remove_dir_all(&payload)?;
    }
    fs::remove_dir_all(archive.dir())?;
    append_event("purged", archive, &[]);
    Ok(())
}

/// A `preparing` archive left by a crash: the directory moved, the
/// manifest did not follow — commit it; the directory never moved — the
/// team is still live, drop the empty archive.
fn repair_preparing(archive: &Archive) -> Result<&'static str> {
    if archive.payload().is_dir() {
        let mut fixed = archive.clone();
        fixed.state = "quarantined".to_string();
        fixed.write()?;
        append_event("quarantined", &fixed, &[("repaired", Value::Bool(true))]);
        return Ok("quarantined");
    }
    fs::remove_dir_all(archive.dir())?;
    Ok("dropped")
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
/// team: nothing is taken from anyone. Data only: no engine starts, the
/// display is built by the next attach.
pub(crate) fn restore_archive(id: &str, as_name: Option<&str>) -> Result<Restored> {
    let archive =
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
    let entry_path = payload.join("team.json");
    let text = fs::read_to_string(&entry_path)
        .map_err(|e| anyhow!("archive {id} has no readable team.json: {e}"))?;
    let Value::Object(mut entry) = serde_json::from_str::<Value>(&text)? else {
        bail!("archive {id}: team.json is not an object");
    };
    let _lock = crate::registry::locked()?;
    if target.symlink_metadata().is_ok() {
        bail!(
            "name '{name}' is in use ({}); pass --as <name>",
            if crate::registry::load(name).is_some() {
                "a live team"
            } else {
                "a directory in the store"
            }
        );
    }
    for member in entry
        .get("members")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_object)
    {
        let sid = map_str(member, "sessionId");
        if sid.is_empty() {
            continue;
        }
        let cli = map_str(member, "cli");
        if let Some((other_team, other)) =
            crate::registry::member_for_session(&sid, Some(cli.as_str()))
        {
            bail!(
                "member '{}' rides session {sid}, which is bound to {other_team}.{other}; \
                 restore refuses to take it",
                map_str(member, "name")
            );
        }
    }
    let at = now();
    let mut restored_from = Map::new();
    restored_from.insert("archiveId".to_string(), Value::from(archive.id.as_str()));
    restored_from.insert("team".to_string(), Value::from(archive.team.as_str()));
    restored_from.insert(
        "createdAt".to_string(),
        Value::from(archive.created_at.as_str()),
    );
    restored_from.insert("restoredAt".to_string(), Value::from(at));
    entry.insert("team".to_string(), Value::from(name));
    entry.insert("createdAt".to_string(), Value::from(format!("{at}")));
    entry.insert("display".to_string(), Value::from(""));
    if archive.workspace.is_empty() {
        entry.insert(
            "workspace".to_string(),
            Value::from(target.to_string_lossy().into_owned()),
        );
    }
    entry.insert("gc".to_string(), json!({"keep": false, "coldSince": null}));
    entry.insert("restoredFrom".to_string(), Value::Object(restored_from));
    fs::rename(&payload, &target)
        .map_err(|e| anyhow!("cannot move the archive back to {}: {e}", target.display()))?;
    crate::registry::write_entry_file(&target.join("team.json"), &entry)?;
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
            let mut gc = entry
                .get("gc")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            gc.insert("keep".to_string(), Value::Bool(on));
            if !on {
                gc.insert("coldSince".to_string(), Value::Null);
            }
            entry.insert("gc".to_string(), Value::Object(gc));
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
/// state (`run/operations/<incarnation>/<dispatchId>.json`).
fn unfinished_operation(workspace: &str) -> Option<String> {
    let root = Path::new(workspace).join("run").join("operations");
    let incarnations = fs::read_dir(root).ok()?;
    for incarnation in incarnations.filter_map(|e| e.ok()) {
        let Ok(records) = fs::read_dir(incarnation.path()) else {
            continue;
        };
        for record in records.filter_map(|e| e.ok()) {
            let Ok(text) = fs::read_to_string(record.path()) else {
                continue;
            };
            let Ok(doc) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if doc["state"] != "terminal" {
                return Some(doc["dispatchId"].as_str().unwrap_or("?").to_string());
            }
        }
    }
    None
}

/// A test's stand-in for the hived's `team-runtime` answer.
#[cfg(test)]
pub(crate) fn fake_hived_runtime() -> &'static Mutex<Option<Map<String, Value>>> {
    static CELL: OnceLock<Mutex<Option<Map<String, Value>>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// The runtime the team's hived reports, when there is a hived: Ok(None)
/// with no socket, Err when the socket is there and nobody answers.
fn hived_runtime(workspace: &str, team: &str) -> Result<Option<Map<String, Value>>, String> {
    #[cfg(test)]
    if let Some(runtime) = fake_hived_runtime().lock().ok().and_then(|r| r.clone()) {
        return Ok(Some(runtime));
    }
    if !crate::hived::socket_path(workspace).exists() {
        return Ok(None);
    }
    crate::hived::request_team_runtime(workspace, team)
        .map(Some)
        .ok_or_else(|| "hived holds the socket but does not answer".to_string())
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

/// The codex thread's open turn, asked of the shared daemon: None when
/// the daemon is not there (no turn can be open) or Err when it is and
/// will not say.
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

/// Everything one run of the collector reads once and shares: the tmux
/// window listing, the claude job ledger (one CLI call, only when a team
/// without a display has a claude member).
struct Observations {
    displayed: Result<HashSet<String>, &'static str>,
    claude: OnceLock<Option<Vec<Map<String, Value>>>>,
}

impl Observations {
    fn gather() -> Observations {
        Observations {
            displayed: displayed_teams(),
            claude: OnceLock::new(),
        }
    }

    fn claude_rows(&self) -> Option<&Vec<Map<String, Value>>> {
        self.claude
            .get_or_init(|| crate::adapters::claude_bg::jobs_ledger("claude"))
            .as_ref()
    }

    /// Where the team stands. Display counts as use; a hived that answers
    /// speaks for its members; without one, each member's engine is asked
    /// in its own way — the claude ledger, the codex daemon's open turn,
    /// the grok leader's socket.
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
        if let Some(id) = unfinished_operation(&workspace) {
            return Activity::Unknown(format!("unfinished operation {id}"));
        }
        let members: Vec<Map<String, Value>> = entry
            .get("members")
            .and_then(Value::as_array)
            .map(|rows| rows.iter().filter_map(Value::as_object).cloned().collect())
            .unwrap_or_default();
        for member in &members {
            let name = map_str(member, "name");
            let sid = map_str(member, "sessionId");
            if sid.is_empty() {
                continue;
            }
            match map_str(member, "cli").as_str() {
                "claude" => {
                    let Some(rows) = self.claude_rows() else {
                        return Activity::Unknown("claude job ledger unavailable".to_string());
                    };
                    let running = rows
                        .iter()
                        .any(|row| map_str(row, "id") == sid && is_set(row.get("pid")));
                    if running {
                        return Activity::Active(format!("{name}'s claude job is running"));
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
                    if crate::adapters::grok_leader::probe_socket(&socket) {
                        return Activity::Active(format!("{name}'s grok leader is alive"));
                    }
                }
                _ => {}
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
    for member in entry
        .get("members")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
    {
        let sid = map_str(member, "sessionId");
        if sid.is_empty() || map_str(member, "cli") != "codex" {
            continue;
        }
        match codex_turn_open(&sid) {
            Err(reason) => {
                return Turns::Unverified(format!("{}: {reason}", map_str(member, "name")))
            }
            Ok(Some(true)) => busy.push(map_str(member, "name")),
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
    /// The tail of a mutating verb: at most once per `AUTO_INTERVAL_SECONDS`.
    Auto,
}

/// One run's findings: a row per registry team and per archive, and the
/// lines for what was done.
#[derive(Debug, Default)]
pub(crate) struct Report {
    pub throttled: bool,
    pub teams: Vec<Value>,
    pub archives: Vec<Value>,
    pub actions: Vec<String>,
}

fn cold_since(entry: &Map<String, Value>) -> Option<f64> {
    entry
        .get("gc")
        .and_then(Value::as_object)
        .and_then(|gc| gc.get("coldSince"))
        .and_then(Value::as_f64)
}

fn is_kept(entry: &Map<String, Value>) -> bool {
    entry
        .get("gc")
        .and_then(Value::as_object)
        .and_then(|gc| gc.get("keep"))
        .and_then(Value::as_bool)
        == Some(true)
}

fn set_cold_since(team: &str, at: Option<f64>) -> Result<()> {
    crate::registry::update_entry(team, |entry| {
        let mut gc = entry
            .get("gc")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        gc.entry("keep".to_string()).or_insert(Value::Bool(false));
        gc.insert(
            "coldSince".to_string(),
            at.map(Value::from).unwrap_or(Value::Null),
        );
        entry.insert("gc".to_string(), Value::Object(gc));
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

pub(crate) fn run(mode: Mode) -> Result<Report> {
    run_at(mode, now())
}

/// The collector at *now*: every registry team is classified, the cold
/// ones clocked and the expired ones archived; every archive past its
/// purge date is purged. A team that fails to archive is reported, not
/// fatal to the run.
pub(crate) fn run_at(mode: Mode, now: f64) -> Result<Report> {
    let write = mode != Mode::DryRun;
    let mut report = Report::default();
    if mode == Mode::Auto {
        if !stamp_due(now) {
            report.throttled = true;
            return Ok(report);
        }
        write_stamp(now)?;
    }
    let observations = Observations::gather();
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
        let since = cold_since(&entry);
        let mut row = match observations.classify(&entry) {
            Activity::Active(reason) => {
                if write && since.is_some() {
                    if let Err(e) = set_cold_since(&team, None) {
                        report.actions.push(format!("{team}: {e}"));
                    }
                }
                let mut row = team_row(&team, "active");
                row.insert("reason".to_string(), Value::from(reason));
                row
            }
            Activity::Unknown(reason) => {
                let mut row = team_row(&team, "blocked");
                row.insert("reason".to_string(), Value::from(reason));
                row
            }
            Activity::Inactive => match since {
                None => {
                    if write {
                        if let Err(e) = set_cold_since(&team, Some(now)) {
                            report.actions.push(format!("{team}: {e}"));
                        }
                    }
                    let mut row = team_row(&team, "cooling");
                    row.insert("coldSince".to_string(), Value::from(now));
                    row.insert(
                        "archiveAfter".to_string(),
                        Value::from(now + COLD_AFTER_SECONDS),
                    );
                    row
                }
                Some(since) if now - since >= COLD_AFTER_SECONDS => {
                    if !write {
                        let mut row = team_row(&team, "expired");
                        row.insert("coldSince".to_string(), Value::from(since));
                        row
                    } else {
                        let workspace = workspace_of(&entry);
                        if crate::hived::socket_path(&workspace).exists() {
                            crate::hived::stop_hived(&workspace);
                        }
                        match archive_team(&team, "expired", false, now) {
                            Ok(archive) => {
                                report.actions.push(format!(
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
                                let mut row = team_row(&team, "error");
                                row.insert("reason".to_string(), Value::from(e.to_string()));
                                row
                            }
                        }
                    }
                }
                Some(since) => {
                    let mut row = team_row(&team, "cooling");
                    row.insert("coldSince".to_string(), Value::from(since));
                    row.insert(
                        "archiveAfter".to_string(),
                        Value::from(since + COLD_AFTER_SECONDS),
                    );
                    row
                }
            },
        };
        row.entry("state".to_string()).or_insert(Value::from("?"));
        report.teams.push(Value::Object(row));
    }
    for archive in list_archives() {
        let row = match archive.state.as_str() {
            "preparing" => {
                if write {
                    match repair_preparing(&archive) {
                        Ok(verdict) => archive_row(&archive, verdict),
                        Err(e) => {
                            report.actions.push(format!("archive {}: {e}", archive.id));
                            archive_row(&archive, "error")
                        }
                    }
                } else {
                    archive_row(&archive, "preparing")
                }
            }
            "purging" => {
                if write {
                    match purge_archive(&archive) {
                        Ok(()) => {
                            report.actions.push(format!(
                                "finished purging archive {} ('{}')",
                                archive.id, archive.team
                            ));
                            archive_row(&archive, "purged")
                        }
                        Err(e) => {
                            report.actions.push(format!("archive {}: {e}", archive.id));
                            archive_row(&archive, "error")
                        }
                    }
                } else {
                    archive_row(&archive, "purging")
                }
            }
            _ if archive.kept() => archive_row(&archive, "kept"),
            _ => match archive.purge_after {
                None => archive_row(&archive, "kept"),
                Some(at) if at <= now => {
                    if write {
                        match purge_archive(&archive) {
                            Ok(()) => {
                                report.actions.push(format!(
                                    "purged archive {} ('{}', quarantined {})",
                                    archive.id,
                                    archive.team,
                                    date(archive.quarantined_at)
                                ));
                                archive_row(&archive, "purged")
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
        };
        report.archives.push(row);
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
            "active" | "blocked" | "error" => map_str(row, "reason"),
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
        (tmp, env, ledger)
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

    fn events() -> Vec<Value> {
        fs::read_to_string(events_path())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
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
        assert_eq!(state_of(&due, "honey"), "archived");
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
    fn test_activity_resets_the_clock_and_missing_evidence_blocks() {
        let (tmp, _env, ledger) = home();
        team("honey", &[member_row("orch", "claude", "job-1")]);
        // clocked once…
        run_at(Mode::Manual, T0).unwrap();
        assert!(cold_since(&crate::registry::load("honey").unwrap()).is_some());

        // …a running claude job is activity: the clock stops
        drop(ledger);
        let running = crate::adapters::claude_bg::testhook::install(
            crate::adapters::claude_bg::testhook::Hook {
                list_jobs_rows: Some(Some(vec![json!({"id": "job-1", "pid": 4242})
                    .as_object()
                    .unwrap()
                    .clone()])),
                ..Default::default()
            },
        );
        let report = run_at(Mode::Manual, T0 + DAY).unwrap();
        assert_eq!(state_of(&report, "honey"), "active");
        assert_eq!(cold_since(&crate::registry::load("honey").unwrap()), None);
        drop(running);

        // a ledger that does not answer blocks: no clock, no archive
        let silent = crate::adapters::claude_bg::testhook::install(
            crate::adapters::claude_bg::testhook::Hook {
                list_jobs_rows: Some(None),
                ..Default::default()
            },
        );
        let report = run_at(Mode::Manual, T0 + 2.0 * DAY).unwrap();
        assert_eq!(state_of(&report, "honey"), "blocked");
        assert_eq!(cold_since(&crate::registry::load("honey").unwrap()), None);
        drop(silent);

        // an asleep job (no pid) is not activity: the clock starts again
        let asleep = crate::adapters::claude_bg::testhook::install(
            crate::adapters::claude_bg::testhook::Hook {
                list_jobs_rows: Some(Some(vec![json!({"id": "job-1"})
                    .as_object()
                    .unwrap()
                    .clone()])),
                ..Default::default()
            },
        );
        let report = run_at(Mode::Manual, T0 + 3.0 * DAY).unwrap();
        assert_eq!(state_of(&report, "honey"), "cooling");
        // an unfinished node operation blocks even a cold team
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
        let report = run_at(Mode::Manual, T0 + 40.0 * DAY).unwrap();
        assert_eq!(state_of(&report, "honey"), "blocked");
        assert!(crate::registry::load("honey").is_some());
        drop(asleep);
        drop(tmp);
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
    fn test_delete_archives_at_once_keep_has_no_purge_date_and_the_name_is_free() {
        let (_tmp, _env, _ledger) = home();
        team("honey", &[member_row("sage", "grok", "sid-sage")]);
        let first = archive_team("honey", "delete", false, T0).unwrap();
        assert_eq!(first.origin, "delete");
        assert!(first.purge_after.is_some());
        assert!(crate::registry::load("honey").is_none());

        // the name is free: a new honey, its own archive later
        team("honey", &[]);
        let second = archive_team("honey", "delete", true, T0).unwrap();
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
    fn test_restore_brings_the_archive_back_as_a_new_instance() {
        let (_tmp, _env, _ledger) = home();
        let dir = team("honey", &[member_row("sage", "grok", "sid-sage")]);
        let archive = archive_team("honey", "delete", false, T0).unwrap();

        let restored = restore_archive(&archive.id, None).unwrap();

        assert_eq!(restored.team, "honey");
        assert_eq!(restored.dir, dir);
        let entry = crate::registry::load("honey").unwrap();
        assert_ne!(map_str(&entry, "createdAt"), "100.0");
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
    }

    #[test]
    fn test_restore_refuses_a_taken_name_or_a_bound_session_and_takes_another_name() {
        let (_tmp, _env, _ledger) = home();
        team("honey", &[member_row("sage", "grok", "sid-sage")]);
        let archive = archive_team("honey", "delete", false, T0).unwrap();
        // a new honey took the name
        team("honey", &[]);
        let err = restore_archive(&archive.id, None).unwrap_err().to_string();
        assert!(err.contains("--as"), "{err}");
        assert!(list_archives().len() == 1);

        // another team rides sage's session: nothing is taken from it
        team("comb", &[member_row("rider", "grok", "sid-sage")]);
        let err = restore_archive(&archive.id, Some("honey2"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("comb.rider"), "{err}");
        assert!(crate::registry::load("honey2").is_none());
        assert!(list_archives().len() == 1);

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
        // a purging or unknown archive is not restorable
        assert!(restore_archive("nope", None).is_err());
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
    fn test_a_preparing_archive_is_committed_or_dropped() {
        let (_tmp, _env, _ledger) = home();
        let dir = team("honey", &[]);
        // the directory moved, the manifest did not follow
        let mut moved = Archive {
            id: "a1b2c3d4e5f6".to_string(),
            team: "honey".to_string(),
            created_at: "100.0".to_string(),
            workspace: String::new(),
            origin: "delete".to_string(),
            state: "preparing".to_string(),
            quarantined_at: T0,
            purge_after: Some(T0 + TRASH_AFTER_SECONDS),
            members: Vec::new(),
        };
        fs::create_dir_all(moved.dir()).unwrap();
        moved.write().unwrap();
        fs::rename(&dir, moved.payload()).unwrap();
        // the directory never moved
        let mut aborted = moved.clone();
        aborted.id = "0123456789ab".to_string();
        fs::create_dir_all(aborted.dir()).unwrap();
        aborted.write().unwrap();

        let report = run_at(Mode::Manual, T0 + 1.0).unwrap();

        moved.state = "quarantined".to_string();
        assert_eq!(archive(&moved.id).unwrap(), moved);
        assert!(!aborted.dir().exists());
        assert_eq!(report.archives.len(), 2, "{:?}", report.archives);
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

        let archive = archive_team("honey", "delete", false, T0).unwrap();
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
        // the clock is a use-clearing affair too
        set_cold_since("honey", Some(T0)).unwrap();
        clear_cold_since("honey").unwrap();
        assert_eq!(cold_since(&crate::registry::load("honey").unwrap()), None);
    }

    #[test]
    fn test_dates_render_as_utc_days() {
        assert_eq!(date(0.0), "1970-01-01");
        assert_eq!(date(T0), "2027-01-15");
    }
}
