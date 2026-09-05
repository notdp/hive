use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::adapters::base::read_json_object;
use crate::adapters::claude_sessions::{_config_dir, _pid_alive, _registry_dir, truthy_str};

use super::{now_epoch, STATUS_STALE_AFTER_SECONDS};

#[cfg(test)]
use super::testhook;

// --------------------------------------------------------------------------
// pane <-> job records
// --------------------------------------------------------------------------
fn _control_dir() -> PathBuf {
    _config_dir().join("hive-control")
}

/// Per-pane record of the bg job hive bound to this pane.
pub fn pane_job_path(pane: &str) -> PathBuf {
    let slug = pane.replace('%', "");
    let slug = if slug.is_empty() { "default" } else { &slug };
    _control_dir().join(format!("hive-pane-{slug}.job"))
}

pub fn write_pane_job(
    pane: &str,
    job_id: &str,
    session_id: &str,
    cwd: &str,
) -> std::io::Result<()> {
    let path = pane_job_path(pane);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut doc = Map::new();
    doc.insert("jobId".to_string(), Value::String(job_id.to_string()));
    doc.insert(
        "sessionId".to_string(),
        Value::String(session_id.to_string()),
    );
    doc.insert("cwd".to_string(), Value::String(cwd.to_string()));
    fs::write(path, Value::Object(doc).to_string())
}

/// The pane→job binding hive writes when it puts a job behind a pane (spawn,
/// or a `--resume <job>` relaunch): the job, and the session id and cwd known
/// at that moment. The session id is empty when no engine entry answered
/// then — a `--resume` of a job whose wake failed, or a launcher spawn whose
/// entry never showed up inside the launcher's wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneJob {
    pub job_id: String,
    pub session_id: String,
    pub cwd: String,
}

/// The binding recorded for *pane*, or None.
pub fn read_pane_job(pane: &str) -> Option<PaneJob> {
    let data = read_json_object(&pane_job_path(pane))?;
    let job_id = truthy_str(data.get("jobId"));
    if job_id.is_empty() {
        return None;
    }
    Some(PaneJob {
        job_id,
        session_id: truthy_str(data.get("sessionId")),
        cwd: truthy_str(data.get("cwd")),
    })
}

pub fn clear_pane_job(pane: &str) {
    let _ = fs::remove_file(pane_job_path(pane));
}

pub fn job_id_for_pane(pane: &str) -> Option<String> {
    read_pane_job(pane).map(|record| record.job_id)
}

/// Inverse of [`pane_job_path`]: `hive-pane-19.job` -> `%19`.
fn _pane_from_record_name(name: &str) -> Option<String> {
    let slug = name.strip_prefix("hive-pane-")?.strip_suffix(".job")?;
    if slug.is_empty() || slug == "default" {
        return None;
    }
    Some(format!("%{slug}"))
}

/// Pane ids that currently have a job record on disk.
pub fn list_recorded_panes() -> Vec<String> {
    let root = _control_dir();
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut panes = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(pane) = _pane_from_record_name(name) {
            panes.push(pane);
        }
    }
    panes
}

/// Pane recorded for *job_id*, or None.
///
/// The reverse lookup behind tool-side identity: a `hive` invocation inside a
/// member's tool subprocess carries `CLAUDE_CODE_MESSAGING_SOCKET` naming the
/// engine's inbox, the engine's registry entry names the jobId, and this maps
/// it back to the tmux pane hive bound the job to.
pub fn pane_for_job(job_id: &str) -> Option<String> {
    if job_id.is_empty() {
        return None;
    }
    for pane in list_recorded_panes() {
        if let Some(record) = read_pane_job(&pane) {
            if record.job_id == job_id {
                return Some(pane);
            }
        }
    }
    None
}

// --------------------------------------------------------------------------
// engine registry entries (sessions/<enginePid>.json, kind == "bg")
// --------------------------------------------------------------------------
#[derive(Debug, Clone, PartialEq)]
pub struct EngineSession {
    pub pid: i32,
    pub job_id: String,
    pub session_id: String,
    pub socket_path: String,
    pub cwd: String,
    pub status: String,
    pub waiting_for: String,
    pub status_updated_at: f64, // epoch seconds, 0.0 when absent
    pub name: String,           // the job's label, as the panel and ledger show it
}

pub(super) fn _entry_to_engine(data: &Map<String, Value>) -> Option<EngineSession> {
    if data.get("kind").and_then(Value::as_str) != Some("bg") {
        return None;
    }
    let pid = data.get("pid").and_then(Value::as_i64)?;
    let job_id = truthy_str(data.get("jobId"));
    let sock = truthy_str(data.get("messagingSocketPath"));
    if job_id.is_empty() || sock.is_empty() {
        return None;
    }
    if !_pid_alive(pid as i32) || !Path::new(&sock).exists() {
        return None;
    }
    let updated = data
        .get("statusUpdatedAt")
        .and_then(Value::as_f64)
        .map(|raw| raw / 1000.0)
        .unwrap_or(0.0);
    Some(EngineSession {
        pid: pid as i32,
        job_id,
        session_id: truthy_str(data.get("sessionId")),
        socket_path: sock,
        cwd: truthy_str(data.get("cwd")),
        status: truthy_str(data.get("status")),
        waiting_for: truthy_str(data.get("waitingFor")),
        status_updated_at: updated,
        name: truthy_str(data.get("name")),
    })
}

/// The live engine's registry entry for *job_id*, or None.
///
/// The engine registers under its own (unstable) pid, so the jobId is found
/// by scanning the registry for the `kind:"bg"` entry naming it. None means
/// no live engine — asleep or dead; [`job_row`](super::job_row) tells them apart.
pub fn engine_session_for_job(job_id: &str) -> Option<EngineSession> {
    if job_id.is_empty() {
        return None;
    }
    let root = _registry_dir();
    let entries = fs::read_dir(&root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(data) = read_json_object(&path) else {
            continue;
        };
        if let Some(engine) = _entry_to_engine(&data) {
            if engine.job_id == job_id {
                return Some(engine);
            }
        }
    }
    None
}

/// The seam `claude_bg/tests.rs` drives through `Hook::engine_for_job`; every
/// in-module caller routes through it so a hooked lookup behaves the same.
pub(super) fn hooked_engine_for_job(job_id: &str) -> Option<EngineSession> {
    #[cfg(test)]
    {
        if let Some(Some(v)) = testhook::with(|h| {
            if h.forbid_engine_lookup {
                panic!("an idle engine must not be polled");
            }
            h.engine_for_job.as_mut().map(|queue| {
                if queue.len() > 1 {
                    queue.pop_front().unwrap()
                } else {
                    queue.front().cloned().unwrap_or(None)
                }
            })
        }) {
            return v;
        }
    }
    engine_session_for_job(job_id)
}

/// The bg engine entry registered under *pid*, or None (viewer pids and
/// interactive sessions have no bg entry).
pub fn engine_session_for_pid(pid: u32) -> Option<EngineSession> {
    let data = read_json_object(&_registry_dir().join(format!("{pid}.json")))?;
    _entry_to_engine(&data)
}

/// True when *pane* records a job whose engine is live right now.
///
/// False also covers a parked (asleep) engine — asleep is not dead, but the
/// cheap per-tick probes must not pay the `agents --all` cost; callers that
/// need the third state use [`job_row`](super::job_row).
pub fn pane_engine_alive(pane: &str) -> bool {
    match job_id_for_pane(pane) {
        Some(job_id) if !job_id.is_empty() => hooked_engine_for_job(&job_id).is_some(),
        _ => false,
    }
}

/// Transcript session id of the pane's recorded job.
///
/// The live engine's registry entry is current truth (an in-session `/clear`
/// rotates it); the record's spawn-time snapshot answers for a parked engine
/// — wake preserves the sessionId, so the snapshot stays valid.
pub fn session_id_for_pane(pane: &str) -> Option<String> {
    let record = read_pane_job(pane)?;
    if let Some(engine) = hooked_engine_for_job(&record.job_id) {
        if !engine.session_id.is_empty() {
            return Some(engine.session_id);
        }
    }
    if record.session_id.is_empty() {
        None
    } else {
        Some(record.session_id)
    }
}

// --------------------------------------------------------------------------
// runtime signal mapping (engine status -> hive runtime fields)
// --------------------------------------------------------------------------

/// Fold an engine entry's status into hive runtime fields.
///
/// `status` is the live truth (`state` in the ledger lags); `waiting`
/// carries `waitingFor`. A stale `statusUpdatedAt` demotes the status to
/// unknown instead of trusting a wedged engine's last word.
pub fn runtime_from_engine(engine: &EngineSession, now: Option<f64>) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert(
        "_runtimeSource".to_string(),
        Value::String("claude_bg".to_string()),
    );
    let current = now.unwrap_or_else(now_epoch);
    if engine.status_updated_at != 0.0
        && current - engine.status_updated_at > STATUS_STALE_AFTER_SECONDS
    {
        fields.insert("busy".to_string(), Value::Bool(false));
        fields.insert(
            "inputState".to_string(),
            Value::String("unknown".to_string()),
        );
        fields.insert(
            "inputReason".to_string(),
            Value::String("stale_status".to_string()),
        );
        return fields;
    }
    for (key, value) in
        crate::adapters::claude_sessions::runtime_from_status(&engine.status, &engine.waiting_for)
    {
        fields.insert(key, value);
    }
    fields
}
