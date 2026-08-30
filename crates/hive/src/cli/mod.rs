//! CLI entry point for hive — port of `src/hive/cli.py` (skeleton half).
//!
//! This module owns the clap command tree for the ENTIRE surface, `pub fn
//! main()`, and the shared helpers both command halves use. Core registry
//! verbs live in `core_cmds`; everything else routes to `rest` (ported
//! separately as `cli/rest.rs`).

pub mod core_cmds;
pub mod rest;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{Map, Value};

use crate::agent::Agent;
use crate::team::{Team, LEAD_AGENT_NAME};
use crate::tmux;
use crate::tmux::PaneInfo;

// ---------------------------------------------------------------------------
// Help layout tables (SectionedHelpGroup equivalent)
// ---------------------------------------------------------------------------

pub(crate) const _COMMAND_HELP_SECTIONS: &[(&str, &str)] = &[
    // Daily — per-turn agent collaboration loop.
    ("team", "Daily"),
    ("send", "Daily"),
    ("ccd", "Daily"),
    ("notify", "Daily"),
    ("compact", "Daily"),
    ("skills", "Daily"),
    // Panes — bring up another agent pane (fresh or forked).
    ("fork", "Panes"),
    ("spawn", "Panes"),
    // Workflow — higher-level flows on top of Hive.
    ("flow", "Workflow"),
    ("worktree", "Workflow"),
    ("pr", "Workflow"),
    ("ls", "Workflow"),
    ("attach", "Workflow"),
    ("view", "Workflow"),
    // Team — wire up the tmux team around the current window.
    ("create", "Team"),
    ("delete", "Team"),
    ("join", "Team"),
    ("layout", "Team"),
    // Human Helpers — human-only popup + split helpers.
    ("cvim", "Human Helpers"),
    ("vim", "Human Helpers"),
    ("vfork", "Human Helpers"),
    ("hfork", "Human Helpers"),
    // Debug — troubleshooting, rarely on the happy path.
    ("doctor", "Debug"),
    ("thread", "Debug"),
    ("capture", "Debug"),
    ("inject", "Debug"),
    ("interrupt", "Debug"),
    ("kill", "Debug"),
    // Extensions.
    ("plugin", "Extensions"),
    ("config", "Extensions"),
    // Launchers — hive-managed claude/codex/grok entry points + shell integration.
    ("claude", "Launchers"),
    ("codex", "Launchers"),
    ("grok", "Launchers"),
    ("shell-init", "Launchers"),
];

pub(crate) const _COMMAND_HELP_SECTION_ORDER: &[&str] = &[
    "Daily",
    "Panes",
    "Workflow",
    "Team",
    "Human Helpers",
    "Debug",
    "Extensions",
    "Launchers",
    "Other Commands",
];

pub(crate) const _COMMAND_HELP_SECTION_DESCRIPTIONS: &[(&str, &str)] = &[
    (
        "Daily",
        "Core loop per turn: inspect context, talk to peers, pull the human in when blocked.",
    ),
    (
        "Panes",
        "Bring up another agent pane — a fresh spawn or a forked clone.",
    ),
    (
        "Workflow",
        "Higher-level flows on top of Hive: worktrees, PR anchors, team snapshots.",
    ),
    (
        "Team",
        "Create, extend, and wire up the tmux team around the current window.",
    ),
    (
        "Human Helpers",
        "Popup editor and split helpers for the human (not the model). In Claude Code / Codex, type `!hive cvim` via shell escape. Requires tmux >= 3.2.",
    ),
    (
        "Debug",
        "Troubleshoot delivery, runtime state, and low-level pane behavior. Not on the happy path.",
    ),
    (
        "Extensions",
        "Manage first-party Hive plugins (Claude Code, Codex).",
    ),
    (
        "Launchers",
        "hive-managed launchers behind the `hcodex` / `hclaude` / `hgrok` shell functions from `hive shell-init`, rarely run by hand. All arguments are forwarded verbatim, so `hive claude --help` shows claude's own help, not this wrapper's.",
    ),
];

pub(crate) const _ROOT_HELP_EXAMPLES: &str = r#"# Team lifecycle
hive create                                  # make this pane the orch of a new team
hive spawn explore --task /tmp/task.md       # spawn a member and dispatch its task atomically
hive team                                    # members + runtime state (busy / inputState / turnPhase)

# Messaging (root thread: body is a short summary, details go in --artifact)
hive send dodo "review this diff" --artifact /tmp/diff.md
hive send dodo "see report" --artifact - <<'EOF'
# Findings
- item
EOF

# Fork, spawn
hive fork                                    # split the current pane into a clone
hive spawn claude                            # bring up a new agent pane

# Debug connectivity
hive doctor dodo                             # probe a peer's connectivity"#;

pub(crate) const _TMUX_REQUIRED_MESSAGE: &str =
    "Hive requires tmux. Start or attach to a tmux session first.";

// Verbs that never need a tmux context — plus the team verbs, which read the
// registry (the truth layer) and only touch tmux when a display exists.
pub(crate) const _TMUX_OPTIONAL_ROOT_COMMANDS: &[&str] = &[
    "plugin",
    "config",
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
pub(crate) fn py_float_str(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e16 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
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
fn os_random_bytes(n: usize) -> Vec<u8> {
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
fn random_choice<'a>(options: &[&'a str]) -> &'a str {
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
    if team.is_empty() {
        None
    } else {
        Some(team)
    }
}

pub(crate) fn _default_agent() -> Option<String> {
    let binding = _discover_tmux_binding();
    let agent = map_str(&binding, "agent");
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
    if map_str(&discovered, "team") == t.name && !map_str(&discovered, "agent").is_empty() {
        payload.insert(
            "self".to_string(),
            Value::String(map_str(&discovered, "agent")),
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
// hived request seam
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn request_send_payload(
    workspace: &str,
    team: &Team,
    sender_agent: &str,
    target_agent: &str,
    body: &str,
    artifact: &str,
    reply_to: &str,
    command_name: &str,
    warn_on_long_body: bool,
) -> Result<Map<String, Value>> {
    if warn_on_long_body {
        _maybe_warn_long_body(body, command_name);
    }
    ensure_team_hived(team, Path::new(workspace));
    let payload = crate::hived::request_send(
        workspace,
        &team.name,
        sender_agent,
        &tmux::get_current_pane_id().unwrap_or_default(),
        target_agent,
        body,
        artifact,
        reply_to,
    );
    let payload = match payload {
        Some(p) if !p.is_empty() => p,
        _ => bail!("hived unavailable"),
    };
    if payload.get("ok") == Some(&Value::Bool(false)) {
        let error = match payload.get("error") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => format!("{command_name} failed"),
        };
        bail!("{error}");
    }
    let mut normalized = payload;
    normalized.shift_remove("ok");
    Ok(normalized)
}

/// Poll hived team-runtime until every agent's first skill turn completes
/// (`inputState == 'ready'`). Returns the agents still not ready at deadline.
pub fn wait_for_peer_ready(
    workspace: &str,
    team_name: &str,
    agents: &HashSet<String>,
    timeout_seconds: f64,
    poll_interval: f64,
) -> HashSet<String> {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs_f64(timeout_seconds.max(0.0));
    let mut waiting: HashSet<String> = agents.clone();
    while !waiting.is_empty() && std::time::Instant::now() < deadline {
        let runtime_payload =
            crate::hived::request_team_runtime(workspace, team_name).unwrap_or_default();
        if let Some(members) = runtime_payload.get("members").and_then(Value::as_object) {
            let mut still: HashSet<String> = HashSet::new();
            for name in &waiting {
                let ready = members
                    .get(name)
                    .and_then(Value::as_object)
                    .and_then(|m| m.get("inputState"))
                    .and_then(Value::as_str)
                    == Some("ready");
                if !ready {
                    still.insert(name.clone());
                }
            }
            waiting = still;
        }
        if !waiting.is_empty() {
            std::thread::sleep(std::time::Duration::from_secs_f64(poll_interval.max(0.0)));
        }
    }
    waiting
}

// ---------------------------------------------------------------------------
// Codex-native gate
// ---------------------------------------------------------------------------

pub(crate) fn _is_codex_tool_env() -> bool {
    !env_string("CODEX_THREAD_ID").trim().is_empty()
}

/// Pane recorded for this codex tool's own thread, or "".
pub(crate) fn _codex_pane_from_thread_env() -> String {
    let thread_id = env_string("CODEX_THREAD_ID").trim().to_string();
    if thread_id.is_empty() {
        return String::new();
    }
    crate::adapters::codex_app_server::pane_for_thread(&thread_id).unwrap_or_default()
}

pub(crate) fn _codex_relaunch_message() -> String {
    "this codex isn't hive-managed — hive runtime is degraded.\n\
     for future launches use hcodex (one-time setup, any shell):\n  \
     grep -q 'hive shell-init' ~/.zshrc || \
     echo 'eval \"$(hive shell-init zsh)\"' >> ~/.zshrc\n\
     then exit this codex (Ctrl-C twice) and run: hive codex resume"
        .to_string()
}

pub(crate) fn _require_codex_native(invoked: Option<&str>) {
    if let Some(invoked) = invoked {
        if _CODEX_NATIVE_REQUIRED_BYPASS_COMMANDS.contains(&invoked) {
            return;
        }
    }
    if !_is_codex_tool_env() || !_codex_pane_from_thread_env().is_empty() {
        return;
    }
    fail(&_codex_relaunch_message());
}

pub(crate) fn _hive_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ---------------------------------------------------------------------------
// Dead-team GC
// ---------------------------------------------------------------------------

/// Clean up legacy per-team dirs and stale contexts for unknown teams.
///
/// "Known" is the registry union with live windows, so a headless team is
/// never treated as dead. On a failed team listing nothing is touched
/// (conservative: cannot prove any team dead).
pub(crate) fn _gc_dead_teams() {
    let live_names: HashSet<String> = match crate::team::list_teams() {
        Ok(teams) => teams.iter().map(|t| map_str(t, "name")).collect(),
        Err(_) => return,
    };
    let root = crate::team::hive_home().join("teams");
    if root.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&root) {
            let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            paths.sort();
            for path in paths {
                if !path.is_dir() {
                    continue;
                }
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if !live_names.contains(&name) {
                    let _ = std::fs::remove_dir_all(&path);
                }
            }
        }
    }
    let ctx = crate::context::load_current_context();
    if let Some(team) = ctx.get("team").filter(|t| !t.is_empty()) {
        if !live_names.contains(team) {
            let _ = crate::context::clear_current_context();
        }
    }
}

// ---------------------------------------------------------------------------
// Member naming
// ---------------------------------------------------------------------------

pub(crate) fn _names_used_in_window(panes: &[PaneInfo]) -> HashSet<String> {
    panes
        .iter()
        .map(|pane| pane.agent.trim().to_string())
        .filter(|agent| !agent.is_empty())
        .collect()
}

/// Pick a short random peer name while avoiding collisions in this window.
pub(crate) fn _derive_agent_name(seen: &mut HashSet<String>) -> String {
    let available: Vec<&str> = _RANDOM_AGENT_NAMES
        .iter()
        .copied()
        .filter(|name| !seen.contains(*name))
        .collect();
    let candidate = if !available.is_empty() {
        random_choice(&available).to_string()
    } else {
        let mut suffix = 1;
        let mut candidate = format!("agent-{suffix}");
        while seen.contains(&candidate) {
            suffix += 1;
            candidate = format!("agent-{suffix}");
        }
        candidate
    };
    seen.insert(candidate.clone());
    candidate
}

pub(crate) fn _window_seen_names(t: &Team, panes: &[PaneInfo]) -> HashSet<String> {
    let mut seen_names = _names_used_in_window(panes);
    seen_names.insert(if t.lead_name.is_empty() {
        LEAD_AGENT_NAME.to_string()
    } else {
        t.lead_name.clone()
    });
    seen_names
}

pub(crate) fn _claim_member_name(name_override: &str, seen_names: &mut HashSet<String>) {
    if name_override.is_empty() {
        return;
    }
    if name_override == "flow" || name_override.starts_with("flow.") {
        fail(&format!(
            "'{name_override}' collides with the flow runner's mailbox address kind (flow.run), not a member name"
        ));
    }
    if seen_names.contains(name_override) {
        fail(&format!(
            "name '{name_override}' is already taken in this window"
        ));
    }
    seen_names.insert(name_override.to_string());
}

pub(crate) fn _resolve_pane_cli(pane: &PaneInfo) -> String {
    let source = if !pane.cli.is_empty() {
        pane.cli.clone()
    } else {
        pane.command.clone()
    };
    let mut pane_cli = crate::agent_cli::normalize_command(&source);
    if !crate::agent_cli::AGENT_CLI_NAMES.contains(&pane_cli.as_str()) {
        if let Some(profile) = crate::agent_cli::detect_profile_for_pane(&pane.pane_id) {
            pane_cli = profile.name.to_string();
        }
    }
    pane_cli
}

pub(crate) fn _classify_pane(pane: &PaneInfo) -> (&'static str, String) {
    let pane_cli = _resolve_pane_cli(pane);
    if crate::agent_cli::AGENT_CLI_NAMES.contains(&pane_cli.as_str()) {
        ("agent", pane_cli)
    } else {
        ("terminal", pane_cli)
    }
}

pub(crate) fn _hive_join_message(agent_name: &str, team_name: &str) -> String {
    format!(
        "You are '{agent_name}' in hive team '{team_name}'. \
         Context is pre-bound. Run `/hive:hive {team_name}` first and follow \
         that protocol. Hive messages will arrive inline as \
         <HIVE ...> ... </HIVE> blocks. \
         Use `hive team` to inspect the team; message any peer with \
         `hive send <name> \"<summary>\" --artifact -`."
    )
}

// ---------------------------------------------------------------------------
// Member registration
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(crate) fn _register_agent_member(
    t: &mut Team,
    pane_id: &str,
    team_name: &str,
    agent_name: &str,
    pane_cli: &str,
    cwd: &str,
    notify: bool,
    group: &str,
) -> Agent {
    let agent = Agent {
        name: agent_name.to_string(),
        team_name: team_name.to_string(),
        pane_id: pane_id.to_string(),
        model: String::new(),
        prompt: String::new(),
        cwd: cwd.to_string(),
        session_id: None,
        spawned_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
        cli: pane_cli.to_string(),
    };
    t.upsert_agent(agent.clone());
    tmux::tag_pane(pane_id, "agent", agent_name, team_name, pane_cli, group);
    let ws = resolve_workspace(Some(&*t), false).unwrap_or_default();
    if !ws.is_empty() {
        let _ = crate::context::save_context_for_pane(pane_id, team_name, &ws, agent_name);
    }
    if notify {
        if let Err(e) = agent.send(&_hive_join_message(agent_name, team_name)) {
            // Registration is transactional: a pane whose native transport
            // refused the join must not linger half-registered. Roll every
            // mutation back so a later retry starts clean.
            t.agents.retain(|a| a.name != agent_name);
            tmux::clear_pane_tags(pane_id);
            if !ws.is_empty() {
                crate::context::clear_context_for_pane(pane_id);
            }
            fail(&format!(
                "pane {pane_id} is not reachable over its native transport ({}); \
                 nothing was registered. Fix the inbox/daemon and retry, \
                 or use --no-notify to register without a reachability check.",
                e.0
            ));
        }
    }
    _registry_record_member(t, &agent);
    agent
}

/// Registry roster row for *agent*, resolving its engine identity.
pub(crate) fn _member_registry_row(agent: &Agent) -> Map<String, Value> {
    let mut session_id = agent.session_id.clone().unwrap_or_default();
    let pane_id = agent.pane_id.clone();
    let cli_name = agent.cli.clone();
    if session_id.is_empty() && !pane_id.is_empty() {
        session_id = match cli_name.as_str() {
            "claude" => crate::adapters::claude_bg::job_id_for_pane(&pane_id).unwrap_or_default(),
            "codex" => {
                crate::adapters::codex_app_server::thread_id_for_pane(&pane_id).unwrap_or_default()
            }
            "grok" => {
                crate::adapters::grok_leader::session_id_for_pane(&pane_id).unwrap_or_default()
            }
            _ => String::new(),
        };
    }
    let mut row = Map::new();
    row.insert("name".to_string(), Value::String(agent.name.clone()));
    row.insert("cli".to_string(), Value::String(cli_name));
    row.insert("model".to_string(), Value::String(agent.model.clone()));
    row.insert("sessionId".to_string(), Value::String(session_id));
    row.insert("cwd".to_string(), Value::String(agent.cwd.clone()));
    row
}

/// Register *agent* in the team registry; seed the team entry if absent.
pub(crate) fn _registry_record_member(t: &Team, agent: &Agent) {
    let team_name = t.name.clone();
    if team_name.is_empty() {
        return;
    }
    let row = _member_registry_row(agent);
    let created_at = if t.created_at == 0.0 {
        String::new()
    } else {
        py_float_str(t.created_at)
    };
    let verdict =
        crate::registry::record_member(&team_name, &row, &created_at).unwrap_or("rejected");
    if verdict == "missing" {
        let _ = crate::registry::record_team(
            &team_name,
            &t.workspace,
            &created_at,
            &[row],
            &t.tmux_window_id,
        );
    }
}

// ---------------------------------------------------------------------------
// spawn_team_agent seam (flow.rs + `hive spawn`)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn spawn_team_agent<'a>(
    t: &'a mut Team,
    team_name: &str,
    agent_name: &str,
    model: &str,
    prompt: &str,
    cwd: &str,
    skill: &str,
    extra_env: &[(String, String)],
    cli_name: Option<&str>,
) -> Result<&'a Agent> {
    let resolved_cli_name = _resolve_spawn_cli_name(cli_name);
    if let Some(model_error) = crate::agent_cli::validate_spawn_model(&resolved_cli_name, model) {
        bail!("{model_error}");
    }
    let env_map: HashMap<String, String> = extra_env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let agent = t.spawn(
        agent_name,
        model,
        prompt,
        cwd,
        skill,
        if env_map.is_empty() {
            None
        } else {
            Some(&env_map)
        },
        &resolved_cli_name,
    )?;
    let ws = resolve_workspace(Some(&*t), false).unwrap_or_default();
    let _ = crate::context::save_context_for_pane(&agent.pane_id, team_name, &ws, agent_name);
    _remember_context(team_name, &ws, LEAD_AGENT_NAME);
    _registry_record_member(t, &agent);
    t.agent_named(agent_name)
        .ok_or_else(|| anyhow!("Agent '{agent_name}' not found"))
}

// ---------------------------------------------------------------------------
// Addressing
// ---------------------------------------------------------------------------

/// Locate a pane by qualified agent name `<prefix>.<name>` across a pane
/// listing. Pure core of `_find_qualified_agent_target` for tests.
pub(crate) fn _find_qualified_agent_target_in(
    panes: &[PaneInfo],
    qualified: &str,
) -> std::result::Result<Option<(String, String)>, String> {
    if !qualified.contains('.') {
        return Ok(None);
    }
    let prefix = qualified.split('.').next().unwrap_or("");
    if prefix.is_empty() {
        return Ok(None);
    }
    let candidates: Vec<&PaneInfo> = panes
        .iter()
        .filter(|p| p.agent == qualified && !p.team.is_empty())
        .collect();
    if candidates.is_empty() {
        return Ok(None);
    }
    for p in &candidates {
        if !p.group.is_empty() && p.group != prefix {
            return Err(format!(
                "agent '{qualified}' on pane {} has conflicting @hive-group '{}' (expected '{prefix}' or empty)",
                p.pane_id, p.group
            ));
        }
    }
    if candidates.len() > 1 {
        return Err(format!(
            "agent '{qualified}' matches {} panes; qualified agent names must be unique",
            candidates.len()
        ));
    }
    Ok(Some((
        candidates[0].team.clone(),
        candidates[0].agent.clone(),
    )))
}

pub(crate) fn _find_qualified_agent_target(
    qualified: &str,
) -> std::result::Result<Option<(String, String)>, String> {
    _find_qualified_agent_target_in(&tmux::list_panes_all(), qualified)
}

/// Split `<team>.<member>` when the prefix names an existing team.
///
/// Team existence is the registry first (a headless team has no window),
/// the window scan second (a live pre-registry team). Returns
/// `(team, member)` or `("", addr)` when the prefix names no team.
pub(crate) fn _split_team_address(addr: &str) -> (String, String) {
    if !addr.contains('.') {
        return (String::new(), addr.to_string());
    }
    let (prefix, rest) = addr.split_once('.').unwrap_or(("", addr));
    if prefix.is_empty() || rest.is_empty() {
        return (String::new(), addr.to_string());
    }
    if crate::registry::load(prefix).is_some() {
        return (prefix.to_string(), rest.to_string());
    }
    let window_claims = crate::team::_find_team_window(prefix, "")
        .map(|(window, _)| !window.is_empty())
        .unwrap_or(false);
    if window_claims {
        return (prefix.to_string(), rest.to_string());
    }
    (String::new(), addr.to_string())
}

/// Resolve the team that owns *to_agent* for a send.
///
/// Qualified names (`<group>.<name>`) bypass the current-window check and
/// load the target pane's team directly, so cross-team sends work across
/// tmux windows. Bare names fall back to the caller's scoped team.
pub(crate) fn _resolve_send_target_team(to_agent: &str) -> (String, Team) {
    if to_agent.contains('.') && to_agent != "flow.run" {
        let resolved = match _find_qualified_agent_target(to_agent) {
            Ok(resolved) => resolved,
            Err(err) => fail(&err),
        };
        let (target_team_name, _) = match resolved {
            Some(pair) => pair,
            None => fail(&format!(
                "agent '{to_agent}' not found in any team \
                 (check @hive-agent tag on the target pane)"
            )),
        };
        let team = ok_or_fail(_load_team(&target_team_name, ""));
        return (target_team_name, team);
    }
    let (team_name, t) = ok_or_fail(resolve_scoped_team(None, true));
    (
        team_name.expect("required resolve returned no team"),
        t.expect("required resolve returned no team"),
    )
}

/// (team, member) whose recorded engine identity is *session_id*.
pub(crate) fn _registry_member_for_session(session_id: &str) -> Option<(String, String)> {
    if session_id.is_empty() {
        return None;
    }
    for entry in crate::registry::list_entries() {
        if let Some(members) = entry.get("members").and_then(Value::as_array) {
            for m in members {
                if let Some(m) = m.as_object() {
                    if map_str(m, "sessionId") == session_id {
                        return Some((map_str(&entry, "team"), map_str(m, "name")));
                    }
                }
            }
        }
    }
    None
}

/// Target resolution for a Claude-session guest (outside tmux).
pub(crate) fn _resolve_guest_send_target(to_agent: &str, team: &str) -> (String, Team) {
    if to_agent == "flow.run" {
        let me = crate::adapters::claude_sessions::self_session();
        let membership = me
            .as_ref()
            .and_then(|s| _registry_member_for_session(&s.session_id));
        let membership = match membership {
            Some(m) => m,
            None => fail("the flow mailbox is a team-internal address; only members deliver to it"),
        };
        let loaded = ok_or_fail(_load_team(&membership.0, ""));
        return (membership.0, loaded);
    }
    if !team.is_empty() {
        let t = ok_or_fail(_load_team(team, ""));
        if _existing_team_agent(&t, to_agent).is_none() {
            fail(&format!("agent '{to_agent}' not found in team '{team}'"));
        }
        let name = t.name.clone();
        return (name, t);
    }
    let candidates: Vec<PaneInfo> = tmux::list_panes_all()
        .into_iter()
        .filter(|p| p.agent == to_agent && !p.team.is_empty())
        .collect();
    let registry_teams: HashSet<String> = crate::registry::list_entries()
        .into_iter()
        .filter(|e| !truthy(e.get("corrupt")))
        .filter(|e| {
            e.get("members")
                .and_then(Value::as_array)
                .map(|members| {
                    members.iter().any(|m| {
                        m.as_object()
                            .map(|m| map_str(m, "name") == to_agent)
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        })
        .map(|e| map_str(&e, "team"))
        .collect();
    let mut teams: Vec<String> = candidates
        .iter()
        .map(|p| p.team.clone())
        .chain(registry_teams.into_iter())
        .collect::<HashSet<String>>()
        .into_iter()
        .collect();
    teams.sort();
    if teams.is_empty() {
        fail(&format!(
            "agent '{to_agent}' not found in any team (see `hive ls`)"
        ));
    }
    if teams.len() > 1 {
        let addresses = teams
            .iter()
            .map(|name| format!("{name}.{to_agent}"))
            .collect::<Vec<_>>()
            .join(", ");
        fail(&format!(
            "agent '{to_agent}' exists in {} teams; address one of: {addresses}",
            teams.len()
        ));
    }
    let team_name = teams.remove(0);
    let loaded = ok_or_fail(_load_team(&team_name, ""));
    (team_name, loaded)
}

pub(crate) fn _existing_team_agent(t: &Team, agent_name: &str) -> Option<Agent> {
    t.get(agent_name).ok()
}

/// pid -> (team, agent) for every live claude team-member engine.
pub(crate) fn _live_member_pids() -> HashMap<i32, (String, String)> {
    let mut out = HashMap::new();
    for p in tmux::list_panes_all() {
        if !p.team.is_empty() && !p.agent.is_empty() {
            let engine = crate::adapters::claude_bg::job_id_for_pane(&p.pane_id)
                .and_then(|job_id| crate::adapters::claude_bg::engine_session_for_job(&job_id));
            if let Some(engine) = engine {
                out.insert(engine.pid, (p.team.clone(), p.agent.clone()));
            }
        }
    }
    out
}

pub(crate) fn _sorted_member_rows(rows: Vec<Map<String, Value>>) -> Vec<Map<String, Value>> {
    let mut rows = rows;
    rows.sort_by_key(|m| {
        let name = map_str(m, "name");
        (name != LEAD_AGENT_NAME, name)
    });
    rows
}

// ---------------------------------------------------------------------------
// Command tree
// ---------------------------------------------------------------------------

fn passthrough_command(
    name: &'static str,
    about: &'static str,
    long_about: &'static str,
) -> Command {
    Command::new(name)
        .about(about)
        .long_about(long_about)
        .disable_help_flag(true)
        .arg(
            Arg::new("args")
                .num_args(0..)
                .allow_hyphen_values(true)
                .trailing_var_arg(true),
        )
}

fn json_default_options(cmd: Command) -> Command {
    cmd.arg(
        Arg::new("plain")
            .long("plain")
            .action(ArgAction::SetTrue)
            .help("Human-readable output instead of the default JSON"),
    )
    .arg(
        Arg::new("legacy_json")
            .long("json")
            .action(ArgAction::SetTrue)
            .hide(true)
            .help("Deprecated no-op (JSON is the default output)"),
    )
}

pub(crate) fn build_cli() -> Command {
    Command::new("hive")
        .about("Hive - tmux-first multi-agent collaboration runtime.")
        .version(_hive_version())
        .disable_version_flag(true)
        .disable_help_subcommand(true)
        .subcommand(
            Command::new("fork")
                .about("Fork the current agent session into a new split pane.")
                .long_about(
                    "Fork the current agent session into a new split pane.\n\n\
                     Humans typically bind this to a keyboard shortcut (terminal + tmux).\n\
                     Agents also invoke it to create a clone that can pick up work without\n\
                     interrupting the current turn.\n\n\
                     Pass `--join-as <name>` to register the new pane as a team member;\n\
                     `--prompt` then sends an initial message after the fork is ready.\n\n\
                     On a pane not bound to any Hive team, fork still works: it produces a bare,\n\
                     independent clone (no team registration, no `@hive-*` tags) and returns\n\
                     `registered: null`, `team: null`. `--join-as` requires a team-bound pane.\n\n\
                     Examples:\n  \
                     hive fork                                  # auto-detect split direction\n  \
                     hive fork --split h                        # force horizontal split\n  \
                     hive fork --join-as dodo-c1 --prompt \"continue the thread\"",
                )
                .arg(
                    Arg::new("pane_id")
                        .long("pane")
                        .default_value("")
                        .help("Source pane ID (default: auto-detect)"),
                )
                .arg(
                    Arg::new("split")
                        .long("split")
                        .short('s')
                        .value_parser(["auto", "h", "v"])
                        .default_value("auto")
                        .help("Split direction (default: auto-detect from pane dimensions)"),
                )
                .arg(
                    Arg::new("join_as")
                        .long("join-as")
                        .default_value("")
                        .help("Register the forked pane into the current team as this agent name"),
                )
                .arg(
                    Arg::new("prompt")
                        .long("prompt")
                        .default_value("")
                        .help("Prompt to send to the forked agent after it is ready"),
                ),
        )
        .subcommand(
            Command::new("join")
                .about("Join a team.")
                .long_about(
                    "Join a team.\n\n\
                     Outside tmux: the current Claude session enters TEAM's roster as a\n\
                     full member. Inside tmux: the current pane (or --pane) registers into\n\
                     the window's team.",
                )
                .arg(Arg::new("team_arg").default_value(""))
                .arg(
                    Arg::new("name_override")
                        .long("as")
                        .default_value("")
                        .help("Name for the new member (default: auto-derived)"),
                )
                .arg(
                    Arg::new("pane_override")
                        .long("pane")
                        .default_value("")
                        .help("Register another pane instead of the current one (tmux only)"),
                )
                .arg(
                    Arg::new("notify")
                        .long("notify")
                        .action(ArgAction::SetTrue)
                        .overrides_with("no_notify")
                        .help("Deliver the join message over the native transport (doubles as a reachability check; --no-notify registers without proving the pane deliverable)"),
                )
                .arg(
                    Arg::new("no_notify")
                        .long("no-notify")
                        .action(ArgAction::SetTrue)
                        .overrides_with("notify"),
                )
                .arg(
                    Arg::new("group_name")
                        .long("group")
                        .default_value("")
                        .help("Cross-team group tag for display and namespace reservation (optional; qualified-name routing works without it)."),
                ),
        )
        .subcommand(
            Command::new("create")
                .about("Create a team.")
                .long_about(
                    "Create a team.\n\n\
                     NAME is optional everywhere (pool-picked by default). Outside tmux:\n\
                     a headless team — `hive attach` renders it. Inside tmux on an agent\n\
                     pane: that pane becomes the orch. Inside tmux on a shell pane: the\n\
                     window binds the team without an orch.",
                )
                .arg(Arg::new("name").default_value(""))
                .arg(
                    Arg::new("desc")
                        .long("desc")
                        .short('d')
                        .default_value("")
                        .help("Team description"),
                )
                .arg(
                    Arg::new("workspace")
                        .long("workspace")
                        .short('w')
                        .default_value("")
                        .help("Workspace path to initialize"),
                )
                .arg(
                    Arg::new("reset_workspace")
                        .long("reset-workspace")
                        .action(ArgAction::SetTrue)
                        .help("Remove existing workspace before initialization"),
                )
                .arg(
                    Arg::new("state_entries")
                        .long("state")
                        .action(ArgAction::Append)
                        .help("Initial state KEY=VALUE (repeatable)"),
                ),
        )
        .subcommand(
            Command::new("delete")
                .about("Delete a team and clean up.")
                .arg(Arg::new("name").required(true))
                .arg(
                    Arg::new("workspace")
                        .long("workspace")
                        .short('w')
                        .default_value("")
                        .help("Workspace path to remove"),
                )
                .arg(
                    Arg::new("keep_workspace")
                        .long("keep-workspace")
                        .action(ArgAction::SetTrue)
                        .hide(true)
                        .help("Deprecated no-op (workspace is now kept by default)"),
                )
                .arg(
                    Arg::new("delete_workspace")
                        .long("delete-workspace")
                        .action(ArgAction::SetTrue)
                        .help("Also delete the workspace directory"),
                ),
        )
        .subcommand(
            Command::new("spawn")
                .about("Spawn an agent pane, optionally dispatching a task atomically.")
                .long_about(
                    "Spawn an agent pane, optionally dispatching a task atomically.\n\n\
                     Creates a new tmux pane in the current window and starts the chosen\n\
                     agent CLI. By default spawns the same CLI as the current pane; use\n\
                     `--cli claude|codex|grok` to pick a specific one.\n\n\
                     With `--task <artifact>`, the member boots straight into the member\n\
                     contract (`/hive:hive`) and the task artifact arrives as its first\n\
                     `<HIVE>` message — spawn and dispatch are one atomic step, so the\n\
                     member never wanders off exploring while waiting for work.\n\n\
                     Examples:\n  \
                     hive spawn explore --task /tmp/tasks/explore.md\n  \
                     hive spawn review --cli codex --task /tmp/tasks/review.md\n  \
                     hive spawn dodo --cli codex\n  \
                     hive spawn claude -m claude-opus-5 --skill none",
                )
                .arg(Arg::new("agent_name").required(true))
                .arg(
                    Arg::new("model")
                        .long("model")
                        .short('m')
                        .default_value("")
                        .help("Model ID. claude: prefer aliases (fable/opus/sonnet) — they always track the latest; codex/grok: checked against the CLI's own catalog"),
                )
                .arg(
                    Arg::new("prompt")
                        .long("prompt")
                        .short('p')
                        .default_value("")
                        .help("Initial prompt (typed into TUI after startup)"),
                )
                .arg(
                    Arg::new("cwd")
                        .long("cwd")
                        .default_value("")
                        .help("Working directory"),
                )
                .arg(
                    Arg::new("skill")
                        .long("skill")
                        .default_value("hive:hive")
                        .help("Base skill to load after startup ('none' to skip)"),
                )
                .arg(
                    Arg::new("env")
                        .long("env")
                        .short('e')
                        .action(ArgAction::Append)
                        .help("Extra env vars (KEY=VALUE, repeatable)"),
                )
                .arg(
                    Arg::new("cli_name")
                        .long("cli")
                        .value_parser(["claude", "codex", "grok"])
                        .help("Agent CLI to spawn (default: same as current pane)"),
                )
                .arg(
                    Arg::new("task_artifact")
                        .long("task")
                        .help("Task artifact to dispatch atomically once the member is ready (member never boots into an empty inbox)"),
                )
                .arg(
                    Arg::new("team_arg")
                        .long("team")
                        .short('t')
                        .default_value("")
                        .help("Explicit team (default: the pane's binding)"),
                ),
        )
        .subcommand(
            Command::new("config")
                .about("Read / write user-level settings (~/.hive/settings.json).")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(
                    Command::new("get")
                        .about("Print the value at KEY (dot-path). Exit 1 when unset.")
                        .arg(Arg::new("key").required(true)),
                )
                .subcommand(
                    Command::new("set")
                        .about("Set KEY to VALUE (true/false/int/float/string).")
                        .arg(Arg::new("key").required(true))
                        .arg(Arg::new("value").required(true)),
                )
                .subcommand(
                    Command::new("unset")
                        .about("Remove KEY. Exit 1 when KEY was not set.")
                        .arg(Arg::new("key").required(true)),
                ),
        )
        .subcommand(
            Command::new("inject")
                .about("Debug: inject raw input into an agent pane.")
                .long_about(
                    "Debug: inject raw input into an agent pane.\n\n\
                     Writes text directly into the target pane without the `<HIVE>`\n\
                     envelope or delivery tracking. Use only when bypassing the message\n\
                     protocol for low-level debugging.\n\n\
                     Example:\n  hive inject dodo \"plain ping\"",
                )
                .arg(Arg::new("agent_name").required(true))
                .arg(Arg::new("text").required(true)),
        )
        .subcommand(
            Command::new("compact")
                .about("Trigger /compact on your own pane.")
                .long_about(
                    "Trigger /compact on your own pane.\n\n\
                     Works on any agent pane, team-bound or not: a pane with no Hive team is\n\
                     compacted by its literal pane facts, and the response carries `member` =\n\
                     the pane id with `team: null`.\n\n\
                     When wired into a tmux key binding, pass `--pane \"#{pane_id}\"` so the\n\
                     triggering pane is captured by tmux at keypress time rather than read\n\
                     from the (potentially stale) TMUX_PANE env in a detached subprocess.\n\n\
                     Examples:\n  hive compact\n  hive compact --pane %21",
                )
                .arg(
                    Arg::new("pane_id")
                        .long("pane")
                        .default_value("")
                        .help("Target pane ID (default: current pane via TMUX_PANE)"),
                ),
        )
        .subcommand(
            Command::new("team")
                .about("Show team overview.")
                .long_about(
                    "Show team overview.\n\n\
                     Returns a JSON payload with `members[]`, `self` (your own name), the\n\
                     bound `tmuxSession` / `tmuxWindow`, `runtimeWorkspace`, and `cwd`.\n\n\
                     Each member row carries the runtime fields `busy`, `inputState`, and\n\
                     `turnPhase` — see docs/runtime-model.md for semantics. `self` is a\n\
                     string pointer: look yourself up in `members[]` for your own state.\n\n\
                     If the current tmux window has no team bound, returns a bootstrap\n\
                     payload instead: `team=null`, a pane list, and a `hint` telling you\n\
                     to run `hive create`.\n\n\
                     Examples:\n  \
                     hive team                                # full payload when a team is bound\n  \
                     hive team | jq '.members[] | select(.name==\"dodo\")'",
                )
                .arg(
                    Arg::new("team_arg")
                        .long("team")
                        .short('t')
                        .default_value("")
                        .help("Explicit team (default: the pane's binding)"),
                ),
        )
        .subcommand(
            Command::new("layout")
                .about("Apply a tmux layout preset to the current team window.")
                .long_about(
                    "Apply a tmux layout preset to the current team window.\n\n\
                     Use ``auto`` to pick a preset adaptively from the window's aspect ratio.",
                )
                .arg(
                    Arg::new("preset")
                        .required(true)
                        .ignore_case(true)
                        .value_parser([
                            "auto",
                            "main-vertical",
                            "main-horizontal",
                            "tiled",
                            "even-horizontal",
                            "even-vertical",
                        ]),
                ),
        )
        .subcommand(
            Command::new("flow")
                .about("Deterministic member orchestration from a Python script.")
                .long_about(
                    "Deterministic member orchestration from a Python script.\n\n\
                     A flow script uses the `hive.flow` library: `agent()` spawns a live\n\
                     member pane, dispatches a task atomically, and blocks for the reply;\n\
                     `parallel()` fans out. Every node is a visible pane — watch, type\n\
                     into, or interrupt any of them while the flow runs.",
                )
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(
                    Command::new("run")
                        .about("Run SCRIPT against the current team.")
                        .long_about(
                            "Run SCRIPT against the current team.\n\n\
                             The script is trusted Python (you or your orch wrote it). Members it\n\
                             spawns reply to the reserved `flow` mailbox; the runner blocks until\n\
                             the script finishes. Typical use from an orch: run it in a background\n\
                             shell and read the output when it completes.",
                        )
                        .arg(Arg::new("script").required(true)),
                ),
        )
        .subcommand(
            Command::new("pr")
                .about("Pin a PR number on the team window's status bar.")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(json_default_options(
                    Command::new("set")
                        .about("Label the current team window with its PR number.")
                        .long_about(
                            "Label the current team window with its PR number.\n\n\
                             Run right after ``gh pr create --draft`` — writes ``@hive-pr`` on the\n\
                             current tmux window and installs a per-window status-bar display derived\n\
                             from the global ``window-status-format`` / ``window-status-current-format``\n\
                             (the index position renders ``PR<n>``; user styling and padding are\n\
                             preserved). Idempotent — re-running replaces the stamp and re-derives\n\
                             the display.",
                        )
                        .arg(Arg::new("number").required(true).value_parser(clap::value_parser!(i64))),
                ))
                .subcommand(json_default_options(
                    Command::new("clear")
                        .about("Clear the current team window's PR number stamp."),
                )),
        )
        .subcommand(
            Command::new("view")
                .about("Read-only viewer for a Claude session transcript (follows live).")
                .arg(Arg::new("session_id").required(true)),
        )
        .subcommand(
            Command::new("attach")
                .about("Render a team's display: jump to its window, or build one.")
                .long_about(
                    "Render a team's display: jump to its window, or build one.\n\n\
                     The registry is the team's existence; this materializes (or finds) its\n\
                     tmux window — one attach pane per member, each riding its engine's own\n\
                     viewer (claude attach loop / codex thread resume / grok session resume).\n\
                     Run from outside tmux it finishes by exec'ing `tmux attach`.",
                )
                .arg(Arg::new("team_name").required(true)),
        )
        .subcommand(json_default_options(
            Command::new("ls")
                .about("List hive teams from the registry, with their display state.")
                .long_about(
                    "List hive teams from the registry, with their display state.\n\n\
                     Works outside tmux too — the registry is the truth layer; without a\n\
                     server every team simply shows as detached.",
                ),
        ))
        .subcommand(
            Command::new("send")
                .about("Send a message to another agent — the only message verb.")
                .long_about(
                    "Send a message to another agent — the only message verb.\n\n\
                     Threading is automatic: when the latest inbound message from the\n\
                     recipient is still unanswered, this send is recorded as its reply;\n\
                     otherwise it opens a new thread. Senders never handle msgIds.\n\n\
                     The recipient is an address, and every `from=` value on a received\n\
                     envelope is one — answer by copying it verbatim. A teammate is a bare\n\
                     name. A member of some team is `<team>.<member>` (how a Claude session\n\
                     outside tmux, e.g. the desktop app, reaches in; bare names work there\n\
                     too while unique across live teams — its message arrives as\n\
                     `from=ccd.<its name>`). A Claude session outside any team is\n\
                     `ccd.<name or title or pid>` (how a member reaches out). `flow.run`\n\
                     is the flow runner's mailbox — an address kind, not a member; sends\n\
                     to it confirm with one `delivered to flow mailbox` line and never\n\
                     get a HIVE ack back.\n\n\
                     New-thread sends must keep `body` to a short summary and put details\n\
                     in `--artifact`; the body is rejected if longer than 500 chars, has\n\
                     3+ lines, contains fenced code, or starts markdown heading/list\n\
                     lines. A send that continues a thread is exempt.\n\n\
                     Delivery is binary and fire-and-forget: the native transport (claude\n\
                     daemon / codex daemon) either accepted the message — its runtime owns\n\
                     it from there — or the command exits non-zero with the transport\n\
                     error. Success prints nothing; there is nothing to poll afterwards.\n\n\
                     Examples:\n  \
                     hive send dodo \"review this diff\" --artifact /tmp/diff.md\n  \
                     hive send \"ccd.PR review\" \"build is green\"    # session by desktop title\n  \
                     hive send dodo \"see report\" --artifact - <<'EOF'\n  \
                     # Findings\n  - item\n  EOF",
                )
                .arg(Arg::new("to_agent").required(true))
                .arg(Arg::new("body").default_value(""))
                .arg(
                    Arg::new("artifact")
                        .long("artifact")
                        .default_value("")
                        .help("Artifact path for large payloads"),
                ),
        )
        .subcommand(
            Command::new("thread")
                .about("Show a reply thread rooted at a msgId.")
                .long_about(
                    "Show a reply thread rooted at a msgId.\n\n\
                     Returns the chain of send/reply events linked to this msgId. Useful\n\
                     to audit conversation flow or resolve \"who replied to what\".\n\n\
                     Example:\n  hive thread aBc1",
                )
                .arg(Arg::new("message_id").required(true)),
        )
        .subcommand(
            Command::new("doctor")
                .about("Diagnose agent connectivity and session state.")
                .long_about(
                    "Diagnose agent connectivity and session state.\n\n\
                     With no argument, probes yourself. With an agent name, probes that\n\
                     peer — pane liveness, transcript readability, hived heartbeat,\n\
                     runtime input state.\n\n\
                     Examples:\n  \
                     hive doctor                  # probe self\n  \
                     hive doctor dodo             # probe a peer",
                )
                .arg(Arg::new("agent_name").default_value("")),
        )
        .subcommand(
            Command::new("capture")
                .about("Debug: capture raw pane output from a team member's pane.")
                .long_about(
                    "Debug: capture raw pane output from a team member's pane.\n\n\
                     Prints the last N lines (default 30) of the member's tmux pane.\n\
                     Use to inspect what the agent actually sees when transcript parsing\n\
                     gives unexpected results.\n\n\
                     Example:\n  hive capture dodo -n 80",
                )
                .arg(Arg::new("member_name").required(true))
                .arg(
                    Arg::new("lines")
                        .long("lines")
                        .short('n')
                        .default_value("30")
                        .value_parser(clap::value_parser!(i64)),
                ),
        )
        .subcommand(
            Command::new("interrupt")
                .about("Interrupt an agent's running turn.")
                .long_about(
                    "Interrupt an agent's running turn.\n\n\
                     Aborts the turn over the member's own transport — addressed to its\n\
                     engine, not typed at its pane. Use when a peer is stuck in a tool\n\
                     loop or you need to abort a runaway action.\n\n\
                     Example:\n  hive interrupt dodo",
                )
                .arg(Arg::new("agent_name").required(true)),
        )
        .subcommand(
            Command::new("kill")
                .about("Kill an agent pane and remove it from the team.")
                .long_about(
                    "Kill an agent pane and remove it from the team.\n\n\
                     Qualified names (`<group>.<name>`) resolve across teams so you can\n\
                     kill a peer-team agent from the main group pane. Bare names resolve\n\
                     against the caller's scoped team.\n\n\
                     Example:\n  hive kill worker1",
                )
                .arg(Arg::new("agent_name").required(true)),
        )
        .subcommand(passthrough_command(
            "cvim",
            "Human-only: edit the last assistant message in vim, send it back.",
            "Human-only: edit the last assistant message in vim, send it back.\n\n\
             Opens a popup vim seeded with the previous assistant message and sends the\n\
             edited result back to the agent pane. Intended to be typed by the human via\n\
             the agent's shell escape (e.g. `!hive cvim`) in Claude Code or Codex. Not\n\
             meant for the model to invoke on its own.",
        ))
        .subcommand(passthrough_command(
            "vim",
            "Human-only: compose in a blank vim buffer, send it to the agent pane.",
            "Human-only: compose in a blank vim buffer, send it to the agent pane.\n\n\
             Intended to be typed by the human via the agent's shell escape (e.g. `!hive vim`)\n\
             in Claude Code or Codex. Not meant for the model to invoke on its own.",
        ))
        .subcommand(passthrough_command(
            "vfork",
            "Human-only: fork the current Hive session into a vertical split.",
            "Human-only: fork the current Hive session into a vertical split.\n\n\
             Intended to be typed by the human via the agent's shell escape (e.g. `!hive vfork`)\n\
             in Claude Code or Codex. Not meant for the model to invoke on its own.",
        ))
        .subcommand(passthrough_command(
            "hfork",
            "Human-only: fork the current Hive session into a horizontal split.",
            "Human-only: fork the current Hive session into a horizontal split.\n\n\
             Intended to be typed by the human via the agent's shell escape (e.g. `!hive hfork`)\n\
             in Claude Code or Codex. Not meant for the model to invoke on its own.",
        ))
        .subcommand(
            Command::new("notify")
                .about("Notify the user for the current pane.")
                .long_about(
                    "Notify the user for the current pane.\n\n\
                     Flashes the tmux window status line, renames the tab, and rings the\n\
                     terminal bell so the user can spot the pending pane at a glance. The\n\
                     flash persists until the user focuses the target window (no\n\
                     timeout). Use this only when you are blocked and need the human\n\
                     back — not for progress updates. Message structure should cover:\n\
                     what happened, why you need them now, what to do on return.\n\n\
                     Examples:\n  hive notify \"press Space to come back and confirm migration\"",
                )
                .arg(Arg::new("message").required(true)),
        )
        .subcommand(
            Command::new("plugin")
                .about("Manage first-party Hive plugins.")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(json_default_options(
                    Command::new("list").about("List available plugins and whether they are enabled."),
                ))
                .subcommand(json_default_options(
                    Command::new("ls")
                        .about("Hidden alias of `hive plugin list`.")
                        .hide(true),
                ))
                .subcommand(json_default_options(
                    Command::new("enable")
                        .about("Enable a plugin and materialize its commands.")
                        .arg(Arg::new("name").required(true)),
                ))
                .subcommand(json_default_options(
                    Command::new("disable")
                        .about("Disable a plugin and remove its commands.")
                        .arg(Arg::new("name").required(true)),
                )),
        )
        .subcommand(passthrough_command(
            "codex",
            "Launch codex on the shared app-server daemon (hive-managed).",
            "Launch codex on the shared app-server daemon (hive-managed).\n\n\
             Usually invoked through the `hcodex` launcher from `hive shell-init` rather\n\
             than by hand; all arguments are forwarded to codex. Replaces the current process\n\
             with codex and never returns on success.",
        ))
        .subcommand(passthrough_command(
            "claude",
            "Launch claude as a hive-managed background job (hclaude launcher).",
            "Launch claude as a hive-managed background job (hclaude launcher).\n\n\
             Interactive launches run as `claude --bg` jobs with the pane attached as\n\
             a viewer; management subcommands and non-interactive shapes pass through\n\
             to plain claude. Does not return on the raw path; on the managed path it\n\
             exits with the viewer loop's status.",
        ))
        .subcommand(passthrough_command(
            "grok",
            "Launch grok attached to a per-pane leader daemon (hive-managed).",
            "Launch grok attached to a per-pane leader daemon (hive-managed).\n\n\
             Usually invoked through the `hgrok` launcher from `hive shell-init` rather\n\
             than by hand; all arguments are forwarded to grok. Replaces the current\n\
             process with grok and never returns on success.",
        ))
        .subcommand(
            Command::new("ccd")
                .about("Discover Claude Code sessions outside the team — the desktop app, another terminal — by their cross-session inbox registry.")
                .long_about(
                    "Discover Claude Code sessions outside the team — the desktop app,\n\
                     another terminal — by their cross-session inbox registry.\n\n\
                     `hive ccd ls` lists the reachable sessions; messaging one is plain\n\
                     `hive send ccd.<name>` (name, desktop title, or pid).",
                )
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(
                    Command::new("ls")
                        .about("List the Claude Code sessions `hive send ccd.<name>` can reach.")
                        .long_about(
                            "List the Claude Code sessions `hive send ccd.<name>` can reach.\n\n\
                             The same registry `/list-agents` reads: every live session that binds a\n\
                             cross-session inbox (Claude Code 2.1.224+). A session on an older CLI, or\n\
                             started in bare mode, has no inbox and is not listed. `title` is the\n\
                             desktop app's session title when one is set. A session that is really a\n\
                             live team member carries a `member` field with its `<team>.<agent>`\n\
                             address: message it over the bus, not here.",
                        ),
                ),
        )
        .subcommand(
            Command::new("resume-hint")
                .about("Print a cd-ready resume command for the session this pane just ran.")
                .hide(true)
                .arg(
                    Arg::new("cli_name")
                        .required(true)
                        .value_parser(["claude", "codex", "grok"]),
                ),
        )
        .subcommand(
            Command::new("shell-init")
                .about("Print the `hcodex` / `hclaude` / `hgrok` launchers for your shell.")
                .long_about(
                    "Print the `hcodex` / `hclaude` / `hgrok` launchers for your shell.\n\n\
                     Add to your shell rc; then `hcodex` / `hclaude` / `hgrok` start a\n\
                     hive-connected codex / claude / grok in the current tmux pane, while the\n\
                     plain `codex` / `claude` / `grok` stay untouched:\n\n  \
                     # ~/.zshrc or ~/.bashrc\n  \
                     eval \"$(hive shell-init zsh)\"\n  \
                     # ~/.config/fish/config.fish\n  \
                     hive shell-init fish | source\n\n\
                     Outside tmux, and for management subcommands and non-interactive flags,\n\
                     the launchers run the plain binary.",
                )
                .arg(Arg::new("shell").default_value("")),
        )
        .subcommand(
            Command::new("worktree")
                .about("Per-feature worktree pool: start a feature, finish it, inspect state.")
                .long_about(
                    "Per-feature worktree pool: start a feature, finish it, inspect state.\n\n\
                     Pool layout: <main checkout>/.claude/worktrees/<feature>, branch == feature.\n\
                     Hive creates/removes worktrees and records ownership in git config;\n\
                     entering/leaving the directory is the agent's own move (Claude:\n\
                     EnterWorktree path=<path> / ExitWorktree action=keep; Codex: cd).\n\n\
                     Examples:\n  \
                     hive worktree start login-flow         # create worktree + branch, print JSON with path\n  \
                     hive worktree status                   # pool state for this repo\n  \
                     hive worktree done login-flow          # remove the worktree, keep the branch",
                )
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(json_default_options(
                    Command::new("set-base")
                        .about("Declare the team's integration branch (the base of every sub-PR).")
                        .long_about(
                            "Declare the team's integration branch (the base of every sub-PR).\n\n\
                             Run from the team window after creating and pushing the branch; every\n\
                             `hive worktree start` in this window afterwards resolves its base from\n\
                             it. REF must already resolve to a commit.",
                        )
                        .arg(Arg::new("ref").required(true)),
                ))
                .subcommand(json_default_options(
                    Command::new("start")
                        .about("Create (or re-attach) the worktree for FEATURE and print its path as JSON.")
                        .long_about(
                            "Create (or re-attach) the worktree for FEATURE and print its path as JSON.\n\n\
                             Exit 0 = ready (mode created/existing/attached/adopted-existing-branch).\n\
                             Exit 1 with mode=needs-rebase = branch exists but does not contain the\n\
                             resolved base: rebase inside the worktree, then rerun start.",
                        )
                        .arg(Arg::new("feature").required(true))
                        .arg(
                            Arg::new("base_ref")
                                .long("base")
                                .help("Base ref override (default: the window's integration branch from `hive worktree set-base`, else detected default branch)"),
                        ),
                ))
                .subcommand(json_default_options(
                    Command::new("done")
                        .about("Remove FEATURE's worktree. The branch is always kept (PRs live on it).")
                        .long_about(
                            "Remove FEATURE's worktree. The branch is always kept (PRs live on it).\n\n\
                             Refuses while you are inside the worktree, while a git operation is in\n\
                             progress, or while there are uncommitted changes (unless --force).",
                        )
                        .arg(Arg::new("feature").required(true))
                        .arg(
                            Arg::new("force")
                                .long("force")
                                .action(ArgAction::SetTrue)
                                .help("Discard uncommitted work (destructive; prints a status summary first)"),
                        ),
                ))
                .subcommand(json_default_options(
                    Command::new("status")
                        .about("Read-only lifecycle view of FEATURE (or every hive-labeled worktree).")
                        .arg(Arg::new("feature")),
                )),
        )
}

// ---------------------------------------------------------------------------
// Root help rendering (SectionedHelpGroup equivalent)
// ---------------------------------------------------------------------------

fn section_for(name: &str) -> &'static str {
    _COMMAND_HELP_SECTIONS
        .iter()
        .find(|(cmd, _)| *cmd == name)
        .map(|(_, section)| *section)
        .unwrap_or("Other Commands")
}

pub(crate) fn render_root_help() -> String {
    let cli = build_cli();
    let mut rows: Vec<(String, String)> = Vec::new();
    for sub in cli.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        rows.push((
            sub.get_name().to_string(),
            sub.get_about().map(|s| s.to_string()).unwrap_or_default(),
        ));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let mut sections: HashMap<&'static str, Vec<(String, String)>> = HashMap::new();
    for (name, help) in rows {
        sections
            .entry(section_for(&name))
            .or_default()
            .push((name, help));
    }

    let mut out = String::new();
    out.push_str("Usage: hive [OPTIONS] COMMAND [ARGS]...\n\n");
    out.push_str("  Hive - tmux-first multi-agent collaboration runtime.\n\n");
    out.push_str("Options:\n");
    out.push_str("  --version   Show the version and exit.\n");
    out.push_str("  -h, --help  Show this message and exit.\n");
    for section in _COMMAND_HELP_SECTION_ORDER {
        let rows = match sections.get(section) {
            Some(rows) if !rows.is_empty() => rows,
            _ => continue,
        };
        out.push('\n');
        out.push_str(section);
        out.push_str(":\n");
        if let Some((_, description)) = _COMMAND_HELP_SECTION_DESCRIPTIONS
            .iter()
            .find(|(name, _)| name == section)
        {
            out.push_str(&format!("  {description}\n\n"));
        }
        let width = rows.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
        for (name, help) in rows {
            out.push_str(&format!("  {name:<width$}  {help}\n"));
        }
    }
    out.push_str("\nExamples:\n");
    for block in _ROOT_HELP_EXAMPLES.split("\n\n") {
        out.push_str(&format!("  {}\n\n", block.replace('\n', "\n  ")));
    }
    out
}

// ---------------------------------------------------------------------------
// main + dispatch
// ---------------------------------------------------------------------------

const _KNOWN_COMMANDS: &[&str] = &[
    "fork",
    "join",
    "create",
    "delete",
    "spawn",
    "config",
    "inject",
    "compact",
    "team",
    "layout",
    "flow",
    "pr",
    "view",
    "attach",
    "ls",
    "send",
    "thread",
    "doctor",
    "capture",
    "interrupt",
    "kill",
    "cvim",
    "vim",
    "vfork",
    "hfork",
    "notify",
    "plugin",
    "codex",
    "claude",
    "grok",
    "ccd",
    "resume-hint",
    "shell-init",
    "worktree",
];

fn arg_str<'a>(m: &'a ArgMatches, key: &str) -> &'a str {
    m.get_one::<String>(key).map(String::as_str).unwrap_or("")
}

fn arg_vec(m: &ArgMatches, key: &str) -> Vec<String> {
    m.get_many::<String>(key)
        .map(|values| values.cloned().collect())
        .unwrap_or_default()
}

/// Root-group gates from the Python `cli()` callback.
fn run_root_gates(invoked: &str) {
    _require_codex_native(Some(invoked));
    if !_TMUX_OPTIONAL_ROOT_COMMANDS.contains(&invoked) && !tmux::is_inside_tmux() {
        if invoked == "send" && crate::adapters::claude_sessions::self_session().is_some() {
            return; // a Claude session sending into hive as a guest
        }
        fail(_TMUX_REQUIRED_MESSAGE);
    }
}

pub fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let args: Vec<String> = argv.iter().skip(1).cloned().collect();

    if args.is_empty() {
        print!("{}", render_root_help());
        std::process::exit(0);
    }
    match args[0].as_str() {
        "-h" | "--help" => {
            print!("{}", render_root_help());
            std::process::exit(0);
        }
        "--version" => {
            println!("hive, version {}", _hive_version());
            std::process::exit(0);
        }
        _ => {}
    }

    let invoked = args[0].clone();
    let help_requested = args.iter().any(|a| a == "-h" || a == "--help");
    if _KNOWN_COMMANDS.contains(&invoked.as_str()) && !help_requested {
        run_root_gates(&invoked);
    }

    // Launcher / human-helper passthrough: everything after the subcommand is
    // forwarded verbatim (Click's ignore_unknown_options + UNPROCESSED args).
    let tail: Vec<String> = args.iter().skip(1).cloned().collect();
    match invoked.as_str() {
        "codex" => {
            rest::codex_cmd(&tail);
            return;
        }
        "claude" => {
            rest::claude_cmd(&tail);
            return;
        }
        "grok" => {
            rest::grok_cmd(&tail);
            return;
        }
        "cvim" | "vim" | "vfork" | "hfork" => {
            if tail.first().map(String::as_str) == Some("--help") {
                let mut cli = build_cli();
                if let Some(sub) = cli.find_subcommand_mut(invoked.as_str()) {
                    let _ = sub.print_long_help();
                }
                std::process::exit(0);
            }
            match invoked.as_str() {
                "cvim" => rest::cvim_cmd(&tail),
                "vim" => rest::vim_cmd(&tail),
                "vfork" => rest::vfork_cmd(&tail),
                _ => rest::hfork_cmd(&tail),
            }
            return;
        }
        _ => {}
    }

    let matches = match build_cli().try_get_matches_from(&argv) {
        Ok(matches) => matches,
        Err(err) => err.exit(),
    };
    dispatch(&matches);
}

fn dispatch(matches: &ArgMatches) {
    match matches.subcommand() {
        Some(("fork", m)) => rest::fork_cmd(
            arg_str(m, "pane_id"),
            arg_str(m, "split"),
            arg_str(m, "join_as"),
            arg_str(m, "prompt"),
        ),
        Some(("join", m)) => core_cmds::join_cmd(
            arg_str(m, "team_arg"),
            arg_str(m, "name_override"),
            arg_str(m, "pane_override"),
            !m.get_flag("no_notify"),
            arg_str(m, "group_name"),
        ),
        Some(("create", m)) => core_cmds::create(
            arg_str(m, "name"),
            arg_str(m, "desc"),
            arg_str(m, "workspace"),
            m.get_flag("reset_workspace"),
            &arg_vec(m, "state_entries"),
        ),
        Some(("delete", m)) => core_cmds::delete(
            arg_str(m, "name"),
            arg_str(m, "workspace"),
            m.get_flag("keep_workspace"),
            m.get_flag("delete_workspace"),
        ),
        Some(("spawn", m)) => {
            // Click declares --task as `type=click.Path(exists=True,
            // dir_okay=False)` — validated at parse time, before the handler.
            let task = m.get_one::<String>("task_artifact").cloned();
            if let Some(task) = &task {
                let p = Path::new(task);
                if !p.exists() {
                    eprintln!("Error: Invalid value for '--task': Path '{task}' does not exist.");
                    std::process::exit(2);
                }
                if p.is_dir() {
                    eprintln!("Error: Invalid value for '--task': Path '{task}' is a directory.");
                    std::process::exit(2);
                }
            }
            rest::spawn(
                arg_str(m, "agent_name"),
                arg_str(m, "model"),
                arg_str(m, "prompt"),
                arg_str(m, "cwd"),
                arg_str(m, "skill"),
                &arg_vec(m, "env"),
                m.get_one::<String>("cli_name").map(String::as_str),
                task.as_deref(),
                arg_str(m, "team_arg"),
            )
        }
        Some(("config", m)) => match m.subcommand() {
            Some(("get", m)) => rest::config_get(arg_str(m, "key")),
            Some(("set", m)) => rest::config_set(arg_str(m, "key"), arg_str(m, "value")),
            Some(("unset", m)) => rest::config_unset(arg_str(m, "key")),
            _ => unreachable!("subcommand required"),
        },
        Some(("inject", m)) => rest::inject_cmd(arg_str(m, "agent_name"), arg_str(m, "text")),
        Some(("compact", m)) => rest::compact_cmd(arg_str(m, "pane_id")),
        Some(("team", m)) => core_cmds::team_cmd(arg_str(m, "team_arg")),
        Some(("layout", m)) => rest::layout_cmd(&arg_str(m, "preset").to_lowercase()),
        Some(("flow", m)) => match m.subcommand() {
            Some(("run", m)) => {
                let script = arg_str(m, "script");
                if !Path::new(script).exists() {
                    eprintln!("Error: Invalid value for 'SCRIPT': Path '{script}' does not exist.");
                    std::process::exit(2);
                }
                rest::flow_run_cmd(script)
            }
            _ => unreachable!("subcommand required"),
        },
        Some(("pr", m)) => match m.subcommand() {
            Some(("set", m)) => rest::pr_set_cmd(
                *m.get_one::<i64>("number").expect("required"),
                m.get_flag("plain"),
            ),
            Some(("clear", m)) => rest::pr_clear_cmd(m.get_flag("plain")),
            _ => unreachable!("subcommand required"),
        },
        Some(("view", m)) => core_cmds::view_cmd(arg_str(m, "session_id")),
        Some(("attach", m)) => rest::attach_cmd(arg_str(m, "team_name")),
        Some(("ls", m)) => core_cmds::ls_cmd(m.get_flag("plain")),
        Some(("send", m)) => core_cmds::send(
            arg_str(m, "to_agent"),
            arg_str(m, "body"),
            arg_str(m, "artifact"),
        ),
        Some(("thread", m)) => rest::thread(arg_str(m, "message_id")),
        Some(("doctor", m)) => core_cmds::doctor(arg_str(m, "agent_name")),
        Some(("capture", m)) => rest::capture(
            arg_str(m, "member_name"),
            *m.get_one::<i64>("lines").unwrap_or(&30),
        ),
        Some(("interrupt", m)) => core_cmds::interrupt(arg_str(m, "agent_name")),
        Some(("kill", m)) => core_cmds::kill(arg_str(m, "agent_name")),
        Some(("notify", m)) => rest::notify_cmd(arg_str(m, "message")),
        Some(("plugin", m)) => match m.subcommand() {
            Some(("list", m)) => rest::plugin_list(m.get_flag("plain")),
            Some(("ls", m)) => rest::plugin_ls(m.get_flag("plain")),
            Some(("enable", m)) => rest::plugin_enable(arg_str(m, "name"), m.get_flag("plain")),
            Some(("disable", m)) => rest::plugin_disable(arg_str(m, "name"), m.get_flag("plain")),
            _ => unreachable!("subcommand required"),
        },
        Some(("ccd", m)) => match m.subcommand() {
            Some(("ls", _)) => rest::ccd_ls_cmd(),
            _ => unreachable!("subcommand required"),
        },
        Some(("resume-hint", m)) => rest::resume_hint_cmd(arg_str(m, "cli_name")),
        Some(("shell-init", m)) => rest::shell_init_cmd(arg_str(m, "shell")),
        Some(("worktree", m)) => match m.subcommand() {
            Some(("set-base", m)) => {
                rest::worktree_set_base_cmd(arg_str(m, "ref"), m.get_flag("plain"))
            }
            Some(("start", m)) => rest::worktree_start_cmd(
                arg_str(m, "feature"),
                m.get_one::<String>("base_ref").map(String::as_str),
                m.get_flag("plain"),
            ),
            Some(("done", m)) => rest::worktree_done_cmd(
                arg_str(m, "feature"),
                m.get_flag("force"),
                m.get_flag("plain"),
            ),
            Some(("status", m)) => rest::worktree_status_cmd(
                m.get_one::<String>("feature").map(String::as_str),
                m.get_flag("plain"),
            ),
            _ => unreachable!("subcommand required"),
        },
        _ => {
            print!("{}", render_root_help());
            std::process::exit(0);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (ported from tests/unit — logic-level only)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn _pane(agent: &str, team: &str, group: &str, pane_id: &str) -> PaneInfo {
        PaneInfo {
            pane_id: pane_id.to_string(),
            title: String::new(),
            command: String::new(),
            role: "agent".to_string(),
            agent: agent.to_string(),
            team: team.to_string(),
            cli: String::new(),
            group: group.to_string(),
        }
    }

    // --- tests/unit/test_group_routing.py ---

    #[test]
    fn test_find_qualified_returns_none_for_bare_name() {
        assert_eq!(_find_qualified_agent_target_in(&[], "orch"), Ok(None));
    }

    #[test]
    fn test_find_qualified_finds_unique_match() {
        let panes = vec![
            _pane("kraken.worker-1", "peer-1", "kraken", "%1"),
            _pane("kraken.judge-1", "peer-1", "kraken", "%2"),
            _pane("other", "peer-1", "", "%3"),
        ];
        assert_eq!(
            _find_qualified_agent_target_in(&panes, "kraken.worker-1"),
            Ok(Some(("peer-1".to_string(), "kraken.worker-1".to_string())))
        );
    }

    #[test]
    fn test_find_qualified_supports_public_squad_name_namespace() {
        let panes = vec![
            _pane("peaky.worker-1000", "dev-0-duo-1000", "peaky", "%1"),
            _pane("shelby.worker-1000", "dev-1-duo-1000", "shelby", "%2"),
            _pane("peaky.orch", "dev-0", "peaky", "%3"),
        ];
        assert_eq!(
            _find_qualified_agent_target_in(&panes, "peaky.worker-1000"),
            Ok(Some((
                "dev-0-duo-1000".to_string(),
                "peaky.worker-1000".to_string()
            )))
        );
    }

    #[test]
    fn test_find_qualified_returns_none_when_agent_missing() {
        let panes = vec![_pane("kraken.worker-1", "peer-1", "kraken", "%1")];
        assert_eq!(
            _find_qualified_agent_target_in(&panes, "kraken.worker-2"),
            Ok(None)
        );
    }

    #[test]
    fn test_find_qualified_raises_on_ambiguous() {
        let panes = vec![
            _pane("kraken.worker-1", "peer-1", "kraken", "%1"),
            _pane("kraken.worker-1", "peer-2", "kraken", "%5"),
        ];
        let err = _find_qualified_agent_target_in(&panes, "kraken.worker-1").unwrap_err();
        assert!(err.contains("unique"));
    }

    #[test]
    fn test_find_qualified_resolves_missing_group() {
        // A pane with matching @hive-agent but no @hive-group is still routable.
        let panes = vec![_pane("kraken.worker-1", "peer-1", "", "%1")];
        assert_eq!(
            _find_qualified_agent_target_in(&panes, "kraken.worker-1"),
            Ok(Some(("peer-1".to_string(), "kraken.worker-1".to_string())))
        );
    }

    #[test]
    fn test_find_qualified_rejects_conflicting_group() {
        // A pane with @hive-agent=kraken.worker-1 but @hive-group=mafia is a
        // tagging mistake — the resolver must error, not silently route.
        let panes = vec![_pane("kraken.worker-1", "peer-1", "mafia", "%1")];
        let err = _find_qualified_agent_target_in(&panes, "kraken.worker-1").unwrap_err();
        assert!(err.contains("conflicting"));
    }

    #[test]
    fn test_find_qualified_ignores_same_suffix_in_other_public_squad() {
        let panes = vec![_pane(
            "shelby.worker-1000",
            "dev-1-duo-1000",
            "shelby",
            "%2",
        )];
        assert_eq!(
            _find_qualified_agent_target_in(&panes, "peaky.worker-1000"),
            Ok(None)
        );
    }

    #[test]
    fn test_find_qualified_requires_non_empty_group_prefix() {
        assert_eq!(_find_qualified_agent_target_in(&[], ".worker-1"), Ok(None));
    }

    #[test]
    fn test_find_qualified_ambiguous_with_missing_groups() {
        // Duplicate @hive-agent across panes is ambiguous even when both lack group.
        let panes = vec![
            _pane("kraken.worker-1", "peer-1", "", "%1"),
            _pane("kraken.worker-1", "peer-2", "", "%5"),
        ];
        let err = _find_qualified_agent_target_in(&panes, "kraken.worker-1").unwrap_err();
        assert!(err.contains("unique"));
    }

    #[test]
    fn test_find_qualified_skips_pane_without_team() {
        // A pane with matching agent name but empty team is not a valid target.
        let panes = vec![_pane("kraken.worker-1", "", "kraken", "%1")];
        assert_eq!(
            _find_qualified_agent_target_in(&panes, "kraken.worker-1"),
            Ok(None)
        );
    }

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
    fn test_split_team_address_passes_through_bare_and_malformed() {
        assert_eq!(
            _split_team_address("plain"),
            ("".to_string(), "plain".to_string())
        );
        assert_eq!(
            _split_team_address(".x"),
            ("".to_string(), ".x".to_string())
        );
        assert_eq!(
            _split_team_address("x."),
            ("".to_string(), "x.".to_string())
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
    fn test_derive_agent_name_avoids_seen_and_falls_back() {
        let mut seen: HashSet<String> = ["yoyo", "lulu"].iter().map(|s| s.to_string()).collect();
        let name = _derive_agent_name(&mut seen);
        assert!(_RANDOM_AGENT_NAMES.contains(&name.as_str()));
        assert_ne!(name, "yoyo");
        assert_ne!(name, "lulu");
        assert!(seen.contains(&name));

        let mut all: HashSet<String> = _RANDOM_AGENT_NAMES.iter().map(|s| s.to_string()).collect();
        assert_eq!(_derive_agent_name(&mut all), "agent-1");
        assert_eq!(_derive_agent_name(&mut all), "agent-2");
    }

    #[test]
    fn test_sorted_member_rows_puts_orch_first() {
        let row = |name: &str| {
            let mut m = Map::new();
            m.insert("name".to_string(), Value::String(name.to_string()));
            m
        };
        let sorted = _sorted_member_rows(vec![row("zed"), row("orch"), row("abe")]);
        let names: Vec<String> = sorted.iter().map(|m| map_str(m, "name")).collect();
        assert_eq!(names, vec!["orch", "abe", "zed"]);
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

    #[test]
    fn test_command_tree_declares_every_python_command() {
        let cli = build_cli();
        for name in _KNOWN_COMMANDS {
            assert!(
                cli.find_subcommand(name).is_some(),
                "missing command {name}"
            );
        }
    }

    #[test]
    fn test_render_root_help_sections_present() {
        let help = render_root_help();
        for section in [
            "Daily:",
            "Panes:",
            "Workflow:",
            "Team:",
            "Human Helpers:",
            "Debug:",
            "Extensions:",
            "Launchers:",
            "Examples:",
        ] {
            assert!(help.contains(section), "missing section {section}");
        }
        assert!(!help.contains("resume-hint"), "hidden command leaked");
    }
}
