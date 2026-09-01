use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};

use super::*;
use crate::team::{Team, LEAD_AGENT_NAME};
use crate::tmux;

pub(crate) const _TMUX_REQUIRED_MESSAGE: &str =
    "Hive requires tmux. Start or attach to a tmux session first.";

// Verbs that never need a tmux context — plus the team verbs, which read the
// registry (the truth layer) and only touch tmux when a display exists.
pub(crate) const _TMUX_OPTIONAL_ROOT_COMMANDS: &[&str] = &[
    "plugin",
    "config",
    "bootstrap",
    "shell-init",
    "codex",
    "claude",
    "grok",
    "resume-hint",
    "skills",
    "worktree",
    "ls",
    "ccd",
    "create",
    "join",
    "spawn",
    "team",
    "kill",
    "delete",
    "attach",
    "view",
];

pub(crate) const _CODEX_NATIVE_REQUIRED_BYPASS_COMMANDS: &[&str] = &[
    "claude",
    "codex",
    "config",
    "doctor",
    "grok",
    "inject",
    "plugin",
    "resume-hint",
    "shell-init",
    "skills",
];

pub const TEAM_NAME_POOL: [&str; 10] = [
    "honey", "comb", "wasp", "bumble", "hornet", "nectar", "pollen", "amber", "clover", "sage",
];

pub(crate) const _RANDOM_AGENT_NAMES: [&str; 10] = [
    "yoyo", "lulu", "nini", "bobo", "kiki", "dodo", "pipi", "toto", "momo", "coco",
];

// ---------------------------------------------------------------------------
// Small shared utilities
// ---------------------------------------------------------------------------

/// Python `_fail`: print `Error: msg` to stderr, exit 1.
pub(crate) fn fail(msg: &str) -> ! {
    eprintln!("Error: {msg}");
    std::process::exit(1);
}

/// Bridge for anyhow-returning helpers used from CLI handlers: any Err takes
/// the `_fail` exit lane.
pub(crate) fn ok_or_fail<T>(result: Result<T>) -> T {
    match result {
        Ok(value) => value,
        Err(err) => fail(&err.to_string()),
    }
}

pub(crate) fn getcwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Python `str(float)` for epoch timestamps: integral floats keep `.0`.
pub(crate) use crate::team::py_float_str;

pub(crate) fn env_string(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

/// `str(map.get(key) or "")` for JSON payload rows.
pub(crate) fn map_str(map: &Map<String, Value>, key: &str) -> String {
    match map.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(Value::Bool(false)) => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Python truthiness for an optional JSON value.
pub(crate) fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map_or(true, |f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `json.dumps(payload, indent=2, ensure_ascii=False)`.
pub(crate) fn json_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// Python `Path(...).expanduser()` (bare `~` and `~/...` forms).
pub(crate) fn expanduser(path: &str) -> String {
    if path == "~" {
        return env_string("HOME");
    }
    if let Some(rest) = path.strip_prefix("~/") {
        let home = env_string("HOME");
        if !home.is_empty() {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

/// os.urandom-grade bytes for name picks and artifact filenames.
pub(super) fn os_random_bytes(n: usize) -> Vec<u8> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    // ponytail: nanos fallback only fires when /dev/urandom is unreadable —
    // uniqueness is what matters here, not cryptographic strength.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for (i, b) in buf.iter_mut().enumerate() {
        *b = ((nanos >> ((i % 16) * 8)) & 0xff) as u8;
    }
    buf
}

/// `secrets.token_urlsafe(4)` — 4 random bytes, base64url, no padding.
pub(crate) fn token_urlsafe4() -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let data = os_random_bytes(4);
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6 & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 63) as usize] as char);
        }
    }
    out
}

/// `secrets.choice(seq)`.
pub(super) fn random_choice<'a>(options: &[&'a str]) -> &'a str {
    let idx = os_random_bytes(1)[0] as usize % options.len();
    options[idx]
}

pub(crate) fn stdin_isatty() -> bool {
    unsafe { libc::isatty(0) == 1 }
}

// ---------------------------------------------------------------------------
// Binding discovery
// ---------------------------------------------------------------------------

/// Member identity from the engine's own env (HIVE_TEAM / HIVE_MEMBER).
///
/// Engines carry their member identity in env (claude bg spawn, grok leader
/// daemon), so a tool subprocess resolves who it is without any pane —
/// the fallback lane when no pane binding exists (headless member, dead
/// display). Workspace comes from the team's registry entry.
pub(crate) fn _discover_env_binding() -> Map<String, Value> {
    let team = env_string("HIVE_TEAM").trim().to_string();
    let agent = env_string("HIVE_MEMBER").trim().to_string();
    if team.is_empty() || agent.is_empty() {
        return Map::new();
    }
    let workspace = crate::registry::load(&team)
        .map(|entry| map_str(&entry, "workspace"))
        .unwrap_or_default();
    let mut payload = Map::new();
    payload.insert("team".to_string(), Value::String(team));
    payload.insert("workspace".to_string(), Value::String(workspace));
    payload.insert("agent".to_string(), Value::String(agent));
    payload.insert("role".to_string(), Value::String("agent".to_string()));
    payload.insert("pane".to_string(), Value::String(String::new()));
    payload.insert("tmuxSession".to_string(), Value::String(String::new()));
    payload.insert("tmuxWindow".to_string(), Value::String(String::new()));
    payload
}

/// Roster identity of the Claude session this process runs inside.
///
/// The last rung of the scope ladder: pane tags cover members with a
/// display, `HIVE_TEAM`/`HIVE_MEMBER` cover spawned engines, but a session
/// that created or joined a team headless carries neither — its scope lives
/// only in the registry row keyed by its sessionId. This is the same
/// authority `hive send` resolves guest senders by.
pub(crate) fn _session_member_binding() -> Map<String, Value> {
    let Some(session) = crate::adapters::claude_sessions::self_session() else {
        return Map::new();
    };
    let Some((team, agent)) = _registry_member_for_session(&session.session_id) else {
        return Map::new();
    };
    let workspace = crate::registry::load(&team)
        .map(|entry| map_str(&entry, "workspace"))
        .unwrap_or_default();
    let mut payload = Map::new();
    payload.insert("team".to_string(), Value::String(team));
    payload.insert("workspace".to_string(), Value::String(workspace));
    payload.insert("agent".to_string(), Value::String(agent));
    payload.insert("role".to_string(), Value::String("agent".to_string()));
    payload.insert("pane".to_string(), Value::String(String::new()));
    payload.insert("tmuxSession".to_string(), Value::String(String::new()));
    payload.insert("tmuxWindow".to_string(), Value::String(String::new()));
    payload
}

pub(crate) fn _discover_tmux_binding() -> Map<String, Value> {
    if !tmux::is_inside_tmux() {
        return _discover_env_binding();
    }
    let current_pane = match tmux::get_current_pane_id() {
        Some(p) if !p.is_empty() => p,
        _ => return _discover_env_binding(),
    };
    let team_name = match tmux::get_pane_option(&current_pane, "hive-team") {
        Some(t) if !t.is_empty() => t,
        _ => return _discover_env_binding(),
    };
    let agent_name = tmux::get_pane_option(&current_pane, "hive-agent").unwrap_or_default();
    let role = tmux::get_pane_option(&current_pane, "hive-role").unwrap_or_default();
    if agent_name.is_empty() && role.is_empty() {
        return Map::new();
    }
    let window_target = tmux::get_current_window_target().unwrap_or_default();
    let session_name = tmux::get_current_session_name().unwrap_or_default();
    let workspace = if !window_target.is_empty() {
        tmux::get_window_option(&window_target, "hive-workspace").unwrap_or_default()
    } else {
        String::new()
    };
    let group = tmux::get_pane_option(&current_pane, "hive-group").unwrap_or_default();
    let mut payload = Map::new();
    payload.insert("team".to_string(), Value::String(team_name));
    payload.insert("workspace".to_string(), Value::String(workspace));
    payload.insert("agent".to_string(), Value::String(agent_name));
    payload.insert("role".to_string(), Value::String(role));
    payload.insert("pane".to_string(), Value::String(current_pane));
    payload.insert("tmuxSession".to_string(), Value::String(session_name));
    payload.insert("tmuxWindow".to_string(), Value::String(window_target));
    if !group.is_empty() {
        payload.insert("group".to_string(), Value::String(group));
    }
    payload
}

pub(crate) fn _default_team() -> Option<String> {
    let binding = _discover_tmux_binding();
    let team = map_str(&binding, "team");
    if !team.is_empty() {
        return Some(team);
    }
    let session = _session_member_binding();
    let team = map_str(&session, "team");
    if team.is_empty() {
        None
    } else {
        Some(team)
    }
}

pub(crate) fn _default_agent() -> Option<String> {
    let binding = _discover_tmux_binding();
    let agent = map_str(&binding, "agent");
    if !agent.is_empty() {
        return Some(agent);
    }
    let session = _session_member_binding();
    let agent = map_str(&session, "agent");
    if agent.is_empty() {
        None
    } else {
        Some(agent)
    }
}

pub(crate) fn _resolve_sender(agent_name: Option<&str>) -> String {
    agent_name
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(_default_agent)
        .unwrap_or_else(|| LEAD_AGENT_NAME.to_string())
}

// ---------------------------------------------------------------------------
// Team / workspace resolution
// ---------------------------------------------------------------------------

pub(crate) fn _load_team(team: &str, prefer_pane: &str) -> Result<Team> {
    Team::load(team, prefer_pane).map_err(|_| anyhow!("team '{team}' not found"))
}

/// Addressing order: explicit team -> binding discovery (pane tags, then
/// engine env identity). An explicit team is the caller's intent — it loads
/// from the registry wherever the caller happens to be.
pub fn resolve_scoped_team(
    team: Option<&str>,
    required: bool,
) -> Result<(Option<String>, Option<Team>)> {
    if let Some(team) = team.filter(|t| !t.is_empty()) {
        let loaded = _load_team(team, "")?;
        return Ok((Some(team.to_string()), Some(loaded)));
    }
    if let Some(discovered) = _default_team() {
        let prefer_pane = tmux::get_current_pane_id().unwrap_or_default();
        let loaded = _load_team(&discovered, &prefer_pane)?;
        return Ok((Some(discovered), Some(loaded)));
    }
    if required {
        bail!(
            "no Hive team in scope — pass -t <team> (see `hive ls`), or run \
             from a bound pane (`hive create` binds one)"
        );
    }
    Ok((None, None))
}

pub fn resolve_workspace(team: Option<&Team>, required: bool) -> Result<String> {
    if let Some(t) = team {
        if !t.workspace.is_empty() {
            return Ok(t.workspace.clone());
        }
    }
    let ctx = crate::context::load_current_context();
    if let Some(ws) = ctx.get("workspace").filter(|w| !w.is_empty()) {
        return Ok(ws.clone());
    }
    if required {
        bail!("workspace not found (create a team with --workspace, or run `hive create`)");
    }
    Ok(String::new())
}

// ---------------------------------------------------------------------------
// Pane target resolution (compact / fork)
// ---------------------------------------------------------------------------

/// Identity of an agent pane resolved straight from its tmux options.
///
/// Deliberately team-agnostic: `team_name` is empty for panes not bound to
/// any Hive team, and `member_label` falls back to the literal pane id.
#[derive(Debug, Clone)]
pub(crate) struct PaneTarget {
    pub pane_id: String,
    pub team_name: String,
    pub is_team_bound: bool,
    pub cli: String,
    pub member_label: String,
}

/// Resolve a pane's identity from tmux pane options *only* (never re-resolve
/// an agent by name through Team state — the cross-window same-name bug PR #8
/// fixed for `compact --pane`).
pub(crate) fn _resolve_pane_target(pane_id: &str) -> PaneTarget {
    let pane = if !pane_id.is_empty() {
        pane_id.to_string()
    } else {
        tmux::get_current_pane_id().unwrap_or_default()
    };
    if pane.is_empty() {
        fail("cannot determine current pane (pass --pane explicitly)");
    }
    let team_name = tmux::get_pane_option(&pane, "hive-team").unwrap_or_default();
    let option_cli = crate::agent_cli::normalize_command(
        &tmux::get_pane_option(&pane, "hive-cli").unwrap_or_default(),
    );
    let cli_name = if crate::agent_cli::AGENT_CLI_NAMES.contains(&option_cli.as_str()) {
        option_cli
    } else {
        crate::agent_cli::detect_profile_for_pane(&pane)
            .map(|profile| profile.name.to_string())
            .unwrap_or_default()
    };
    if cli_name.is_empty() {
        fail(&format!("unsupported agent pane '{pane}'"));
    }
    let member_label = match tmux::get_pane_option(&pane, "hive-agent") {
        Some(agent) if !agent.is_empty() => agent,
        _ => pane.clone(),
    };
    PaneTarget {
        pane_id: pane,
        is_team_bound: !team_name.is_empty(),
        team_name,
        cli: cli_name,
        member_label,
    }
}

pub(crate) fn _ensure_pane_in_scope(t: &Team, pane_id: &str) {
    if pane_id.is_empty() {
        return;
    }
    let pane_window = tmux::get_pane_window_target(pane_id).unwrap_or_default();
    let team_window = t.tmux_window.clone();
    if !team_window.is_empty() && !pane_window.is_empty() && pane_window != team_window {
        fail(&format!(
            "pane '{pane_id}' is in tmux window '{pane_window}', not team '{}' window '{team_window}'",
            t.name
        ));
    }
    if let Some(pane_team) = tmux::get_pane_option(pane_id, "hive-team") {
        if !pane_team.is_empty() && pane_team != t.name {
            fail(&format!(
                "pane '{pane_id}' already belongs to team '{pane_team}'"
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Send protocol helpers
// ---------------------------------------------------------------------------

pub(crate) fn _maybe_warn_long_body(body: &str, command: &str) {
    if let Some(hint) = crate::runtime_state::body_warning_hint(body) {
        eprintln!(
            "{}",
            crate::runtime_state::format_body_warning(command, &hint)
        );
    }
}

/// Pure core of `_validate_root_send_protocol`: Some(error) when the body
/// violates the root-thread protocol.
pub(crate) fn _root_send_protocol_error(body: &str) -> Option<String> {
    let summary = body.trim();
    if summary.is_empty() {
        return Some("new root send requires a short body summary".to_string());
    }
    // artifact is not mandatory — short confirmations like "ack" legitimately
    // don't need one. The length/structure gate below already forces bulky or
    // structured content into --artifact.
    if crate::runtime_state::body_warning_hint(summary).is_some() {
        return Some(
            "new root send body must stay short and unstructured; move details into --artifact \
             (prefer `--artifact -` unless you already have a file)"
                .to_string(),
        );
    }
    None
}

pub(crate) fn _validate_root_send_protocol(body: &str, _artifact: &str) {
    if let Some(err) = _root_send_protocol_error(body) {
        fail(&err);
    }
}

// ---------------------------------------------------------------------------
// Payload helpers
// ---------------------------------------------------------------------------

pub(crate) fn _add_runtime_location_fields(payload: &mut Map<String, Value>) {
    if !payload.contains_key("runtimeWorkspace") && payload.contains_key("workspace") {
        if let Some(ws) = payload.shift_remove("workspace") {
            payload.insert("runtimeWorkspace".to_string(), ws);
        }
    }
    payload.insert("cwd".to_string(), Value::String(getcwd()));
}

/// Stable per-window slug. Uses the tmux window id (`@42` → `w42`); falls
/// back to the mutable window index only when no id is available.
pub(crate) fn _window_id_slug(window_id: &str, fallback_index: &str) -> String {
    let raw = window_id.trim_start_matches('@');
    let raw = if raw.is_empty() {
        if fallback_index.is_empty() {
            "0"
        } else {
            fallback_index
        }
    } else {
        raw
    };
    format!("w{raw}")
}

pub(crate) fn _default_auto_workspace_path(
    session_name: &str,
    window_id: &str,
    fallback_index: &str,
) -> PathBuf {
    PathBuf::from(format!(
        "/tmp/hive-{session_name}-{}",
        _window_id_slug(window_id, fallback_index)
    ))
}

/// Window-id-derived team name — the overflow scheme behind the pool.
pub(crate) fn _default_team_name_for_window(
    session_name: &str,
    window_id: &str,
    window_index: &str,
) -> String {
    format!(
        "{session_name}-{}",
        _window_id_slug(window_id, window_index)
    )
}

/// Group tags and qualified `@hive-agent` prefixes claimed by live panes.
pub(crate) fn _claimed_group_namespaces() -> HashSet<String> {
    let mut claimed = HashSet::new();
    for pane in tmux::list_panes_all() {
        let group = pane.group.trim();
        if !group.is_empty() {
            claimed.insert(group.to_string());
        }
        let agent = pane.agent.trim();
        if let Some((prefix, _)) = agent.split_once('.') {
            if !prefix.is_empty() {
                claimed.insert(prefix.to_string());
            }
        }
    }
    claimed
}

/// Short memorable name for a new team; window-id scheme as overflow.
pub(crate) fn _pick_team_name(session_name: &str, window_id: &str, window_index: &str) -> String {
    let mut used: HashSet<String> = tmux::list_panes_all()
        .into_iter()
        .filter(|p| !p.team.is_empty())
        .map(|p| p.team)
        .collect();
    used.extend(_claimed_group_namespaces());
    // The registry is the name authority: a headless or detached team owns
    // its name until `hive delete` — a pool pick must never clobber it.
    for entry in crate::registry::list_entries() {
        let team = map_str(&entry, "team");
        if !team.is_empty() {
            used.insert(team);
        }
    }
    for candidate in TEAM_NAME_POOL {
        if !used.contains(candidate) {
            return candidate.to_string();
        }
    }
    _default_team_name_for_window(session_name, window_id, window_index)
}

pub(crate) fn _remember_context(team: &str, workspace: &str, agent: &str) {
    let current = crate::context::load_current_context();
    let get = |key: &str| current.get(key).cloned().unwrap_or_default();
    let team = if team.is_empty() {
        get("team")
    } else {
        team.to_string()
    };
    let workspace = if workspace.is_empty() {
        get("workspace")
    } else {
        workspace.to_string()
    };
    let agent = if agent.is_empty() {
        get("agent")
    } else {
        agent.to_string()
    };
    let _ = crate::context::save_current_context(&team, &workspace, &agent);
}

pub(crate) fn _parse_entries(entries: &[String]) -> Map<String, Value> {
    match crate::bus::parse_key_value(entries) {
        Ok(map) => map,
        Err(err) => fail(&err.to_string()),
    }
}

pub(crate) fn _team_window_identity(t: &mut Team) -> (String, String) {
    let window_target = if !t.tmux_window.is_empty() {
        t.tmux_window.clone()
    } else {
        tmux::get_current_window_target().unwrap_or_default()
    };
    let mut window_id = t.tmux_window_id.clone();
    if window_id.is_empty() && !window_target.is_empty() {
        window_id = tmux::get_window_id(&window_target).unwrap_or_default();
    }
    if window_id.is_empty() {
        window_id = tmux::get_current_window_id().unwrap_or_default();
    }
    if !window_target.is_empty() && t.tmux_window.is_empty() {
        t.tmux_window = window_target.clone();
    }
    if !window_id.is_empty() && t.tmux_window_id.is_empty() {
        t.tmux_window_id = window_id.clone();
    }
    (window_target, window_id)
}

/// CLI-side `_ensure_team_hived` (mutates the team like the Python original).
pub(crate) fn _ensure_team_hived(t: &mut Team, workspace: &str) -> Option<i32> {
    let (window_target, window_id) = _team_window_identity(t);
    crate::hived::ensure_hived(workspace, &t.name, &window_target, &window_id)
}

/// Seam used by flow.rs (return ignored there; team not mutated).
pub fn ensure_team_hived(t: &Team, workspace: &Path) {
    let mut clone = t.clone();
    let _ = _ensure_team_hived(&mut clone, &workspace.to_string_lossy());
}

pub(crate) fn _augment_team_payload_with_runtime(
    t: &mut Team,
    mut payload: Map<String, Value>,
) -> Map<String, Value> {
    let ws = resolve_workspace(Some(&*t), false).unwrap_or_default();
    if ws.is_empty() {
        return payload;
    }
    let _ = _ensure_team_hived(t, &ws);
    let runtime = match crate::hived::request_team_runtime(&ws, &t.name) {
        Some(r) if !r.is_empty() => r,
        _ => return payload,
    };
    if runtime.get("ok") == Some(&Value::Bool(false)) {
        return payload;
    }
    let members_runtime = match runtime.get("members").and_then(Value::as_object) {
        Some(m) => m.clone(),
        None => return payload,
    };
    if let Some(Value::Array(members)) = payload.get_mut("members") {
        for member in members.iter_mut() {
            let member = match member.as_object_mut() {
                Some(m) => m,
                None => continue,
            };
            let name = map_str(member, "name");
            let runtime_fields = match members_runtime.get(&name).and_then(Value::as_object) {
                Some(f) => f,
                None => continue,
            };
            for key in [
                "alive",
                "cliAlive",
                "busy",
                "model",
                "sessionId",
                "inputState",
                "inputReason",
                "turnPhase",
            ] {
                match runtime_fields.get(key) {
                    None | Some(Value::Null) => continue,
                    Some(Value::String(s)) if s.is_empty() => continue,
                    Some(value) => {
                        member.insert(key.to_string(), value.clone());
                    }
                }
            }
        }
    }
    if let Some(Value::Array(needs_answer)) = runtime.get("needsAnswer") {
        if !needs_answer.is_empty() {
            payload.insert(
                "needsAnswer".to_string(),
                Value::Array(needs_answer.clone()),
            );
        }
    }
    payload
}

pub(crate) fn _should_show_description(desc: Option<&Value>) -> bool {
    match desc {
        Some(Value::String(s)) if !s.is_empty() => !s.starts_with("auto-init from "),
        _ => false,
    }
}

pub(crate) fn _team_status_payload(t: &mut Team) -> Map<String, Value> {
    let status = t.status();
    let mut payload = _augment_team_payload_with_runtime(t, status);
    // The flow runner's mailbox is a reserved address, not a member — list
    // it beside the roster so "hive team can't find flow" never reads as
    // "my report was lost".
    payload.insert(
        "mailboxes".to_string(),
        serde_json::json!([{"addr": "flow.run", "kind": "flow", "delivery": "bus"}]),
    );
    if !_should_show_description(payload.get("description")) {
        payload.shift_remove("description");
    }
    let discovered = if tmux::is_inside_tmux() {
        _discover_tmux_binding()
    } else {
        Map::new()
    };
    let session = _session_member_binding();
    if map_str(&discovered, "team") == t.name && !map_str(&discovered, "agent").is_empty() {
        payload.insert(
            "self".to_string(),
            Value::String(map_str(&discovered, "agent")),
        );
    } else if map_str(&session, "team") == t.name && !map_str(&session, "agent").is_empty() {
        // A joined/creator session has no pane tags; its roster row is live
        // truth, fresher than the saved context file below.
        payload.insert(
            "self".to_string(),
            Value::String(map_str(&session, "agent")),
        );
    } else {
        let ctx = crate::context::load_current_context();
        let ctx_team = ctx.get("team").cloned().unwrap_or_default();
        let ctx_agent = ctx.get("agent").cloned().unwrap_or_default();
        if ctx_team == t.name && !ctx_agent.is_empty() {
            payload.insert("self".to_string(), Value::String(ctx_agent));
        }
    }
    _add_runtime_location_fields(&mut payload);
    payload
}

pub(crate) fn _resolve_target_pane() -> String {
    match tmux::get_current_pane_id() {
        Some(current) if !current.is_empty() => current,
        _ => fail("cannot determine target pane (run inside tmux)"),
    }
}

pub(crate) fn _resolve_artifact_path(artifact: &str, workspace: &str) -> String {
    if artifact.is_empty() {
        return String::new();
    }
    if artifact == "-" {
        // Read from stdin, save to workspace artifacts
        if workspace.is_empty() {
            fail("--artifact - requires a workspace (run inside a team)");
        }
        let heredoc_recipe = "  hive <cmd> <args> --artifact - <<'EOF'\n  # details\n  EOF";
        if stdin_isatty() {
            fail(&format!(
                "--artifact - expects piped stdin but a terminal is attached; \
                 use a heredoc instead:\n{heredoc_recipe}"
            ));
        }
        let mut content = String::new();
        use std::io::Read;
        let _ = std::io::stdin().read_to_string(&mut content);
        if content.is_empty() {
            fail(&format!(
                "--artifact - received empty stdin; pipe content in or use a heredoc:\n{heredoc_recipe}"
            ));
        }
        let ws_artifacts = Path::new(workspace).join("artifacts");
        let _ = std::fs::create_dir_all(&ws_artifacts);
        // Short random id — file name is never parsed by downstream code.
        let filename = format!("{}.md", token_urlsafe4());
        let path = ws_artifacts.join(filename);
        let _ = std::fs::write(&path, &content);
        return path.to_string_lossy().into_owned();
    }
    let resolved_artifact = expanduser(artifact);
    if !Path::new(&resolved_artifact).exists() {
        fail(&format!("artifact not found: {resolved_artifact}"));
    }
    resolved_artifact
}

pub(crate) fn _resolve_spawn_cli_name(cli_name: Option<&str>) -> String {
    if let Some(cli) = cli_name {
        if crate::agent_cli::AGENT_CLI_NAMES.contains(&cli) {
            return cli.to_string();
        }
    }
    let current_pane = tmux::get_current_pane_id().unwrap_or_default();
    if !current_pane.is_empty() {
        let option_cli = crate::agent_cli::normalize_command(
            &tmux::get_pane_option(&current_pane, "hive-cli").unwrap_or_default(),
        );
        if crate::agent_cli::AGENT_CLI_NAMES.contains(&option_cli.as_str()) {
            return option_cli;
        }
        if let Some(profile) = crate::agent_cli::detect_profile_for_pane(&current_pane) {
            return profile.name.to_string();
        }
    }
    "claude".to_string()
}

// ---------------------------------------------------------------------------
// Tests (ported from tests/unit — logic-level only)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- tests/unit/test_env_binding.py (env-lane cases) ---

    fn clear_tmux_env() {
        for key in [
            "TMUX",
            "TMUX_PANE",
            "CODEX_THREAD_ID",
            "CLAUDE_CODE_MESSAGING_SOCKET",
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn test_env_binding_resolves_identity_and_workspace() {
        let _guard = crate::registry::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path().join(".hive"));
        clear_tmux_env();
        let mut member = Map::new();
        member.insert("name".to_string(), Value::String("rex".to_string()));
        member.insert("cli".to_string(), Value::String("grok".to_string()));
        assert_eq!(
            crate::registry::record_team("honey", "/tmp/ws-h", "1.0", &[member], "").unwrap(),
            "written"
        );
        std::env::set_var("HIVE_TEAM", "honey");
        std::env::set_var("HIVE_MEMBER", "rex");

        let binding = _discover_tmux_binding();

        assert_eq!(map_str(&binding, "team"), "honey");
        assert_eq!(map_str(&binding, "agent"), "rex");
        assert_eq!(map_str(&binding, "workspace"), "/tmp/ws-h");
        assert_eq!(map_str(&binding, "pane"), "");
    }

    #[test]
    fn test_default_team_falls_back_to_session_membership() {
        // A session that created or joined a team headless has no pane tags
        // and no spawn env; its scope lives in the registry row keyed by its
        // sessionId — the same authority `hive send` resolves guests by.
        let _guard = crate::registry::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path().join(".hive"));
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path().join(".claude"));
        clear_tmux_env();
        std::env::remove_var("HIVE_TEAM");
        std::env::remove_var("HIVE_MEMBER");
        let mut member = Map::new();
        member.insert("name".to_string(), Value::String("orch".to_string()));
        member.insert("cli".to_string(), Value::String("claude".to_string()));
        member.insert("sessionId".to_string(), Value::String("s-wasp".to_string()));
        assert_eq!(
            crate::registry::record_team("wasp", "/tmp/ws-w", "1.0", &[member], "").unwrap(),
            "written"
        );
        let sessions = tmp.path().join(".claude").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("me.json"),
            serde_json::json!({
                "name": "me",
                "pid": std::process::id(),
                "messagingSocketPath": "/tmp/me.sock",
                "sessionId": "s-wasp",
            })
            .to_string(),
        )
        .unwrap();
        std::env::set_var("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/me.sock");

        assert_eq!(_default_team().as_deref(), Some("wasp"));
        assert_eq!(_default_agent().as_deref(), Some("orch"));
        let binding = _session_member_binding();
        assert_eq!(map_str(&binding, "workspace"), "/tmp/ws-w");

        // A session on no roster resolves nothing.
        std::env::set_var("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/ghost.sock");
        assert_eq!(_default_team(), None);
        std::env::remove_var("CLAUDE_CODE_MESSAGING_SOCKET");
        std::env::remove_var("CLAUDE_CONFIG_DIR");
    }

    #[test]
    fn test_spawn_env_outranks_session_membership() {
        // HIVE_TEAM/HIVE_MEMBER is the explicit spawn identity; the session
        // rung is the fallback, never an override.
        let _guard = crate::registry::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path().join(".hive"));
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path().join(".claude"));
        clear_tmux_env();
        let mut member = Map::new();
        member.insert("name".to_string(), Value::String("orch".to_string()));
        member.insert("sessionId".to_string(), Value::String("s-wasp".to_string()));
        assert_eq!(
            crate::registry::record_team("wasp", "/tmp/ws-w", "1.0", &[member], "").unwrap(),
            "written"
        );
        let sessions = tmp.path().join(".claude").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("me.json"),
            serde_json::json!({
                "name": "me",
                "pid": std::process::id(),
                "messagingSocketPath": "/tmp/me.sock",
                "sessionId": "s-wasp",
            })
            .to_string(),
        )
        .unwrap();
        std::env::set_var("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/me.sock");
        std::env::set_var("HIVE_TEAM", "honey");
        std::env::set_var("HIVE_MEMBER", "rex");

        assert_eq!(_default_team().as_deref(), Some("honey"));
        assert_eq!(_default_agent().as_deref(), Some("rex"));

        std::env::remove_var("HIVE_TEAM");
        std::env::remove_var("HIVE_MEMBER");
        std::env::remove_var("CLAUDE_CODE_MESSAGING_SOCKET");
        std::env::remove_var("CLAUDE_CONFIG_DIR");
    }

    #[test]
    fn test_env_binding_needs_both_markers() {
        let _guard = crate::registry::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path().join(".hive"));
        clear_tmux_env();
        std::env::set_var("HIVE_TEAM", "honey");
        std::env::remove_var("HIVE_MEMBER");
        assert!(_discover_tmux_binding().is_empty());
    }

    // --- pure naming / addressing helpers ---

    #[test]
    fn test_window_id_slug_prefers_window_id() {
        assert_eq!(_window_id_slug("@42", "3"), "w42");
        assert_eq!(_window_id_slug("", "3"), "w3");
        assert_eq!(_window_id_slug("", ""), "w0");
    }

    #[test]
    fn test_default_team_name_for_window_uses_slug() {
        assert_eq!(_default_team_name_for_window("dev", "@7", "1"), "dev-w7");
        assert_eq!(_default_team_name_for_window("dev", "", "5"), "dev-w5");
    }

    #[test]
    fn test_default_auto_workspace_path_shape() {
        assert_eq!(
            _default_auto_workspace_path("dev", "@9", "0"),
            PathBuf::from("/tmp/hive-dev-w9")
        );
    }

    #[test]
    fn test_root_send_protocol_rejects_empty_and_structured_bodies() {
        assert_eq!(
            _root_send_protocol_error("  "),
            Some("new root send requires a short body summary".to_string())
        );
        assert!(_root_send_protocol_error("ack").is_none());
        let long_body = "x".repeat(501);
        assert!(_root_send_protocol_error(&long_body).is_some());
    }

    #[test]
    fn test_token_urlsafe4_shape() {
        let token = token_urlsafe4();
        assert_eq!(token.len(), 6);
        assert!(token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn test_should_show_description_filters_auto_init() {
        assert!(!_should_show_description(None));
        assert!(!_should_show_description(Some(&Value::String(
            String::new()
        ))));
        assert!(!_should_show_description(Some(&Value::String(
            "auto-init from tmux dev (dev:1)".to_string()
        ))));
        assert!(_should_show_description(Some(&Value::String(
            "real description".to_string()
        ))));
    }
}
