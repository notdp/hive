use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::Result;
use serde_json::{json, Value};

use super::grok_home;
use crate::adapters::base::washed_spawner_env;

// --------------------------------------------------------------------------
// daemon keys: the engine's identity on disk
//
// A leader daemon is keyed by WHO it serves, not where it is displayed:
// `m-<team>.<member>` for a team member (the engine survives its pane),
// `p<slug>` for a raw `hive grok` pane outside any team (pane lifecycle).
// Pane-facing APIs resolve the pane to its key through the pane's member
// tags, so a tagged member pane and a headless caller reach the same files.
// --------------------------------------------------------------------------

const KEY_TTL: f64 = 5.0;

static KEY_CACHE: OnceLock<Mutex<HashMap<String, (Instant, String)>>> = OnceLock::new();

pub(crate) fn key_cache() -> &'static Mutex<HashMap<String, (Instant, String)>> {
    KEY_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn member_key(team: &str, member: &str) -> String {
    format!("m-{team}.{member}")
}

/// `l-<id>` — a leader a launcher raised outside tmux (`hgrok` at a
/// terminal, `handoff.rs`). It belongs to that launcher until a create or
/// join binds it to a member, and to the member from then on.
pub fn launch_key(id: &str) -> String {
    format!("l-{id}")
}

pub fn is_launch_key(key: &str) -> bool {
    key.strip_prefix("l-")
        .is_some_and(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// The alias a bound launch leaves for its member: `m-<team>.<member>.alias`
/// naming the launch key. A leader binds its socket path itself, so the
/// member cannot take the leader's socket over; every path lookup for the
/// member follows the alias instead (`canonical_key`).
pub fn alias_path_for_key(member: &str) -> PathBuf {
    grok_home().join("hive").join(format!("{member}.alias"))
}

/// The launch key a member's alias names, when the alias is a valid one.
pub fn alias_target(member: &str) -> Option<String> {
    let text = fs::read_to_string(alias_path_for_key(member)).ok()?;
    let key = text.trim().to_string();
    is_launch_key(&key).then_some(key)
}

/// The key whose files serve *key*: a member with an alias resolves to the
/// launch key it was bound from, everything else to itself.
pub fn canonical_key(key: &str) -> String {
    if key.starts_with("m-") {
        if let Some(target) = alias_target(key) {
            return target;
        }
    }
    key.to_string()
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
fn pane_option(pane: &str, key: &str) -> Option<String> {
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
        let cache = key_cache().lock().unwrap();
        if let Some((at, key)) = cache.get(pane) {
            if now.duration_since(*at).as_secs_f64() < KEY_TTL {
                return key.clone();
            }
        }
    }
    let mut key = pane_key(pane);
    if !pane.is_empty() {
        let team = pane_option(pane, "hive-team").unwrap_or_default();
        let member = pane_option(pane, "hive-agent").unwrap_or_default();
        if !team.is_empty() && !member.is_empty() {
            key = member_key(&team, &member);
        }
    }
    key_cache()
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
    grok_home()
        .join("hive")
        .join(format!("{}.sock", canonical_key(key)))
}

pub fn pane_socket_path(pane: &str) -> PathBuf {
    socket_path_for_key(&resolve_pane_key(pane))
}

/// Sibling record of the session id hive minted for this daemon.
pub fn session_path_for_key(key: &str) -> PathBuf {
    socket_path_for_key(key).with_extension("session")
}

pub fn pane_session_path(pane: &str) -> PathBuf {
    session_path_for_key(&resolve_pane_key(pane))
}

/// The member a session record is bound to: the team instance
/// (`team::created_at_key`) and the member name, written at the mint
/// (`create_member_session`, the resume and fork lanes) or when a create
/// or join binds a launch (`handoff::bind_launch`). A record without one
/// — a launch nobody has bound, or one written before the binding existed
/// — names no member and is never revived (`binding::retained`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordBinding {
    pub team: String,
    pub created_at: String,
    pub member: String,
}

const BINDING_FIELDS: [&str; 3] = ["team", "createdAt", "member"];

fn binding_fields(binding: &RecordBinding) -> [(&'static str, &str); 3] {
    [
        ("team", binding.team.as_str()),
        ("createdAt", binding.created_at.as_str()),
        ("member", binding.member.as_str()),
    ]
}

/// Write the key's record whole: the session, its cwd, and the binding
/// when the record is a member's.
pub fn write_session_key(
    key: &str,
    session_id: &str,
    cwd: &str,
    binding: Option<&RecordBinding>,
) -> Result<()> {
    let path = session_path_for_key(key);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut record = json!({"sessionId": session_id, "cwd": cwd});
    if let Some(binding) = binding {
        for (field, value) in binding_fields(binding) {
            record[field] = Value::from(value);
        }
    }
    fs::write(&path, record.to_string())?;
    Ok(())
}

/// Set (or, with `None`, clear) the binding on an existing record, every
/// other field of it kept as it was: the session, its cwd, and anything
/// hive does not know.
pub fn bind_session_key(key: &str, binding: Option<&RecordBinding>) -> Result<()> {
    let path = session_path_for_key(key);
    let text = fs::read_to_string(&path)?;
    let mut record: Value = serde_json::from_str(&text)?;
    let Some(fields) = record.as_object_mut() else {
        anyhow::bail!("session record {} is not an object", path.display());
    };
    for field in BINDING_FIELDS {
        fields.remove(field);
    }
    if let Some(binding) = binding {
        for (field, value) in binding_fields(binding) {
            fields.insert(field.to_string(), Value::from(value));
        }
    }
    fs::write(&path, record.to_string())?;
    Ok(())
}

/// The pane TUI's record write at launch (`hive grok` on a pane). A member
/// pane resolves to its member key, whose record the mint has already
/// written with its binding before the TUI is launched onto the session:
/// that binding stays when the TUI carries the same session, so the record
/// the mint bound is not unbound by the launch that follows it. Another
/// session on the key is a record the mint did not write; it starts unbound.
///
/// A member aliased to a launch writes the launch's record, under the
/// launch's lock like every other read-then-write of it (`handoff`): a
/// rollback or a rebind landing between the resolve and the write would
/// otherwise have this write carry a stale session and binding onto
/// whatever record the member resolves to by then. The alias is resolved
/// again under the lock; a member no longer naming the locked launch is
/// refused rather than re-targeted.
pub fn write_pane_session(pane: &str, session_id: &str, cwd: &str) -> Result<()> {
    let key = resolve_pane_key(pane);
    let target = canonical_key(&key);
    if !is_launch_key(&target) {
        return write_session_keeping_binding(&key, session_id, cwd);
    }
    #[cfg(test)]
    super::tests::pane_write_interleave();
    let _lock = super::handoff::launch_lock(&target)?;
    anyhow::ensure!(
        canonical_key(&key) == target,
        "{key} no longer resolves to launch {target}"
    );
    write_session_keeping_binding(&target, session_id, cwd)
}

fn write_session_keeping_binding(key: &str, session_id: &str, cwd: &str) -> Result<()> {
    let binding = read_session_key(key)
        .filter(|record| record.session_id == session_id)
        .and_then(|record| record.binding);
    write_session_key(key, session_id, cwd, binding.as_ref())
}

/// The session hive minted for a key, with the cwd recorded at spawn and
/// the member the record is bound to, when it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub session_id: String,
    pub cwd: String,
    pub binding: Option<RecordBinding>,
}

pub fn read_session_key(key: &str) -> Option<SessionRecord> {
    let text = fs::read_to_string(session_path_for_key(key)).ok()?;
    let data: Value = serde_json::from_str(&text).ok()?;
    let obj = data.as_object()?;
    let field = |name: &str| {
        obj.get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let session_id = field("sessionId")?;
    let cwd = field("cwd")?;
    let binding = match (field("team"), field("createdAt"), field("member")) {
        (Some(team), Some(created_at), Some(member)) => Some(RecordBinding {
            team,
            created_at,
            member,
        }),
        _ => None,
    };
    Some(SessionRecord {
        session_id,
        cwd,
        binding,
    })
}

pub fn read_pane_session(pane: &str) -> Option<SessionRecord> {
    read_session_key(&resolve_pane_key(pane))
}

/// Leader env: this pane, and nothing inherited that lies about identity.
///
/// The spawner may itself run inside another member's engine (an orch's
/// `hive workflow run`), whose env carries that engine's identity markers —
/// CLAUDE_CODE_MESSAGING_SOCKET, CODEX_THREAD_ID or the spawner's own
/// GROK_SESSION_ID would each make every hive call inside this grok member
/// resolve to the *spawner*. Wash them: the leader exports its own session
/// id into the tools it runs, and that is the only identity they need.
pub(crate) fn daemon_env_for_pane(pane: &str) -> HashMap<String, String> {
    let mut env = washed_spawner_env(&["CODEX_THREAD_ID", "GROK_SESSION_ID"]);
    env.insert("TMUX_PANE".to_string(), pane.to_string());
    env
}

/// Inverse of [`socket_path_for_key`]: `p19.sock` -> `p19`.
pub(crate) fn key_from_socket_name(name: &str) -> Option<String> {
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
    if is_launch_key(key) {
        return Some(key.to_string());
    }
    None
}

/// `m-honey.rex.alias` -> `m-honey.rex`: a bound launch listed under the
/// member it serves, so the member's lifecycle (kill, delete, the hived's
/// reap) reaches the leader through the alias.
pub(crate) fn key_from_alias_name(name: &str) -> Option<String> {
    let key = name.strip_suffix(".alias")?;
    member_from_key(key).map(|_| key.to_string())
}
