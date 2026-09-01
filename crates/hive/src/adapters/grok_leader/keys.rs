use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::Result;
use serde_json::{json, Value};

use super::grok_home;

// --------------------------------------------------------------------------
// daemon keys: the engine's identity on disk
//
// A leader daemon is keyed by WHO it serves, not where it is displayed:
// `m-<team>.<member>` for a team member (the engine survives its pane),
// `p<slug>` for a raw `hive grok` pane outside any team (pane lifecycle).
// Pane-facing APIs resolve the pane to its key through the pane's member
// tags, so a tagged member pane and a headless caller reach the same files.
// --------------------------------------------------------------------------

const _KEY_TTL: f64 = 5.0;

static _KEY_CACHE: OnceLock<Mutex<HashMap<String, (Instant, String)>>> = OnceLock::new();

pub(super) fn _key_cache() -> &'static Mutex<HashMap<String, (Instant, String)>> {
    _KEY_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn member_key(team: &str, member: &str) -> String {
    format!("m-{team}.{member}")
}

pub fn pane_key(pane: &str) -> String {
    let slug = pane.replace('%', "");
    if slug.is_empty() {
        "pdefault".to_string()
    } else {
        format!("p{slug}")
    }
}

/// `m-<team>.<member>` -> (team, member); team names are dot-free.
pub fn member_from_key(key: &str) -> Option<(String, String)> {
    let rest = key.strip_prefix("m-")?;
    let (team, member) = rest.split_once('.')?;
    if team.is_empty() || member.is_empty() {
        return None;
    }
    Some((team.to_string(), member.to_string()))
}

/// The pane-option read seam: the real tmux round-trip in production, a
/// per-test override (default: untagged) under cfg(test) — tests must never
/// hit the real tmux server.
fn _pane_option(pane: &str, key: &str) -> Option<String> {
    #[cfg(test)]
    {
        super::tests::pane_option_override(pane, key)
    }
    #[cfg(not(test))]
    {
        crate::tmux::get_pane_option(pane, key)
    }
}

/// The daemon key a pane addresses: its member key when tagged, else its
/// pane key. Cached briefly — tag reads are tmux round-trips on hot paths.
pub fn resolve_pane_key(pane: &str) -> String {
    let now = Instant::now();
    {
        let cache = _key_cache().lock().unwrap();
        if let Some((at, key)) = cache.get(pane) {
            if now.duration_since(*at).as_secs_f64() < _KEY_TTL {
                return key.clone();
            }
        }
    }
    let mut key = pane_key(pane);
    if !pane.is_empty() {
        let team = _pane_option(pane, "hive-team").unwrap_or_default();
        let member = _pane_option(pane, "hive-agent").unwrap_or_default();
        if !team.is_empty() && !member.is_empty() {
            key = member_key(&team, &member);
        }
    }
    _key_cache()
        .lock()
        .unwrap()
        .insert(pane.to_string(), (now, key.clone()));
    key
}

/// Leader socket under the real GROK_HOME.
///
/// Deliberately short (`hive/p19.sock` / `hive/m-honey.rex.sock`):
/// AF_UNIX paths cap at 104 bytes and the leader binds this path itself.
pub fn socket_path_for_key(key: &str) -> PathBuf {
    grok_home().join("hive").join(format!("{key}.sock"))
}

pub fn pane_socket_path(pane: &str) -> PathBuf {
    socket_path_for_key(&resolve_pane_key(pane))
}

/// Sibling pidfile of the leader socket.
///
/// Written once the socket appears so the hived (which does not start the
/// daemon) can prove liveness and reap orphans.
pub fn pane_pidfile_path(pane: &str) -> PathBuf {
    pane_socket_path(pane).with_extension("pid")
}

/// Sibling record of the session id hive minted for this daemon.
pub fn session_path_for_key(key: &str) -> PathBuf {
    socket_path_for_key(key).with_extension("session")
}

pub fn pane_session_path(pane: &str) -> PathBuf {
    session_path_for_key(&resolve_pane_key(pane))
}

pub fn write_session_key(key: &str, session_id: &str, cwd: &str) -> Result<()> {
    let path = session_path_for_key(key);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &path,
        json!({"sessionId": session_id, "cwd": cwd}).to_string(),
    )?;
    Ok(())
}

pub fn write_pane_session(pane: &str, session_id: &str, cwd: &str) -> Result<()> {
    write_session_key(&resolve_pane_key(pane), session_id, cwd)
}

pub fn read_session_key(key: &str) -> Option<(String, String)> {
    let text = fs::read_to_string(session_path_for_key(key)).ok()?;
    let data: Value = serde_json::from_str(&text).ok()?;
    let obj = data.as_object()?;
    let session_id = obj
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|sid| !sid.is_empty())?;
    let cwd = obj
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())?;
    Some((session_id.to_string(), cwd.to_string()))
}

pub fn read_pane_session(pane: &str) -> Option<(String, String)> {
    read_session_key(&resolve_pane_key(pane))
}

/// Leader env: the member's identity, nothing inherited that lies.
///
/// The spawner may itself run inside another member's engine (an orch's
/// flow runner), whose env carries that engine's identity markers —
/// CLAUDE_CODE_MESSAGING_SOCKET would make every hive call inside this
/// grok member resolve to the *orch's* pane, and inherited HIVE_TEAM /
/// HIVE_MEMBER would name the spawner. Wash them; pin our own.
pub(super) fn _daemon_env_for_pane(pane: &str) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| {
            !(key.starts_with("CLAUDE")
                || key.starts_with("ANTHROPIC")
                || matches!(
                    key.as_str(),
                    "CODEX_THREAD_ID" | "HIVE_TEAM" | "HIVE_MEMBER"
                ))
        })
        .collect();
    env.insert("TMUX_PANE".to_string(), pane.to_string());
    if let Some((team, member)) = member_from_key(&resolve_pane_key(pane)) {
        env.insert("HIVE_TEAM".to_string(), team);
        env.insert("HIVE_MEMBER".to_string(), member);
    }
    env
}

/// Inverse of [`socket_path_for_key`]: `p19.sock` -> `p19`.
pub(super) fn _key_from_socket_name(name: &str) -> Option<String> {
    let key = name.strip_suffix(".sock")?;
    if key.starts_with("m-") {
        return if member_from_key(key).is_some() {
            Some(key.to_string())
        } else {
            None
        };
    }
    if let Some(rest) = key.strip_prefix('p') {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return Some(key.to_string());
        }
    }
    None
}
