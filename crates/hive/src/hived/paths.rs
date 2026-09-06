// --------------------------------------------------------------------------
// small helpers
// --------------------------------------------------------------------------

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::devlog;

use super::*;

pub(crate) fn now_iso() -> String {
    format!("{}Z", devlog::utc_now_iso_seconds())
}

pub(super) fn getpid() -> i64 {
    std::process::id() as i64
}

pub(crate) fn hived_metadata(started_at: &str) -> Map<String, Value> {
    let mut meta = Map::new();
    meta.insert("pid".to_string(), Value::from(getpid()));
    meta.insert("started_at".to_string(), Value::from(started_at));
    meta.insert("code_hash".to_string(), Value::from(hived_build_hash()));
    meta
}

/// Registry `createdAt` is compared as a string, so the hived formats its
/// float exactly like the CLI writer did (whole seconds keep a `.0`).
pub(super) use crate::pyval::py_float_str;

pub(super) fn map_get_str(map: &Map<String, Value>, key: &str) -> String {
    match map.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => match other {
            Value::Bool(b) => {
                if *b {
                    "True".to_string()
                } else {
                    "False".to_string()
                }
            }
            _ => other.to_string(),
        },
    }
}

// --------------------------------------------------------------------------
// paths / owner file
// --------------------------------------------------------------------------

pub(crate) fn run_dir_impl(workspace: &str) -> PathBuf {
    devlog::run_dir(Path::new(workspace))
}

/// The socket hived binds and clients connect to. Relocates out of the
/// workspace when the in-tree path would overflow `sun_path`
/// (`devlog::hived_socket_path_in`); `socket_link_path` is the in-tree
/// name either way.
pub fn socket_path(workspace: &str) -> PathBuf {
    devlog::hived_socket_path_in(&hooked_run_dir(workspace))
}

/// `<run dir>/hived.sock`: the socket itself when it fits, a symlink to the
/// relocated socket when it does not.
pub(crate) fn socket_link_path(workspace: &str) -> PathBuf {
    devlog::hived_socket_link_path(&hooked_run_dir(workspace))
}

pub(crate) fn lock_path(workspace: &str) -> PathBuf {
    hooked_run_dir(workspace).join("hived.lock")
}

fn owner_path(workspace: &str) -> PathBuf {
    hooked_run_dir(workspace).join("hived.owner.json")
}

pub(crate) fn write_hived_owner_impl(workspace: &str, pid: i64, started_at: &str, token: &str) {
    let path = owner_path(workspace);
    let tmp = path.with_file_name(format!(
        "{}.{pid}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("")
    ));
    let mut payload = Map::new();
    payload.insert("pid".to_string(), Value::from(pid));
    payload.insert("startedAt".to_string(), Value::from(started_at));
    payload.insert("token".to_string(), Value::from(token));
    let write = || -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&tmp, serde_json::to_string(&payload).unwrap_or_default())?;
        fs::rename(&tmp, &path)?;
        Ok(())
    };
    if write().is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

fn read_hived_owner(workspace: &str) -> Option<Map<String, Value>> {
    let text = fs::read_to_string(owner_path(workspace)).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

fn owner_pid(owner: &Map<String, Value>) -> Option<i64> {
    match owner.get("pid") {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn owner_matches_current_process(owner: Option<&Map<String, Value>>, owner_token: &str) -> bool {
    let Some(owner) = owner else { return true };
    if owner.is_empty() {
        return true;
    }
    // A missing pid reads as 0; an unparseable one is never a foreign owner
    // (treated as matching).
    let pid = match owner.get("pid") {
        None => 0,
        Some(_) => match owner_pid(owner) {
            Some(pid) => pid,
            None => return true,
        },
    };
    pid == getpid() && owner.get("token").and_then(Value::as_str) == Some(owner_token)
}

pub(crate) fn foreign_owner_pid(workspace: &str, owner_token: &str) -> Option<i64> {
    let owner = read_hived_owner(workspace);
    if owner_matches_current_process(owner.as_ref(), owner_token) {
        return None;
    }
    Some(owner.as_ref().and_then(owner_pid).unwrap_or(0))
}

fn cleanup_owner_if_current(workspace: &str, owner_token: &str) {
    let owner = read_hived_owner(workspace);
    let Some(owner) = owner else { return };
    if owner.is_empty() || !owner_matches_current_process(Some(&owner), owner_token) {
        return;
    }
    let _ = fs::remove_file(owner_path(workspace));
}

pub(crate) fn cleanup_socket_if_owner(workspace: &str, owner_token: &str) {
    let owner = read_hived_owner(workspace);
    if let Some(owner) = owner.as_ref() {
        if !owner.is_empty() && !owner_matches_current_process(Some(owner), owner_token) {
            return;
        }
    }
    hooked_cleanup_socket(workspace);
    cleanup_owner_if_current(workspace, owner_token);
}

pub(crate) fn cleanup_socket_impl(workspace: &str) {
    let sock = socket_path(workspace);
    let link = socket_link_path(workspace);
    let _ = fs::remove_file(&sock);
    if link != sock {
        let _ = fs::remove_file(&link);
        // the per-workspace directory under /tmp/hive-<uid> is ours alone;
        // remove_dir only succeeds once it is empty
        if let Some(dir) = sock.parent() {
            let _ = fs::remove_dir(dir);
        }
    }
}
