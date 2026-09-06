use std::collections::HashSet;
use std::path::Path;

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};

use super::*;
use crate::team::{Team, LEAD_AGENT_NAME};
use crate::tmux;

pub(crate) const _TMUX_REQUIRED_MESSAGE: &str =
    "Hive requires tmux. Start or attach to a tmux session first.";

/// Refusal for an engine whose own session id names no roster row. Told
/// apart from `_TMUX_REQUIRED_MESSAGE` because the caller has no terminal to
/// go find: it is an engine subprocess, and its identity is the broken part.
pub(crate) const _UNROSTERED_ENGINE_MESSAGE: &str =
    "this engine's session names nobody on any team's roster \
     (the member was killed, or the team deleted)";

/// Env an engine mints for its own subprocesses. A process carrying one of
/// these is an engine context: it is some member, or it is nobody — it is
/// never the human at the orch's keyboard.
pub(crate) const _ENGINE_MARKER_ENV: [&str; 3] = [
    "CODEX_THREAD_ID",
    "GROK_SESSION_ID",
    "CLAUDE_CODE_MESSAGING_SOCKET",
];

// Verbs that never need a tmux context — plus the team verbs, which read the
// registry (the truth layer) and address the team's window by id, so a
// caller outside tmux or in another session reaches it the same way. `flow`
// rides the same doctrine, and `flow node --team` exists for callers without
// a pane identity (a workflow proxy subagent, a desktop session).
pub(crate) const _TMUX_OPTIONAL_ROOT_COMMANDS: &[&str] = &[
    "plugin",
    "config",
    "shell-init",
    "codex",
    "claude",
    "grok",
    "resume-hint",
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
    "flow",
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
pub(crate) use crate::pyval::{py_float_str, truthy};

/// The hive binary that tmux hooks, the flow dock and the cvim asset call
/// back into. HIVE_BIN overrides `current_exe` — `hive cvim` exports it for
/// the bash asset, and integration tests (whose current_exe is the test
/// harness) point hooks at the real binary with it.
pub(crate) fn self_exe() -> String {
    let overridden = env_string("HIVE_BIN");
    if !overridden.is_empty() {
        return overridden;
    }
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hive".to_string())
}

/// Python `shlex.quote`: alphanumerics and `_@%+=:,./-` pass through bare,
/// anything else is wrapped in single quotes.
pub(crate) fn shlex_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    let safe = value.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '_' | '-')
    });
    if safe {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

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

/// Roster identity of the engine session this process runs inside.
///
/// The session rung of the scope ladder: pane tags cover a caller sitting in
/// a member pane, but an engine's tool subprocess carries no pane of its own
/// — its scope is the registry row keyed by its sessionId.
/// Three engines key that row, each by an id it mints for itself and hands
/// to its own tool subprocesses: a codex tool by its `CODEX_THREAD_ID`
/// (restricted to codex rows), a grok tool by its `GROK_SESSION_ID`
/// (restricted to grok rows), a Claude session by its messaging socket.
/// Inherited env is never identity, which is why nothing below this rung
/// reads env at all. This is the same authority `hive send` resolves guest
/// senders and the codex-native gate by.
pub(crate) fn _session_member_binding() -> Map<String, Value> {
    let Some((team, agent)) = _codex_thread_member_env()
        .or_else(_grok_session_member_env)
        .or_else(_claude_session_member)
    else {
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

fn _claude_session_member() -> Option<(String, String)> {
    let session = crate::adapters::claude_sessions::self_session()?;
    _registry_member_for_session(&session.session_id)
}

/// The pane's own tags, or — with no pane identity — the session row.
pub(crate) fn _discover_tmux_binding() -> Map<String, Value> {
    let pane = _discover_pane_binding();
    if pane.is_empty() {
        _session_member_binding()
    } else {
        pane
    }
}

/// The current pane's own tags, and nothing else: empty outside tmux, on
/// an untagged pane, or on a tagged pane that names no agent or role.
fn _discover_pane_binding() -> Map<String, Value> {
    if !tmux::is_inside_tmux() {
        return Map::new();
    }
    let current_pane = match tmux::get_current_pane_id() {
        Some(p) if !p.is_empty() => p,
        _ => return Map::new(),
    };
    let team_name = match tmux::get_pane_option(&current_pane, "hive-team") {
        Some(t) if !t.is_empty() => t,
        _ => return Map::new(),
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

/// The binding ladder, one walk: pane tags, then the engine's own session
/// row. Each lane is read at most once, and the session row only when the
/// pane's tags did not settle *pick*.
fn _first_binding<T>(pick: impl Fn(&Map<String, Value>) -> Option<T>) -> Option<T> {
    [
        _discover_pane_binding as fn() -> Map<String, Value>,
        _session_member_binding,
    ]
    .into_iter()
    .find_map(|lane| pick(&lane()))
}

/// The first non-empty *field* of the binding ladder.
fn _default_binding_field(field: &str) -> Option<String> {
    _first_binding(|binding| Some(map_str(binding, field)).filter(|value| !value.is_empty()))
}

pub(crate) fn _default_team() -> Option<String> {
    _default_binding_field("team")
}

pub(crate) fn _default_agent() -> Option<String> {
    _default_binding_field("agent")
}

pub(crate) fn _resolve_sender(agent_name: Option<&str>) -> String {
    agent_name
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(_default_agent)
        .or_else(_unresolved_sender_fallback)
        .unwrap_or_else(|| {
            fail(
                "cannot resolve own member identity: this engine is on no roster \
                 (a codex thread, grok session or Claude session not recorded by \
                 any team) — join a team first, or run from a bound pane",
            )
        })
}

/// Sender when the identity ladder resolves nothing.
///
/// Only a human in a plain tmux shell speaks as orch: the shell carries no
/// engine marker and sits in a real tmux client. A process carrying an
/// engine's marker (codex thread, grok session, Claude messaging socket) or
/// running with no tmux client at all is a member context, and an unresolved
/// member must not sign as orch.
fn _unresolved_sender_fallback() -> Option<String> {
    if _engine_marker_env() || env_string("TMUX").is_empty() {
        return None;
    }
    Some(LEAD_AGENT_NAME.to_string())
}

/// True when this process carries an engine's own identity marker.
pub(crate) fn _engine_marker_env() -> bool {
    _ENGINE_MARKER_ENV
        .iter()
        .any(|key| !env_string(key).trim().is_empty())
}

// ---------------------------------------------------------------------------
// Team / workspace resolution
// ---------------------------------------------------------------------------

pub(crate) fn _load_team(team: &str, prefer_pane: &str) -> Result<Team> {
    Team::load(team, prefer_pane).map_err(|_| anyhow!("team '{team}' not found"))
}

/// Addressing order: explicit team -> binding discovery (pane tags, then the
/// engine's own session row). An explicit team is the caller's intent — it
/// loads from the registry wherever the caller happens to be.
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
    if let Some(hint) = crate::message::body_warning_hint(body) {
        eprintln!("{}", crate::message::format_body_warning(command, &hint));
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
    if crate::message::body_warning_hint(summary).is_some() {
        return Some(
            "new root send body must stay short and unstructured; move details into --artifact \
             (prefer `--artifact -` unless you already have a file)"
                .to_string(),
        );
    }
    None
}

pub(crate) fn _validate_root_send_protocol(body: &str) {
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
    // The registry is the name authority: a team whose window is gone owns
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
    let Some(runtime) =
        crate::team::usable_runtime(crate::hived::request_team_runtime(&ws, &t.name))
    else {
        return payload;
    };
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
    let me = _self_member_for_team(&t.name);
    if !me.is_empty() {
        payload.insert("self".to_string(), Value::String(me));
    }
    _add_runtime_location_fields(&mut payload);
    payload
}

/// Which member of *team* this process is, or "" when it is none of them.
///
/// The scope ladder, strongest evidence first: the pane's own tags, the
/// roster row keyed by this engine's own session id, and only then the
/// saved context file. The session rung is what answers outside tmux,
/// where a member's tool has no pane: the context file there was written
/// by whoever spawned it and would answer with the orch — see
/// [`_session_member_binding`].
pub(crate) fn _self_member_for_team(team: &str) -> String {
    match _self_binding() {
        Some((bound_team, member)) if bound_team == team => member,
        _ => String::new(),
    }
}

/// (team, member) this process is, by the strongest rung that answers.
///
/// The first rung to resolve settles it, even when it names another team:
/// this engine is that member, so a weaker rung claiming the team being
/// asked about is a leftover, not a second identity.
fn _self_binding() -> Option<(String, String)> {
    let bound = _first_binding(|binding| {
        let team = map_str(binding, "team");
        let agent = map_str(binding, "agent");
        (!team.is_empty() && !agent.is_empty()).then_some((team, agent))
    });
    if bound.is_some() {
        return bound;
    }
    let ctx = crate::context::load_current_context();
    let team = ctx.get("team").cloned().unwrap_or_default();
    let agent = ctx.get("agent").cloned().unwrap_or_default();
    (!team.is_empty() && !agent.is_empty()).then_some((team, agent))
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
    use crate::testenv::EnvGuard;

    // --- tests/unit/test_env_binding.py (env-lane cases) ---

    /// Isolated HIVE_HOME and CLAUDE_CONFIG_DIR under *tmp*, with no engine
    /// identity inherited from the shell, for the test's lifetime.
    fn isolated(tmp: &std::path::Path) -> EnvGuard {
        let mut env = EnvGuard::cleared(&crate::testenv::IDENTITY_VARS);
        env.set("HIVE_HOME", tmp.join(".hive"));
        env.set("CLAUDE_CONFIG_DIR", tmp.join(".claude"));
        env
    }

    /// A grok member row on *team*, keyed by *session_id*.
    fn record_grok_member(team: &str, workspace: &str, name: &str, session_id: &str) {
        let mut member = Map::new();
        member.insert("name".to_string(), Value::String(name.to_string()));
        member.insert("cli".to_string(), Value::String("grok".to_string()));
        member.insert(
            "sessionId".to_string(),
            Value::String(session_id.to_string()),
        );
        assert_eq!(
            crate::registry::record_team(team, workspace, "1.0", &[member], "").unwrap(),
            "written"
        );
    }

    #[test]
    fn test_grok_session_id_resolves_identity_and_workspace() {
        // A grok member's tool has no pane, no thread and no Claude
        // socket: its leader exports GROK_SESSION_ID into every tool
        // subprocess, and that id keys its grok roster row.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut env = isolated(tmp.path());
        record_grok_member("honey", "/tmp/ws-h", "rex", "s-rex");
        env.set("GROK_SESSION_ID", "s-rex");

        let binding = _session_member_binding();
        assert_eq!(map_str(&binding, "team"), "honey");
        assert_eq!(map_str(&binding, "agent"), "rex");
        assert_eq!(map_str(&binding, "workspace"), "/tmp/ws-h");
        assert_eq!(map_str(&binding, "pane"), "");
        assert_eq!(_discover_tmux_binding(), binding);
        assert_eq!(_default_team().as_deref(), Some("honey"));
        assert_eq!(_default_agent().as_deref(), Some("rex"));
        assert_eq!(_resolve_sender(None), "rex");
        assert_eq!(_self_member_for_team("honey"), "rex");
        // another team's status payload is not this member's identity
        assert_eq!(_self_member_for_team("wasp"), "");

        // the member was killed: the leader's env survives it, the roster
        // does not, and nothing signs as it
        record_grok_member("honey", "/tmp/ws-h", "ant", "s-ant");
        assert!(_session_member_binding().is_empty());
        assert_eq!(_default_team(), None);
        assert_eq!(_default_agent(), None);
        assert_eq!(_self_member_for_team("honey"), "");
    }

    #[test]
    fn test_grok_session_ignores_a_row_of_another_cli() {
        // The row match is the identity: a claude row carrying the same id
        // is a stranger, exactly as it is for a codex thread.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut env = isolated(tmp.path());
        let mut member = Map::new();
        member.insert("name".to_string(), Value::String("orch".to_string()));
        member.insert("cli".to_string(), Value::String("claude".to_string()));
        member.insert("sessionId".to_string(), Value::String("s-both".to_string()));
        crate::registry::record_team("wasp", "/tmp/ws-w", "1.0", &[member], "").unwrap();
        env.set("GROK_SESSION_ID", "s-both");

        assert!(_session_member_binding().is_empty());
        assert_eq!(_default_team(), None);
        assert_eq!(_default_agent(), None);
        assert_eq!(_self_member_for_team("wasp"), "");
    }

    #[test]
    fn test_grok_session_outranks_the_saved_context_file() {
        // The saved context file was written by the spawner and names the
        // orch; the member's own session row must outrank it.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut env = isolated(tmp.path());
        record_grok_member("hornet", "/tmp/ws-hn", "bee", "s-bee");
        crate::context::save_context_for_pane("", "hornet", "/tmp/ws-hn", LEAD_AGENT_NAME).unwrap();

        // context file alone: the orch answers
        assert_eq!(_self_member_for_team("hornet"), LEAD_AGENT_NAME);

        env.set("GROK_SESSION_ID", "s-bee");
        assert_eq!(_self_member_for_team("hornet"), "bee");
    }

    #[test]
    fn test_default_team_falls_back_to_session_membership() {
        // A session that created or joined a team from outside tmux has no
        // pane tags; its scope lives in the registry row keyed by its
        // sessionId — the same authority `hive send` resolves guests by.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut env = isolated(tmp.path());
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
        env.set("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/me.sock");

        assert_eq!(_default_team().as_deref(), Some("wasp"));
        assert_eq!(_default_agent().as_deref(), Some("orch"));
        let binding = _session_member_binding();
        assert_eq!(map_str(&binding, "workspace"), "/tmp/ws-w");

        // A session on no roster resolves nothing.
        env.set("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/ghost.sock");
        assert_eq!(_default_team(), None);
    }

    #[test]
    fn test_default_team_resolves_a_codex_member_by_its_thread() {
        // A codex member's tool with no pane record and no Claude socket:
        // its CODEX_THREAD_ID keys a codex roster row.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut env = isolated(tmp.path());
        env.set("CODEX_HOME", tmp.path().join(".codex"));
        let mut member = Map::new();
        member.insert("name".to_string(), Value::String("review".to_string()));
        member.insert("cli".to_string(), Value::String("codex".to_string()));
        member.insert(
            "sessionId".to_string(),
            Value::String("01aa-headless".to_string()),
        );
        assert_eq!(
            crate::registry::record_team("rr", "/tmp/ws-rr", "1.0", &[member], "").unwrap(),
            "written"
        );
        env.set("CODEX_THREAD_ID", "01aa-headless");

        assert_eq!(_default_team().as_deref(), Some("rr"));
        assert_eq!(_default_agent().as_deref(), Some("review"));
        assert_eq!(_resolve_sender(None), "review");
        let binding = _session_member_binding();
        assert_eq!(map_str(&binding, "workspace"), "/tmp/ws-rr");
        assert_eq!(map_str(&binding, "pane"), "");

        // A thread on no roster resolves nothing.
        env.set("CODEX_THREAD_ID", "01aa-ghost");
        assert_eq!(_default_team(), None);
        assert_eq!(_default_agent(), None);
    }

    #[test]
    fn test_codex_thread_ignores_claude_row_with_same_session_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut env = isolated(tmp.path());
        env.set("CODEX_HOME", tmp.path().join(".codex"));
        let mut member = Map::new();
        member.insert("name".to_string(), Value::String("orch".to_string()));
        member.insert("cli".to_string(), Value::String("claude".to_string()));
        member.insert(
            "sessionId".to_string(),
            Value::String("01aa-shared".to_string()),
        );
        crate::registry::record_team("wasp", "/tmp/ws-w", "1.0", &[member], "").unwrap();
        env.set("CODEX_THREAD_ID", "01aa-shared");

        assert_eq!(_default_team(), None);
        assert_eq!(_default_agent(), None);
        assert!(_session_member_binding().is_empty());
    }

    #[test]
    fn test_unresolved_sender_defaults_to_orch_only_for_tmux_shell() {
        let mut env = EnvGuard::cleared(&crate::testenv::IDENTITY_VARS);
        // no tmux client, no engine marker: nothing to sign as
        assert_eq!(_unresolved_sender_fallback(), None);
        // a human shell inside a tmux client speaks as orch
        env.set("TMUX", "/tmp/tmux-0/default,1,0");
        assert_eq!(_unresolved_sender_fallback().as_deref(), Some("orch"));
        // an engine marker makes it a member context even inside tmux
        for key in _ENGINE_MARKER_ENV {
            env.set(key, "x");
            assert_eq!(_unresolved_sender_fallback(), None, "{key}");
            env.remove(key);
        }
    }

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
