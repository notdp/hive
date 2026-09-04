use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};

use super::*;
use crate::agent::Agent;
use crate::team::{Team, LEAD_AGENT_NAME};
use crate::tmux;
use crate::tmux::PaneInfo;

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

/// Hive-managed identity for a codex tool thread: a pane record (display
/// bound), or a codex roster row whose sessionId is this thread (a headless
/// member has no pane until `hive attach` materializes one — the registry,
/// not the pane record, is the truth layer).
pub(crate) fn _codex_thread_is_hive_managed(thread_id: &str) -> bool {
    let thread_id = thread_id.trim();
    if thread_id.is_empty() {
        return false;
    }
    if crate::adapters::codex_app_server::pane_for_thread(thread_id)
        .filter(|p| !p.is_empty())
        .is_some()
    {
        return true;
    }
    _codex_thread_member(thread_id).is_some()
}

/// (team, member) of the codex roster row whose sessionId is *thread_id*.
///
/// The self-identity rung for a codex tool: the row match *is* the identity
/// — a claude row carrying the same id is a stranger. The registry records
/// no liveness for a thread, and no cheaper authority exists (the pane
/// record is display binding, not a heartbeat), so liveness is enforced at
/// delivery, where the daemon answers or does not.
pub(crate) fn _codex_thread_member(thread_id: &str) -> Option<(String, String)> {
    _registry_member_matching(thread_id.trim(), Some("codex"))
}

/// The codex member this process's own tool thread belongs to, or None.
pub(crate) fn _codex_thread_member_env() -> Option<(String, String)> {
    _codex_thread_member(&env_string("CODEX_THREAD_ID"))
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
    if !_is_codex_tool_env() || _codex_thread_is_hive_managed(&env_string("CODEX_THREAD_ID")) {
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

/// Names taken in the team: the window's tagged panes, the lead, and the
/// registry roster (a headless or pane-less member owns its name too).
pub(crate) fn _window_seen_names(t: &Team, panes: &[PaneInfo]) -> HashSet<String> {
    let mut seen_names = _names_used_in_window(panes);
    if let Some(entry) = crate::registry::load(&t.name) {
        seen_names.extend(_roster_names(&entry));
    }
    seen_names.insert(if t.lead_name.is_empty() {
        LEAD_AGENT_NAME.to_string()
    } else {
        t.lead_name.clone()
    });
    seen_names
}

/// Member names in a registry entry's roster.
pub(crate) fn _roster_names(entry: &Map<String, Value>) -> HashSet<String> {
    entry
        .get("members")
        .and_then(Value::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(Value::as_object)
                .map(|m| map_str(m, "name"))
                .collect()
        })
        .unwrap_or_default()
}

/// Why *name_override* cannot be claimed against *seen_names*, or None.
pub(crate) fn _member_name_conflict(
    name_override: &str,
    seen_names: &HashSet<String>,
) -> Option<String> {
    if name_override == "flow" || name_override.starts_with("flow.") {
        return Some(format!(
            "'{name_override}' collides with the flow runner's mailbox address kind (flow.run), not a member name"
        ));
    }
    if seen_names.contains(name_override) {
        return Some(format!(
            "name '{name_override}' is already taken in this team"
        ));
    }
    None
}

pub(crate) fn _claim_member_name(name_override: &str, seen_names: &mut HashSet<String>) {
    if name_override.is_empty() {
        return;
    }
    if let Some(error) = _member_name_conflict(name_override, seen_names) {
        fail(&error);
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
    // Registration is transactional: a pane whose native transport refused
    // the join, or a team whose registry entry is gone, must not linger
    // half-registered. Roll every mutation back so a later retry starts clean.
    let rollback = |t: &mut Team| {
        t.agents.retain(|a| a.name != agent_name);
        tmux::clear_pane_tags(pane_id);
        if !ws.is_empty() {
            crate::context::clear_context_for_pane(pane_id);
        }
    };
    if notify {
        if let Err(e) = agent.send(&_hive_join_message(agent_name, team_name)) {
            rollback(t);
            fail(&format!(
                "pane {pane_id} is not reachable over its native transport ({}); \
                 nothing was registered. Fix the inbox/daemon and retry, \
                 or use --no-notify to register without a reachability check.",
                e.0
            ));
        }
    }
    if _registry_record_member(t, &agent) == RecordVerdict::Missing {
        rollback(t);
        fail(&format!(
            "team '{team_name}' has no registry entry (deleted?); nothing was registered"
        ));
    }
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

/// What `_registry_record_member` did with the roster row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordVerdict {
    Written,
    /// The entry belongs to a recycled name's successor: left untouched.
    Stale,
    /// No entry: the team was deleted; nothing is resurrected.
    Missing,
    /// Unsafe team name or store error.
    Rejected,
}

/// Register *agent* in the team's registry roster.
///
/// Only the one row is ever written: a missing entry means the team was
/// deleted under this run and must stay deleted, and a foreign `createdAt`
/// means the name was recycled and the successor's roster is not ours to
/// edit. Both are reported, never repaired, so the caller can back out.
#[must_use = "a Missing verdict leaves the member's engine orphaned unless the caller retires it"]
pub(crate) fn _registry_record_member(t: &Team, agent: &Agent) -> RecordVerdict {
    let row = _member_registry_row(agent);
    let verdict = match crate::registry::record_member(&t.name, &row, &t.created_at_key()) {
        Ok("written") => RecordVerdict::Written,
        Ok("stale") => RecordVerdict::Stale,
        Ok("missing") => RecordVerdict::Missing,
        _ => RecordVerdict::Rejected,
    };
    let why = match verdict {
        RecordVerdict::Written => return verdict,
        RecordVerdict::Stale => "was recreated since this run loaded it",
        RecordVerdict::Missing => "has no registry entry (deleted?)",
        RecordVerdict::Rejected => "refused the registry write",
    };
    eprintln!(
        "warning: team '{}' {why}; '{}' not recorded in its roster",
        t.name, agent.name
    );
    verdict
}

/// Put a pane-less *agent* whose engine is already running on the roster.
/// A team deleted under the spawn cannot hold it: the engine is stopped and
/// the spawn fails, rather than leaving a member no address can reach.
pub(crate) fn _record_headless_member(t: &mut Team, agent: Agent) -> Result<Agent> {
    t.upsert_agent(agent.clone());
    if _registry_record_member(t, &agent) == RecordVerdict::Missing {
        t.retire(&agent.name);
        bail!(
            "team '{}' has no registry entry (deleted?); '{}' retired",
            t.name,
            agent.name
        );
    }
    Ok(agent)
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
    // Team::spawn owns the cross-process name claim and its rollback.
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
    if _registry_record_member(t, &agent) == RecordVerdict::Missing {
        t.retire(agent_name);
        bail!("team '{team_name}' has no registry entry (deleted?); '{agent_name}' retired");
    }
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
    _registry_member_matching(session_id, None)
}

/// Roster row keyed by *session_id*, optionally narrowed to one *cli*.
fn _registry_member_matching(session_id: &str, cli: Option<&str>) -> Option<(String, String)> {
    if session_id.is_empty() {
        return None;
    }
    for entry in crate::registry::list_entries() {
        if let Some(members) = entry.get("members").and_then(Value::as_array) {
            for m in members {
                if let Some(m) = m.as_object() {
                    if map_str(m, "sessionId") == session_id
                        && cli.is_none_or(|cli| map_str(m, "cli") == cli)
                    {
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

    fn _registry_team(name: &str, created_at: f64, members: &[&str]) -> Team {
        let rows: Vec<Map<String, Value>> = members
            .iter()
            .map(|n| {
                let mut m = Map::new();
                m.insert("name".to_string(), Value::String((*n).to_string()));
                m
            })
            .collect();
        crate::registry::record_team(name, "/ws", &py_float_str(created_at), &rows, "").unwrap();
        Team {
            name: name.to_string(),
            created_at,
            ..Default::default()
        }
    }

    fn _headless_agent(name: &str, cli: &str) -> Agent {
        Agent {
            name: name.to_string(),
            team_name: "honey".to_string(),
            pane_id: String::new(),
            model: String::new(),
            prompt: String::new(),
            cwd: "/repo".to_string(),
            session_id: Some("sid-1".to_string()),
            spawned_at: 0.0,
            cli: cli.to_string(),
        }
    }

    fn _roster(name: &str) -> Vec<String> {
        crate::registry::load(name)
            .map(|e| {
                let mut v: Vec<String> = _roster_names(&e).into_iter().collect();
                v.sort();
                v
            })
            .unwrap_or_default()
    }

    #[test]
    fn test_seen_names_include_registry_only_members() {
        let _guard = crate::registry::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        _iso(tmp.path());
        let pool: Vec<&str> = _RANDOM_AGENT_NAMES.to_vec();
        let t = _registry_team("honey", 100.0, &pool);

        let mut seen = _window_seen_names(&t, &[]);

        for name in &pool {
            assert!(seen.contains(*name), "{name}");
        }
        assert_eq!(
            _member_name_conflict(pool[0], &seen).unwrap(),
            format!("name '{}' is already taken in this team", pool[0])
        );
        assert_eq!(_derive_agent_name(&mut seen), "agent-1");
    }

    #[test]
    fn test_record_member_never_resurrects_a_deleted_team() {
        let _guard = crate::registry::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        _iso(tmp.path());
        let t = _registry_team("honey", 100.0, &[]);
        crate::registry::delete_team("honey").unwrap();
        let agent = _headless_agent("worker", "claude");

        assert_eq!(_registry_record_member(&t, &agent), RecordVerdict::Missing);

        assert!(!crate::registry::entry_path("honey").unwrap().exists());
    }

    #[test]
    fn test_headless_spawn_into_a_deleted_team_stops_the_engine() {
        let _guard = crate::registry::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        _iso(tmp.path());
        let mut hook = crate::agent::testhook::Hook::new();
        hook.job_row_ids.push("sid-1".to_string());
        let _hook = crate::agent::testhook::install(hook);
        let mut t = _registry_team("honey", 100.0, &[]);
        crate::registry::delete_team("honey").unwrap();

        let err = _record_headless_member(&mut t, _headless_agent("worker", "claude"))
            .unwrap_err()
            .to_string();

        assert!(err.contains("no registry entry"), "{err}");
        assert!(t.agent_named("worker").is_none());
        assert!(!crate::registry::entry_path("honey").unwrap().exists());
        let stopped = crate::agent::testhook::with(|h| h.stopped.clone()).unwrap();
        assert_eq!(stopped, vec!["sid-1".to_string()]);
    }

    #[test]
    fn test_record_member_leaves_a_recreated_team_alone() {
        let _guard = crate::registry::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        _iso(tmp.path());
        let stale = _registry_team("honey", 100.0, &["old"]);
        let _fresh = _registry_team("honey", 200.0, &["new"]);
        let agent = _headless_agent("worker", "claude");

        assert_eq!(
            _registry_record_member(&stale, &agent),
            RecordVerdict::Stale
        );

        let entry = crate::registry::load("honey").unwrap();
        assert_eq!(entry["createdAt"], "200.0");
        assert_eq!(_roster("honey"), vec!["new".to_string()]);
    }

    #[test]
    fn test_headless_created_at_round_trips_through_record_member() {
        let _guard = crate::registry::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        _iso(tmp.path());
        // an integral epoch is the case `format!("{now}")` got wrong
        let t = _registry_team("honey", 1_700_000_000.0, &[]);
        let agent = _headless_agent("worker", "codex");

        assert_eq!(_registry_record_member(&t, &agent), RecordVerdict::Written);

        assert_eq!(_roster("honey"), vec!["worker".to_string()]);
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

    // --- codex-native gate: headless members are hive-managed via the registry ---

    fn _iso(tmp: &std::path::Path) {
        std::env::set_var("HIVE_HOME", tmp.join("hive"));
        std::env::set_var("CODEX_HOME", tmp.join("codex"));
    }

    #[test]
    fn test_codex_thread_unknown_everywhere_is_unmanaged() {
        let tmp = tempfile::TempDir::new().unwrap();
        _iso(tmp.path());
        assert!(!_codex_thread_is_hive_managed("01aa-unknown"));
        assert!(!_codex_thread_is_hive_managed(""));
    }

    #[test]
    fn test_codex_thread_with_pane_record_is_managed() {
        let tmp = tempfile::TempDir::new().unwrap();
        _iso(tmp.path());
        crate::adapters::codex_app_server::write_pane_thread("%7", "01aa-pane", "/tmp").unwrap();
        assert!(_codex_thread_is_hive_managed("01aa-pane"));
    }

    #[test]
    fn test_codex_thread_matching_registry_member_is_managed() {
        let tmp = tempfile::TempDir::new().unwrap();
        _iso(tmp.path());
        let member: Map<String, Value> = [
            ("name", "review"),
            ("cli", "codex"),
            ("sessionId", "01aa-headless"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
        .collect();
        crate::registry::record_team("rr", "", "1.0", &[member], "").unwrap();
        // no pane record: a headless member's identity is the registry row
        assert!(_codex_thread_is_hive_managed(" 01aa-headless "));
        assert!(!_codex_thread_is_hive_managed("01aa-other"));
        assert_eq!(
            _codex_thread_member(" 01aa-headless "),
            Some(("rr".to_string(), "review".to_string()))
        );
    }

    #[test]
    fn test_codex_thread_matching_claude_row_is_not_managed() {
        let _guard = crate::registry::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        _iso(tmp.path());
        let member: Map<String, Value> = [
            ("name", "orch"),
            ("cli", "claude"),
            ("sessionId", "01aa-claude"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
        .collect();
        crate::registry::record_team("rr", "", "1.0", &[member], "").unwrap();
        // A claude session id colliding with the thread id is a stranger to
        // the codex gate; the generic session lookup still sees the row.
        assert_eq!(_codex_thread_member("01aa-claude"), None);
        assert!(!_codex_thread_is_hive_managed("01aa-claude"));
        assert!(_registry_member_for_session("01aa-claude").is_some());
        std::env::set_var("CODEX_THREAD_ID", "01aa-claude");
        assert_eq!(_codex_thread_member_env(), None);
        std::env::remove_var("CODEX_THREAD_ID");
    }
}
