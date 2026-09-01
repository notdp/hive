//! Team: a tmux window with a group of agents.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

use crate::agent::Agent;
use crate::tmux::PaneInfo;

#[cfg(test)]
use self::tests::fake_layout as layout;
#[cfg(test)]
use self::tests::fake_tmux as tmux;
#[cfg(not(test))]
use crate::layout;
#[cfg(not(test))]
use crate::tmux;

pub const LEAD_AGENT_NAME: &str = "orch";
const _TMUX_REQUIRED_MESSAGE: &str = "Hive requires tmux. Start or attach to a tmux session first.";

/// Python module constant `team.HIVE_HOME`; a per-call env read here so tests
/// can redirect it (nextest runs one process per test).
pub fn hive_home() -> PathBuf {
    let home = std::env::var("HIVE_HOME")
        .unwrap_or_else(|_| format!("{}/.hive", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(home)
}

fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn getcwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Python `str(float)`: integral floats keep a trailing `.0`.
// ponytail: no scientific-notation branch — epoch timestamps never reach 1e16.
pub(crate) fn py_float_str(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e16 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

/// Python truthiness for an optional JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map_or(true, |f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `str(row.get(key) or "")` for the string payloads registry rows carry.
fn row_str(row: &Map<String, Value>, key: &str) -> String {
    match row.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn new_agent(
    name: &str,
    team_name: &str,
    pane_id: &str,
    cli: &str,
    cwd: &str,
    model: &str,
    session_id: Option<String>,
) -> Agent {
    Agent {
        name: name.to_string(),
        team_name: team_name.to_string(),
        pane_id: pane_id.to_string(),
        model: model.to_string(),
        prompt: String::new(),
        cwd: cwd.to_string(),
        session_id,
        spawned_at: now_epoch(),
        cli: cli.to_string(),
    }
}

/// Why *name* cannot be a team name, or "" when it can.
pub fn validate_team_name(name: &str) -> String {
    if name == "ccd" {
        return format!(
            "team name '{name}' is invalid: 'ccd' is the reserved send \
             address for Claude sessions outside any team"
        );
    }
    if name == "flow" {
        return format!(
            "team name '{name}' is invalid: 'flow' is the flow runner's \
             send-address kind (flow.run), not a team name"
        );
    }
    if name.contains('.') {
        return format!(
            "team name '{name}' is invalid: dots separate send-address \
             segments (`<team>.<member>`), so a team name must be dot-free"
        );
    }
    if crate::registry::entry_path(name).is_none() {
        return format!("team name '{name}' is invalid: not a safe registry name");
    }
    String::new()
}

/// What Team.load reads back from `_find_team_window` (the Python window-data
/// dict: window_id / workspace / desc / created).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TeamWindowData {
    pub window_id: String,
    pub workspace: String,
    pub desc: String,
    pub created: String,
}

/// Everything Team.spawn hands to Agent::spawn (also the recorded call shape
/// the tests assert on).
#[derive(Debug, Clone)]
struct SpawnCall {
    name: String,
    team_name: String,
    target_pane: String,
    model: String,
    prompt: String,
    cwd: String,
    is_first: bool,
    split_horizontal: bool,
    split_size: Option<String>,
    skill: String,
    extra_env: Option<HashMap<String, String>>,
    cli: String,
}

// --- cross-module seams (cfg-switched so unit tests mirror the pytest
// monkeypatching without a tmux server or the parallel agent/agent_cli ports)

#[cfg(not(test))]
fn agent_spawn(call: SpawnCall) -> Result<Agent> {
    Agent::spawn(
        &call.name,
        &call.team_name,
        &call.target_pane,
        crate::agent::SpawnOptions {
            model: call.model.clone(),
            prompt: call.prompt.clone(),
            cwd: call.cwd.clone(),
            is_first: call.is_first,
            split_horizontal: call.split_horizontal,
            split_size: call.split_size.clone(),
            skill: call.skill.clone(),
            extra_env: call
                .extra_env
                .as_ref()
                .map(|env| env.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
            cli: call.cli.clone(),
            ..Default::default()
        },
    )
}

#[cfg(test)]
fn agent_spawn(call: SpawnCall) -> Result<Agent> {
    tests::fake_agent::spawn(call)
}

#[cfg(not(test))]
fn detect_current_session_id(cwd: &str, pane_id: &str) -> Option<String> {
    crate::agent::detect_current_session_id(cwd, "", pane_id)
}

#[cfg(test)]
fn detect_current_session_id(cwd: &str, pane_id: &str) -> Option<String> {
    tests::fake_agent::detect_current_session_id(cwd, pane_id)
}

#[cfg(not(test))]
fn agent_kill(agent: &Agent) {
    agent.kill();
}

#[cfg(test)]
fn agent_kill(agent: &Agent) {
    tests::fake_agent::kill(agent);
}

#[cfg(not(test))]
fn member_role_for_pane(pane_id: &str) -> String {
    crate::agent_cli::member_role_for_pane(pane_id).to_string()
}

#[cfg(test)]
fn member_role_for_pane(pane_id: &str) -> String {
    tests::fake_agent_cli::member_role_for_pane(pane_id)
}

/// Python Team.load's cli resolution for a member pane: the pane tag, the
/// pane command, then live profile detection, then the "claude" default.
#[cfg(not(test))]
fn resolve_member_cli(pane: &PaneInfo) -> String {
    let resolved = if !pane.cli.is_empty() {
        pane.cli.clone()
    } else {
        crate::agent_cli::normalize_command(&pane.command)
    };
    if crate::agent_cli::AGENT_CLI_NAMES.contains(&resolved.as_str()) {
        return resolved;
    }
    match crate::agent_cli::detect_profile_for_pane(&pane.pane_id) {
        Some(profile) => profile.name.to_string(),
        None => "claude".to_string(),
    }
}

#[cfg(test)]
fn resolve_member_cli(pane: &PaneInfo) -> String {
    tests::fake_agent_cli::resolve_member_cli(pane)
}

#[cfg(not(test))]
fn find_team_window_for_load(name: &str, prefer_pane: &str) -> Result<(String, TeamWindowData)> {
    _find_team_window(name, prefer_pane)
}

#[cfg(test)]
fn find_team_window_for_load(name: &str, prefer_pane: &str) -> Result<(String, TeamWindowData)> {
    Ok(tests::fake_find_team_window(name, prefer_pane))
}

#[derive(Debug, Clone)]
pub struct Team {
    pub name: String,
    pub description: String,
    pub workspace: String,
    pub lead_name: String,
    /// Insertion-ordered roster (the Python `dict[str, Agent]`); spawn splits
    /// from the *last inserted* member, so order is behavior, not cosmetics.
    pub agents: Vec<Agent>,
    pub created_at: f64,
    pub lead_pane_id: String,
    pub lead_session_id: Option<String>,
    pub tmux_session: String,
    pub tmux_window: String,
    pub tmux_window_id: String,
    pub member_groups: HashMap<String, String>,
}

impl Default for Team {
    fn default() -> Self {
        Team {
            name: String::new(),
            description: String::new(),
            workspace: String::new(),
            lead_name: LEAD_AGENT_NAME.to_string(),
            agents: Vec::new(),
            created_at: now_epoch(),
            lead_pane_id: String::new(),
            lead_session_id: None,
            tmux_session: String::new(),
            tmux_window: String::new(),
            tmux_window_id: String::new(),
            member_groups: HashMap::new(),
        }
    }
}

impl Team {
    pub fn agent_named(&self, name: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.name == name)
    }

    /// `self.agents[name] = agent`: replace in place (keeps the original
    /// insertion position, like a Python dict assignment) or append.
    pub fn upsert_agent(&mut self, agent: Agent) {
        match self.agents.iter_mut().find(|a| a.name == agent.name) {
            Some(slot) => *slot = agent,
            None => self.agents.push(agent),
        }
    }

    // --- Window-level tmux options ---

    fn _write_window_options(&self) {
        let target = &self.tmux_window;
        if target.is_empty() {
            return;
        }
        tmux::configure_hive_window(target);
        tmux::set_window_option(target, "@hive-team", &self.name);
        tmux::set_window_option(target, "@hive-workspace", &self.workspace);
        if !self.description.is_empty() {
            tmux::set_window_option(target, "@hive-desc", &self.description);
        }
        tmux::set_window_option(target, "@hive-created", &py_float_str(self.created_at));
    }

    // --- Lifecycle ---

    /// Create a team bound to *window_target* (not necessarily the focused
    /// window).
    ///
    /// `create()` binds to the currently-focused tmux window, which is wrong
    /// after a `break_pane` moves the lead pane to a fresh window while the
    /// client still views the origin. `create_for_window` takes the final
    /// window explicitly so callers can break out first, then bind the team
    /// where the pane actually landed — team identity must follow the final
    /// window (Bug A).
    #[allow(clippy::too_many_arguments)]
    pub fn create_for_window(
        name: &str,
        window_target: &str,
        lead_pane_id: &str,
        lead_name: &str,
        description: &str,
        cwd: &str,
        workspace: &str,
        tag_lead: bool,
    ) -> Result<Team> {
        if !tmux::is_inside_tmux() {
            bail!("{}", _TMUX_REQUIRED_MESSAGE);
        }
        let error = validate_team_name(name);
        if !error.is_empty() {
            bail!("{}", error);
        }
        if crate::registry::load(name).is_some() {
            // The registry is the name authority: a headless or detached
            // team owns its name (its engines may still be running) until
            // `hive delete` releases it. Never silently clobbered.
            bail!(
                "team '{name}' already exists in the registry \
                 (hive delete {name} releases the name)"
            );
        }

        let existing_team = if !window_target.is_empty() {
            tmux::get_window_option(window_target, "hive-team")
        } else {
            None
        };
        if let Some(existing) = existing_team.filter(|t| !t.is_empty()) {
            bail!("Team '{existing}' already exists in this window");
        }

        let resolved_cwd = if cwd.is_empty() {
            getcwd()
        } else {
            cwd.to_string()
        };
        let mut team = Team {
            name: name.to_string(),
            description: description.to_string(),
            workspace: workspace.to_string(),
            lead_name: lead_name.to_string(),
            ..Default::default()
        };

        team.lead_pane_id = if !lead_pane_id.is_empty() {
            lead_pane_id.to_string()
        } else {
            tmux::get_current_pane_id().unwrap_or_default()
        };
        team.lead_session_id = detect_current_session_id(&resolved_cwd, &team.lead_pane_id);
        team.tmux_session = if window_target.contains(':') {
            window_target.split(':').next().unwrap_or("").to_string()
        } else {
            tmux::get_current_session_name().unwrap_or_default()
        };
        team.tmux_window = window_target.to_string();
        team.tmux_window_id = tmux::get_window_id(window_target).unwrap_or_default();
        if tag_lead && !team.lead_pane_id.is_empty() {
            tmux::tag_pane(
                &team.lead_pane_id,
                &member_role_for_pane(&team.lead_pane_id),
                &team.lead_name,
                name,
                "",
                "",
            );
        }

        team._write_window_options();
        Ok(team)
    }

    /// Create a new team in the currently-focused tmux window.
    pub fn create(name: &str, description: &str, cwd: &str, workspace: &str) -> Result<Team> {
        if !tmux::is_inside_tmux() {
            bail!("{}", _TMUX_REQUIRED_MESSAGE);
        }
        Team::create_for_window(
            name,
            &tmux::get_current_window_target().unwrap_or_default(),
            &tmux::get_current_pane_id().unwrap_or_default(),
            LEAD_AGENT_NAME,
            description,
            cwd,
            workspace,
            true,
        )
    }

    /// Load a team: registry entry for identity and roster, tmux for display.
    ///
    /// The registry is the authoritative record — a team with an entry loads
    /// even when no tmux window renders it (members then have no pane
    /// binding). The tmux window, when one claims the team, binds panes onto
    /// roster members and contributes display-only metadata; a pane-tagged
    /// member missing from the registry still loads (union), so a team
    /// predating the registry writers keeps working.
    /// When *prefer_pane* is given, its window is preferred when multiple
    /// windows claim the same team name.
    pub fn load(name: &str, prefer_pane: &str) -> Result<Team> {
        let snap = crate::registry::load(name);
        let hint = if !prefer_pane.is_empty() {
            prefer_pane.to_string()
        } else {
            tmux::get_current_pane_id().unwrap_or_default()
        };
        let (window_target, window_data) = find_team_window_for_load(name, &hint)?;
        if snap.is_none() && window_target.is_empty() {
            bail!("Team '{name}' not found");
        }

        let snap_workspace = snap
            .as_ref()
            .map(|s| row_str(s, "workspace"))
            .unwrap_or_default();
        let snap_created = match snap.as_ref() {
            Some(s) if truthy(s.get("createdAt")) => match s.get("createdAt") {
                Some(Value::String(text)) => text.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            },
            _ => String::new(),
        };
        let created_source = if !snap_created.is_empty() {
            snap_created
        } else if !window_data.created.is_empty() {
            window_data.created.clone()
        } else {
            "0".to_string()
        };
        let created_at: f64 = created_source.trim().parse().map_err(|_| {
            anyhow::anyhow!("could not convert string to float: '{created_source}'")
        })?;

        let mut team = Team {
            name: name.to_string(),
            description: window_data.desc.clone(),
            workspace: if !snap_workspace.is_empty() {
                snap_workspace
            } else {
                window_data.workspace.clone()
            },
            created_at,
            tmux_session: if window_target.contains(':') {
                window_target.split(':').next().unwrap_or("").to_string()
            } else {
                String::new()
            },
            tmux_window: window_target.clone(),
            tmux_window_id: window_data.window_id.clone(),
            ..Default::default()
        };

        if let Some(snap) = snap.as_ref() {
            if let Some(members) = snap.get("members").and_then(Value::as_array) {
                for row in members {
                    let row = match row.as_object() {
                        Some(r) => r,
                        None => continue,
                    };
                    let member = row_str(row, "name");
                    if member.is_empty() {
                        continue;
                    }
                    let cli = {
                        let c = row_str(row, "cli");
                        if c.is_empty() {
                            "claude".to_string()
                        } else {
                            c
                        }
                    };
                    let session_id = {
                        let s = row_str(row, "sessionId");
                        if s.is_empty() {
                            None
                        } else {
                            Some(s)
                        }
                    };
                    team.upsert_agent(new_agent(
                        &member,
                        name,
                        "",
                        &cli,
                        &row_str(row, "cwd"),
                        &row_str(row, "model"),
                        session_id,
                    ));
                }
            }
        }

        let panes = if !window_target.is_empty() {
            tmux::list_panes_full(&window_target)
        } else {
            Vec::new()
        };
        for pane in panes {
            if pane.team != name {
                continue;
            }
            if pane.role == "agent" {
                if !pane.agent.is_empty() && !pane.group.is_empty() {
                    team.member_groups
                        .insert(pane.agent.clone(), pane.group.clone());
                }
                let resolved_cli = resolve_member_cli(&pane);
                let mut agent = new_agent(
                    &pane.agent,
                    name,
                    &pane.pane_id,
                    &resolved_cli,
                    &tmux::display_value(&pane.pane_id, "#{pane_current_path}").unwrap_or_default(),
                    "",
                    None,
                );
                if resolved_cli == "codex" {
                    // A codex member's session id IS its threadId on the
                    // shared app-server daemon, recorded per pane at
                    // spawn/launch time.
                    agent.session_id =
                        crate::adapters::codex_app_server::thread_id_for_pane(&pane.pane_id);
                } else if resolved_cli == "claude" {
                    // A claude member's durable identity is its bg jobId,
                    // recorded per pane at spawn/launch time — resume
                    // wakes the job, so the jobId is what snapshots and
                    // resume flows carry.
                    agent.session_id = crate::adapters::claude_bg::job_id_for_pane(&pane.pane_id);
                }
                if let Some(registered) = team.agent_named(&pane.agent) {
                    // A live pane is fresher than the registry row for
                    // display-derived fields, but the recorded engine
                    // identity survives a pane whose records were wiped.
                    if agent.session_id.is_none() {
                        agent.session_id = registered.session_id.clone();
                    }
                    if agent.model.is_empty() {
                        agent.model = registered.model.clone();
                    }
                }
                team.upsert_agent(agent);
            }
        }

        Ok(team)
    }

    /// Write team state to tmux options (window + pane level).
    pub fn save(&self) {
        self._write_window_options();
    }

    pub fn lead_agent(&self) -> Option<Agent> {
        if self.lead_pane_id.is_empty() {
            return None;
        }
        Some(Agent {
            name: self.lead_name.clone(),
            team_name: self.name.clone(),
            pane_id: self.lead_pane_id.clone(),
            model: String::new(),
            prompt: String::new(),
            cwd: tmux::display_value(&self.lead_pane_id, "#{pane_current_path}")
                .unwrap_or_else(getcwd),
            session_id: self.lead_session_id.clone(),
            spawned_at: now_epoch(),
            cli: tmux::get_pane_option(&self.lead_pane_id, "hive-cli").unwrap_or_default(),
        })
    }

    // --- Agent management ---

    /// Spawn a new agent in the team.
    pub fn spawn(
        &mut self,
        name: &str,
        model: &str,
        prompt: &str,
        cwd: &str,
        skill: &str,
        extra_env: Option<&HashMap<String, String>>,
        cli: &str,
    ) -> Result<Agent> {
        if name == "flow" || name.starts_with("flow.") {
            bail!(
                "'{name}' collides with the flow runner's mailbox address kind (flow.run), not a member name"
            );
        }
        if self.agent_named(name).is_some() {
            bail!("Agent '{name}' already exists in team '{}'", self.name);
        }
        // A team with a display window is addressable from outside tmux:
        // the split targets the team's own window/panes by id, and targeted
        // tmux commands need no $TMUX. Only a team with no display pins the
        // caller to a tmux context.
        if !tmux::is_inside_tmux() && self.tmux_window.is_empty() {
            bail!("{}", _TMUX_REQUIRED_MESSAGE);
        }

        let is_first = self.agents.is_empty();
        // The team's own window, never the caller's focused one — a spawn
        // issued from another window must land and re-tile where the team
        // lives (kill already resolves the same way).
        let window_for_split = if !self.tmux_window.is_empty() {
            self.tmux_window.clone()
        } else {
            tmux::get_current_window_target().unwrap_or_default()
        };
        // Fallback for an anchor that resolved empty: the team window's own
        // pane. A registry-loaded team carries no lead pane, and roster rows
        // without a live pane (headless spawns, members whose pane died)
        // keep an empty pane_id — `record_member` is retain+push, so such a
        // row sitting *last* is the norm, not a corner.
        let window_anchor = |target: String| -> String {
            if !target.is_empty() || window_for_split.is_empty() {
                return target;
            }
            tmux::list_panes_full(&window_for_split)
                .first()
                .map(|p| p.pane_id.clone())
                .unwrap_or_default()
        };
        let (target, split_horizontal) = if is_first {
            let target = if !self.lead_pane_id.is_empty() {
                self.lead_pane_id.clone()
            } else {
                tmux::get_current_pane_id().unwrap_or_default()
            };
            (
                window_anchor(target),
                layout::split_horizontal(&window_for_split, 2),
            )
        } else {
            // The last member that still has a live pane — not the last
            // roster row, which may be paneless.
            let target = self
                .agents
                .iter()
                .rev()
                .find(|a| !a.pane_id.is_empty())
                .map(|a| a.pane_id.clone())
                .unwrap_or_default();
            (window_anchor(target), false)
        };
        let split_size = "50%";

        let agent = agent_spawn(SpawnCall {
            name: name.to_string(),
            team_name: self.name.clone(),
            target_pane: target,
            model: model.to_string(),
            prompt: prompt.to_string(),
            cwd: if cwd.is_empty() {
                getcwd()
            } else {
                cwd.to_string()
            },
            is_first,
            split_horizontal,
            split_size: Some(split_size.to_string()),
            skill: skill.to_string(),
            extra_env: extra_env.cloned(),
            cli: cli.to_string(),
        })?;

        tmux::tag_pane(&agent.pane_id, "agent", name, &self.name, cli, "");
        self.upsert_agent(agent.clone());

        let window_target = if !self.tmux_window.is_empty() {
            Some(self.tmux_window.clone())
        } else {
            tmux::get_current_window_target()
        };
        if let Some(window_target) = window_target.filter(|w| !w.is_empty()) {
            tmux::configure_hive_window(&window_target);
            let _ = layout::apply_adaptive(&window_target);
        }

        Ok(agent)
    }

    pub fn get(&self, name: &str) -> Result<Agent> {
        if let Some(lead) = self.lead_agent() {
            if name == lead.name {
                return Ok(lead);
            }
        }
        match self.agent_named(name) {
            Some(agent) => Ok(agent.clone()),
            None => bail!("Agent '{name}' not found"),
        }
    }

    /// Get team status.
    pub fn status(&self) -> Map<String, Value> {
        let mut members: Vec<Value> = Vec::new();
        if let Some(lead) = self.lead_agent() {
            let mut row = Map::new();
            row.insert("name".to_string(), Value::String(lead.name.clone()));
            row.insert(
                "role".to_string(),
                Value::String(member_role_for_pane(&lead.pane_id)),
            );
            row.insert("pane".to_string(), Value::String(lead.pane_id.clone()));
            if let Some(group) = self.member_groups.get(&lead.name).filter(|g| !g.is_empty()) {
                row.insert("group".to_string(), Value::String(group.clone()));
            }
            members.push(Value::Object(row));
        }
        let mut names: Vec<&str> = self.agents.iter().map(|a| a.name.as_str()).collect();
        names.sort_unstable();
        for name in names {
            let agent = match self.agent_named(name) {
                Some(a) => a,
                None => continue,
            };
            let mut row = Map::new();
            row.insert("name".to_string(), Value::String(name.to_string()));
            row.insert("role".to_string(), Value::String("agent".to_string()));
            row.insert("pane".to_string(), Value::String(agent.pane_id.clone()));
            if let Some(group) = self.member_groups.get(name).filter(|g| !g.is_empty()) {
                row.insert("group".to_string(), Value::String(group.clone()));
            }
            members.push(Value::Object(row));
        }
        let mut status = Map::new();
        status.insert("name".to_string(), Value::String(self.name.clone()));
        status.insert(
            "description".to_string(),
            Value::String(self.description.clone()),
        );
        status.insert(
            "workspace".to_string(),
            Value::String(self.workspace.clone()),
        );
        status.insert(
            "tmuxSession".to_string(),
            Value::String(self.tmux_session.clone()),
        );
        status.insert(
            "tmuxWindow".to_string(),
            Value::String(self.tmux_window.clone()),
        );
        status.insert("members".to_string(), Value::Array(members));
        status
    }

    /// Kill all agent panes (not the session itself if in-place).
    pub fn cleanup(&self) {
        for agent in &self.agents {
            agent_kill(agent);
        }
        if !self.lead_pane_id.is_empty() && tmux::is_pane_alive(&self.lead_pane_id) {
            tmux::clear_pane_tags(&self.lead_pane_id);
        }
    }
}

/// True when *window_target* still hosts a live pane tagged as a member of
/// *team_name*.
///
/// A window with live members is a real team, not a stale leftover — duplicate
/// resolution must never strip its tags, even when another window claims the
/// same name. Callers destroy window options on False, so a failed tmux
/// listing (unknown) conservatively counts as live: only a successful listing
/// can prove a window stale.
pub fn _window_has_live_team_members(window_target: &str, team_name: &str) -> bool {
    match tmux::list_panes_full_or_none(window_target) {
        None => true,
        Some(panes) => panes
            .iter()
            .any(|p| p.team == team_name && (!p.agent.is_empty() || !p.role.is_empty())),
    }
}

/// Find the tmux window that hosts team *name* by scanning window options.
///
/// When multiple windows claim the same team name (e.g. after a window
/// move/reorder leaves stale tags), the window containing *prefer_pane*
/// wins.  If *prefer_pane* is not supplied we fall back to the window
/// that actually has panes tagged for the team.  Provably-stale duplicates
/// (no live member panes) get their `@hive-team` tag stripped; live
/// duplicates are preserved so two colliding teams never lose their tags.
pub fn _find_team_window(name: &str, prefer_pane: &str) -> Result<(String, TeamWindowData)> {
    let r = tmux::_run(
        &[
            "list-windows",
            "-a",
            "-F",
            "#{session_name}:#{window_index}\t#{window_id}\t#{@hive-team}\t#{@hive-workspace}\t#{@hive-desc}\t#{@hive-created}",
        ],
        false,
        5,
    )?;

    let mut candidates: Vec<(String, TeamWindowData)> = Vec::new();
    for line in r.stdout.trim().split('\n') {
        if line.is_empty() {
            continue;
        }
        let mut parts: Vec<String> = line.split('\t').map(str::to_string).collect();
        if parts.len() == 5 {
            parts.insert(1, String::new());
        }
        while parts.len() < 6 {
            parts.push(String::new());
        }
        if parts[2] == name {
            candidates.push((
                parts[0].clone(),
                TeamWindowData {
                    window_id: parts[1].clone(),
                    workspace: parts[3].clone(),
                    desc: parts[4].clone(),
                    created: parts[5].clone(),
                },
            ));
        }
    }

    if candidates.is_empty() {
        return Ok((String::new(), TeamWindowData::default()));
    }
    if candidates.len() == 1 {
        return Ok(candidates.into_iter().next().unwrap());
    }

    let all_windows: Vec<String> = candidates.iter().map(|c| c.0.clone()).collect();

    // Multiple windows claim this team — resolve the conflict.
    // 1) Prefer the window that contains *prefer_pane*.
    if !prefer_pane.is_empty() {
        if let Some(pane_window) =
            tmux::get_pane_window_target(prefer_pane).filter(|w| !w.is_empty())
        {
            for (wt, data) in &candidates {
                if *wt == pane_window {
                    _gc_stale_team_windows(name, wt, &all_windows);
                    return Ok((wt.clone(), data.clone()));
                }
            }
        }
    }

    // 2) Prefer the window that has panes actually tagged for this team.
    for (wt, data) in &candidates {
        if _window_has_live_team_members(wt, name) {
            _gc_stale_team_windows(name, wt, &all_windows);
            return Ok((wt.clone(), data.clone()));
        }
    }

    // 3) Fall back to first match (shouldn't normally happen).
    Ok(candidates.into_iter().next().unwrap())
}

/// Strip @hive-team (and sibling options) from *provably stale* duplicate
/// windows of *name*.
///
/// A window that still hosts live member panes is left untouched: two live
/// teams that collide on a name must both survive so neither loses its routing
/// tags. `hive doctor` surfaces such collisions for manual repair.
pub fn _gc_stale_team_windows(name: &str, keep: &str, all_windows: &[String]) {
    for wt in all_windows {
        if wt == keep {
            continue;
        }
        if _window_has_live_team_members(wt, name) {
            continue;
        }
        for key in [
            "hive-team",
            "hive-workspace",
            "hive-desc",
            "hive-created",
            "hive-peers",
        ] {
            tmux::clear_window_option(wt, &format!("@{key}"));
        }
    }
}

/// Report tmux windows that collide on the same `@hive-team` name.
///
/// Bug A could leave two live teams tagged with one name across different
/// windows. This scans all windows, groups by team, and returns every group
/// with more than one window — including each window's id, workspace, and live
/// member panes — so `hive doctor` can surface the collision. Detection only:
/// retagging a live team can break hived identity / pane context / pending
/// sends, so repair is left to a human.
pub fn duplicate_team_bindings() -> Result<Vec<Map<String, Value>>> {
    let r = tmux::_run(
        &[
            "list-windows",
            "-a",
            "-F",
            "#{session_name}:#{window_index}\t#{window_id}\t#{@hive-team}\t#{@hive-workspace}",
        ],
        false,
        5,
    )?;

    // serde_json's preserve_order Map keeps team insertion order like the
    // Python dict.
    let mut by_team: Map<String, Value> = Map::new();
    for line in r.stdout.trim().split('\n') {
        if line.is_empty() {
            continue;
        }
        let mut parts: Vec<String> = line.split('\t').map(str::to_string).collect();
        while parts.len() < 4 {
            parts.push(String::new());
        }
        let (window, window_id, team, workspace) = (&parts[0], &parts[1], &parts[2], &parts[3]);
        if team.is_empty() {
            continue;
        }
        let members: Vec<Value> = tmux::list_panes_full(window)
            .iter()
            .filter(|p| p.team == *team && (!p.agent.is_empty() || !p.role.is_empty()))
            .map(|p| json!({"name": p.agent, "pane": p.pane_id, "group": p.group}))
            .collect();
        let row = json!({
            "tmuxWindow": window,
            "windowId": window_id,
            "workspace": workspace,
            "liveMembers": members,
        });
        match by_team
            .entry(team.clone())
            .or_insert_with(|| Value::Array(Vec::new()))
        {
            Value::Array(rows) => rows.push(row),
            _ => unreachable!(),
        }
    }

    let mut duplicates: Vec<Map<String, Value>> = Vec::new();
    for (team, windows) in by_team {
        let count = windows.as_array().map_or(0, Vec::len);
        if count > 1 {
            let mut dupe = Map::new();
            dupe.insert("team".to_string(), Value::String(team));
            dupe.insert("windows".to_string(), windows);
            dupe.insert(
                "repair".to_string(),
                Value::String(
                    "manual: two windows claim this team; do not auto-retag a live team"
                        .to_string(),
                ),
            );
            duplicates.push(dupe);
        }
    }
    Ok(duplicates)
}

/// List all teams: registry entries unioned with tmux-tagged windows.
///
/// A registry entry lists its team whether or not a window renders it; a
/// window row fills in (or contributes teams predating the registry).
pub fn list_teams() -> Result<Vec<Map<String, Value>>> {
    let mut by_name: Map<String, Value> = Map::new();
    for entry in crate::registry::list_entries() {
        let team = row_str(&entry, "team");
        if truthy(entry.get("corrupt")) || team.is_empty() {
            continue;
        }
        by_name.insert(
            team.clone(),
            json!({
                "name": team,
                "tmuxWindow": "",
                "tmuxSession": "",
                "workspace": row_str(&entry, "workspace"),
            }),
        );
    }

    let r = tmux::_run(
        &[
            "list-windows",
            "-a",
            "-F",
            "#{session_name}:#{window_index}\t#{@hive-team}\t#{@hive-workspace}",
        ],
        false,
        5,
    )?;
    for line in r.stdout.trim().split('\n') {
        if line.is_empty() {
            continue;
        }
        let mut parts: Vec<String> = line.split('\t').map(str::to_string).collect();
        while parts.len() < 3 {
            parts.push(String::new());
        }
        if !parts[1].is_empty() {
            let entry = by_name
                .entry(parts[1].clone())
                .or_insert_with(|| json!({"name": parts[1], "workspace": ""}));
            let obj = match entry {
                Value::Object(o) => o,
                _ => unreachable!(),
            };
            obj.insert("tmuxWindow".to_string(), Value::String(parts[0].clone()));
            obj.insert(
                "tmuxSession".to_string(),
                Value::String(if parts[0].contains(':') {
                    parts[0].split(':').next().unwrap_or("").to_string()
                } else {
                    String::new()
                }),
            );
            let workspace = obj
                .get("workspace")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            obj.insert(
                "workspace".to_string(),
                Value::String(if workspace.is_empty() {
                    parts[2].clone()
                } else {
                    workspace
                }),
            );
        }
    }
    let mut out: Vec<Map<String, Value>> = Vec::new();
    for (_, value) in by_name {
        let mut obj = match value {
            Value::Object(o) => o,
            _ => unreachable!(),
        };
        if !obj.contains_key("tmuxWindow") {
            obj.insert("tmuxWindow".to_string(), Value::String(String::new()));
        }
        if !obj.contains_key("tmuxSession") {
            obj.insert("tmuxSession".to_string(), Value::String(String::new()));
        }
        out.push(obj);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::{Run, TmuxError};
    use std::cell::RefCell;
    use std::sync::MutexGuard;

    // ------------------------------------------------------------------
    // Fake state shared by fake_tmux / fake_layout / fake_agent /
    // fake_agent_cli (the monkeypatch equivalent of tests/conftest.py's
    // configure_hive_home fixture).
    // ------------------------------------------------------------------

    pub struct FakeState {
        pub tmux_inside: bool,
        pub current_pane: String,
        pub session_name: String,
        pub current_window_target: Option<String>,
        pub window_options: HashMap<String, HashMap<String, String>>,
        pub pane_options: HashMap<String, HashMap<String, String>>,
        pub default_command: String,
        pub pane_commands: HashMap<String, String>,
        pub pane_alive: bool,
        pub tagged: Vec<(String, String, String, String, String, String)>,
        pub borders: Vec<String>,
        pub cleared: Vec<(String, String)>,
        pub display_values: HashMap<(String, String), String>,
        pub default_display_value: Option<String>,
        pub pane_window_targets: HashMap<String, String>,
        pub run_fn: Option<Box<dyn Fn(&[String], bool) -> Result<Run, TmuxError>>>,
        pub list_panes_full_fn: Option<Box<dyn Fn(&str) -> Vec<PaneInfo>>>,
        pub list_panes_full_or_none_fn: Option<Box<dyn Fn(&str) -> Option<Vec<PaneInfo>>>>,
        pub window_size: (i64, i64),
        pub layout_panes: Vec<String>,
        pub layout_actions: Vec<(String, String, String, String)>,
        pub find_override: Option<Box<dyn Fn(&str, &str) -> (String, TeamWindowData)>>,
        pub detect_session_id: Option<String>,
        pub spawn_calls: Vec<SpawnCall>,
        pub spawn_fn: Option<Box<dyn Fn(usize, &SpawnCall) -> Agent>>,
        pub killed: Vec<String>,
    }

    impl Default for FakeState {
        fn default() -> Self {
            FakeState {
                tmux_inside: true,
                current_pane: "%0".to_string(),
                session_name: "dev".to_string(),
                current_window_target: Some("dev:0".to_string()),
                window_options: HashMap::new(),
                pane_options: HashMap::new(),
                default_command: "claude".to_string(),
                pane_commands: HashMap::new(),
                pane_alive: true,
                tagged: Vec::new(),
                borders: Vec::new(),
                cleared: Vec::new(),
                display_values: HashMap::new(),
                default_display_value: None,
                pane_window_targets: HashMap::new(),
                run_fn: None,
                list_panes_full_fn: None,
                list_panes_full_or_none_fn: None,
                window_size: (0, 0),
                layout_panes: Vec::new(),
                layout_actions: Vec::new(),
                find_override: None,
                detect_session_id: None,
                spawn_calls: Vec::new(),
                spawn_fn: None,
                killed: Vec::new(),
            }
        }
    }

    thread_local! {
        static STATE: RefCell<FakeState> = RefCell::new(FakeState::default());
    }

    pub fn with_state<R>(f: impl FnOnce(&mut FakeState) -> R) -> R {
        STATE.with(|state| f(&mut state.borrow_mut()))
    }

    fn window_id_for_target(target: &str) -> String {
        let suffix = if target.contains(':') {
            target.rsplit(':').next().unwrap_or("0")
        } else {
            "0"
        };
        format!("@{suffix}")
    }

    /// conftest's state-backed `_find_team_window` patch (with a per-test
    /// override slot, like the tests that re-monkeypatch it).
    pub fn fake_find_team_window(name: &str, prefer_pane: &str) -> (String, TeamWindowData) {
        with_state(|st| {
            if let Some(f) = &st.find_override {
                return f(name, prefer_pane);
            }
            for (target, opts) in &st.window_options {
                if opts.get("hive-team").map(String::as_str) == Some(name) {
                    return (
                        target.clone(),
                        TeamWindowData {
                            window_id: window_id_for_target(target),
                            workspace: opts.get("hive-workspace").cloned().unwrap_or_default(),
                            desc: opts.get("hive-desc").cloned().unwrap_or_default(),
                            created: opts
                                .get("hive-created")
                                .cloned()
                                .unwrap_or_else(|| "0".to_string()),
                        },
                    );
                }
            }
            (String::new(), TeamWindowData::default())
        })
    }

    pub mod fake_tmux {
        use super::with_state;
        use crate::tmux::{PaneInfo, Run, TmuxError};

        fn strip(option: &str) -> String {
            option.trim_start_matches('@').to_string()
        }

        pub fn is_inside_tmux() -> bool {
            with_state(|st| st.tmux_inside)
        }

        pub fn get_current_pane_id() -> Option<String> {
            with_state(|st| Some(st.current_pane.clone()))
        }

        pub fn get_current_session_name() -> Option<String> {
            with_state(|st| Some(st.session_name.clone()))
        }

        pub fn get_current_window_target() -> Option<String> {
            with_state(|st| st.current_window_target.clone())
        }

        pub fn get_window_id(target: &str) -> Option<String> {
            Some(super::window_id_for_target(target))
        }

        pub fn get_window_option(target: &str, key: &str) -> Option<String> {
            with_state(|st| {
                st.window_options
                    .get(target)
                    .and_then(|opts| opts.get(key))
                    .cloned()
            })
        }

        pub fn set_window_option(target: &str, option: &str, value: &str) {
            with_state(|st| {
                st.window_options
                    .entry(target.to_string())
                    .or_default()
                    .insert(strip(option), value.to_string());
            });
        }

        pub fn clear_window_option(target: &str, option: &str) {
            with_state(|st| {
                st.cleared.push((target.to_string(), option.to_string()));
                if let Some(opts) = st.window_options.get_mut(target) {
                    opts.remove(&strip(option));
                }
            });
        }

        pub fn enable_pane_border_status(target: &str) {
            with_state(|st| st.borders.push(target.to_string()));
        }

        pub fn configure_hive_window(target: &str) {
            enable_pane_border_status(target);
            set_window_option(target, "monitor-activity", "off");
            set_window_option(target, "monitor-bell", "off");
        }

        pub fn tag_pane(
            pane_id: &str,
            role: &str,
            agent: &str,
            team: &str,
            cli: &str,
            group: &str,
        ) {
            with_state(|st| {
                st.tagged.push((
                    pane_id.to_string(),
                    role.to_string(),
                    agent.to_string(),
                    team.to_string(),
                    cli.to_string(),
                    group.to_string(),
                ));
                let opts = st.pane_options.entry(pane_id.to_string()).or_default();
                opts.insert("hive-role".to_string(), role.to_string());
                opts.insert("hive-agent".to_string(), agent.to_string());
                opts.insert("hive-team".to_string(), team.to_string());
                if !cli.is_empty() {
                    opts.insert("hive-cli".to_string(), cli.to_string());
                }
                if !group.is_empty() {
                    opts.insert("hive-group".to_string(), group.to_string());
                }
            });
        }

        pub fn clear_pane_tags(pane_id: &str) {
            with_state(|st| {
                st.pane_options.remove(pane_id);
            });
        }

        pub fn is_pane_alive(_pane_id: &str) -> bool {
            with_state(|st| st.pane_alive)
        }

        pub fn get_pane_option(pane_id: &str, key: &str) -> Option<String> {
            with_state(|st| {
                st.pane_options
                    .get(pane_id)
                    .and_then(|opts| opts.get(key))
                    .cloned()
            })
        }

        pub fn get_pane_current_command(pane_id: &str) -> Option<String> {
            with_state(|st| {
                Some(
                    st.pane_commands
                        .get(pane_id)
                        .cloned()
                        .unwrap_or_else(|| st.default_command.clone()),
                )
            })
        }

        pub fn display_value(target: &str, fmt: &str) -> Option<String> {
            with_state(|st| {
                st.display_values
                    .get(&(target.to_string(), fmt.to_string()))
                    .cloned()
                    .or_else(|| st.default_display_value.clone())
            })
        }

        pub fn get_pane_window_target(pane_id: &str) -> Option<String> {
            with_state(|st| st.pane_window_targets.get(pane_id).cloned())
        }

        pub fn list_panes_full(target: &str) -> Vec<PaneInfo> {
            with_state(|st| {
                if let Some(f) = &st.list_panes_full_fn {
                    return f(target);
                }
                if target.is_empty() {
                    return Vec::new();
                }
                let mut out = Vec::new();
                for (pane_id, opts) in &st.pane_options {
                    if opts.get("hive-team").map_or(false, |t| !t.is_empty()) {
                        out.push(PaneInfo {
                            pane_id: pane_id.clone(),
                            title: String::new(),
                            command: "claude".to_string(),
                            role: opts.get("hive-role").cloned().unwrap_or_default(),
                            agent: opts.get("hive-agent").cloned().unwrap_or_default(),
                            team: opts.get("hive-team").cloned().unwrap_or_default(),
                            cli: opts.get("hive-cli").cloned().unwrap_or_default(),
                            group: opts.get("hive-group").cloned().unwrap_or_default(),
                        });
                    }
                }
                out
            })
        }

        pub fn list_panes_full_or_none(target: &str) -> Option<Vec<PaneInfo>> {
            let via_override =
                with_state(|st| st.list_panes_full_or_none_fn.as_ref().map(|f| f(target)));
            if let Some(result) = via_override {
                return result;
            }
            let mut panes = list_panes_full(target);
            let current = with_state(|st| st.current_pane.clone());
            if !panes.iter().any(|p| p.pane_id == current) {
                panes.push(PaneInfo {
                    pane_id: current,
                    ..Default::default()
                });
            }
            Some(panes)
        }

        pub fn _run(args: &[&str], check: bool, _timeout: u64) -> Result<Run, TmuxError> {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            with_state(|st| match &st.run_fn {
                Some(f) => f(&owned, check),
                None => Ok(Run {
                    returncode: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }),
            })
        }
    }

    /// Fake layout runs the REAL `layout::pick` over fake tmux state — the
    /// same shape as the pytest suite, which patches only `hive.layout.tmux`.
    pub mod fake_layout {
        use super::with_state;

        pub fn split_horizontal(window_target: &str, pane_count_after: usize) -> bool {
            if window_target.is_empty() {
                return true;
            }
            let size = with_state(|st| st.window_size);
            match crate::layout::pick(size, pane_count_after) {
                None => true,
                Some(choice) => choice.orientation == "horizontal",
            }
        }

        pub fn apply_adaptive(window_target: &str) -> Option<crate::layout::LayoutChoice> {
            if window_target.is_empty() {
                return None;
            }
            let (size, count) = with_state(|st| (st.window_size, st.layout_panes.len()));
            let choice = crate::layout::pick(size, count)?;
            with_state(|st| {
                for (key, value) in &choice.options {
                    st.layout_actions.push((
                        "opt".to_string(),
                        window_target.to_string(),
                        key.to_string(),
                        value.to_string(),
                    ));
                }
                st.layout_actions.push((
                    "layout".to_string(),
                    window_target.to_string(),
                    choice.preset.to_string(),
                    String::new(),
                ));
            });
            Some(choice)
        }
    }

    pub mod fake_agent {
        use super::super::{new_agent, SpawnCall};
        use super::with_state;
        use crate::agent::Agent;

        pub fn detect_current_session_id(_cwd: &str, _pane_id: &str) -> Option<String> {
            with_state(|st| st.detect_session_id.clone())
        }

        pub fn spawn(call: SpawnCall) -> anyhow::Result<Agent> {
            with_state(|st| {
                st.spawn_calls.push(call.clone());
                let n = st.spawn_calls.len();
                let agent = match &st.spawn_fn {
                    Some(f) => f(n, &call),
                    None => new_agent(
                        &call.name,
                        &call.team_name,
                        "%9",
                        &call.cli,
                        &call.cwd,
                        &call.model,
                        None,
                    ),
                };
                Ok(agent)
            })
        }

        pub fn kill(agent: &Agent) {
            with_state(|st| st.killed.push(agent.name.clone()));
        }
    }

    pub mod fake_agent_cli {
        use super::fake_tmux;
        use crate::tmux::PaneInfo;

        fn normalize(command: &str) -> String {
            let value = command.trim().to_lowercase();
            let value = value
                .rsplit('/')
                .next()
                .unwrap_or("")
                .trim_start_matches('-');
            match value {
                "claude-code" | "claudecode" | "claude.exe" => "claude".to_string(),
                other => other.to_string(),
            }
        }

        fn is_agent_command(command: &str) -> bool {
            matches!(normalize(command).as_str(), "claude" | "codex" | "grok")
        }

        pub fn member_role_for_pane(pane_id: &str) -> String {
            let command = fake_tmux::get_pane_current_command(pane_id).unwrap_or_default();
            if is_agent_command(&command) {
                "agent".to_string()
            } else {
                "terminal".to_string()
            }
        }

        pub fn resolve_member_cli(pane: &PaneInfo) -> String {
            let resolved = if !pane.cli.is_empty() {
                pane.cli.clone()
            } else {
                normalize(&pane.command)
            };
            if matches!(resolved.as_str(), "claude" | "codex" | "grok") {
                resolved
            } else {
                "claude".to_string()
            }
        }
    }

    // ------------------------------------------------------------------
    // Test helpers
    // ------------------------------------------------------------------

    /// Mirror of tests/conftest.py's `configure_hive_home` fixture.
    fn configure_hive_home(
        tmux_inside: bool,
        current_pane: &str,
    ) -> (tempfile::TempDir, MutexGuard<'static, ()>) {
        let guard = crate::registry::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path().join(".hive"));
        std::env::set_var("CODEX_HOME", tmp.path().join(".codex"));
        std::env::set_var("CLAUDE_HOME", tmp.path().join(".claude"));
        std::env::set_var("GROK_HOME", tmp.path().join(".grok"));
        std::env::set_var("XDG_CACHE_HOME", tmp.path().join(".cache"));
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path().join("claude-env-isolation"));
        std::env::remove_var("CLAUDE_CODE_MESSAGING_SOCKET");
        std::env::remove_var("CODEX_THREAD_ID");
        std::env::remove_var("HIVE_TEAM");
        std::env::remove_var("HIVE_MEMBER");
        with_state(|st| {
            *st = FakeState::default();
            st.tmux_inside = tmux_inside;
            st.current_pane = current_pane.to_string();
        });
        (tmp, guard)
    }

    fn pane_info(pane_id: &str, command: &str, role: &str, agent: &str, team: &str) -> PaneInfo {
        PaneInfo {
            pane_id: pane_id.to_string(),
            title: String::new(),
            command: command.to_string(),
            role: role.to_string(),
            agent: agent.to_string(),
            team: team.to_string(),
            cli: String::new(),
            group: String::new(),
        }
    }

    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(o) => o,
            _ => unreachable!(),
        }
    }

    fn run_stdout(stdout: &'static str) -> Box<dyn Fn(&[String], bool) -> Result<Run, TmuxError>> {
        Box::new(move |_args, _check| {
            Ok(Run {
                returncode: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            })
        })
    }

    // ------------------------------------------------------------------
    // Ported tests (tests/unit/test_team.py, plus the pure-logic
    // validate_team_name test from tests/cli/test_message_commands.py)
    // ------------------------------------------------------------------

    #[test]
    fn test_team_create_inside_tmux_tags_lead_and_detects_session() {
        let (_tmp, _guard) = configure_hive_home(true, "%7");
        with_state(|st| st.detect_session_id = Some("sess-123".to_string()));

        let team = Team::create("team-a", "demo", "", "/tmp/ws").unwrap();

        assert_eq!(team.lead_pane_id, "%7");
        assert_eq!(team.lead_session_id.as_deref(), Some("sess-123"));
        assert_eq!(team.tmux_session, "dev");
        assert_eq!(team.tmux_window, "dev:0");
        assert_eq!(team.tmux_window_id, "@0");
        let tagged = with_state(|st| st.tagged.clone());
        assert_eq!(
            tagged,
            vec![(
                "%7".to_string(),
                "agent".to_string(),
                "orch".to_string(),
                "team-a".to_string(),
                String::new(),
                String::new()
            )]
        );
        assert_eq!(
            with_state(|st| st.borders.clone()),
            vec!["dev:0".to_string()]
        );
        assert_eq!(
            fake_tmux::get_window_option("dev:0", "monitor-activity").as_deref(),
            Some("off")
        );
        assert_eq!(
            fake_tmux::get_window_option("dev:0", "monitor-bell").as_deref(),
            Some("off")
        );
    }

    #[test]
    fn test_team_create_rejects_outside_tmux() {
        let (_tmp, _guard) = configure_hive_home(false, "%0");

        let err = Team::create("team-a", "", "", "").unwrap_err();
        assert!(err.to_string().contains("requires tmux"), "{err}");
    }

    #[test]
    fn test_team_create_rejects_reserved_or_dotted_names() {
        // `hive send` parses `<team>.<member>` / `ccd.<session>`: a team named
        // ccd, or one carrying a dot, would be unaddressable
        let (_tmp, _guard) = configure_hive_home(true, "%0");

        for name in ["ccd", "ccd.desk", "a.b"] {
            let err = Team::create(name, "", "", "").unwrap_err();
            assert!(err.to_string().contains("invalid"), "{name}: {err}");
        }
    }

    #[test]
    fn test_team_save_and_load_round_trip() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        let mut team = Team {
            name: "team-a".to_string(),
            description: "demo".to_string(),
            workspace: "/tmp/ws".to_string(),
            lead_pane_id: "%0".to_string(),
            lead_session_id: Some("sess-1".to_string()),
            tmux_session: "dev".to_string(),
            tmux_window: "dev:0".to_string(),
            ..Default::default()
        };
        team.agents.push(new_agent(
            "claude", "team-a", "%1", "claude", "/tmp", "m1", None,
        ));

        team.save();
        assert_eq!(
            with_state(|st| st.borders.clone()),
            vec!["dev:0".to_string()]
        );

        // Set up pane tags for load to find (in real usage, set during create/spawn)
        fake_tmux::tag_pane("%0", "agent", "orch", "team-a", "claude", "");
        fake_tmux::tag_pane("%1", "agent", "claude", "team-a", "claude", "");

        let loaded = Team::load("team-a", "").unwrap();

        assert_eq!(loaded.name, "team-a");
        assert_eq!(loaded.description, "demo");
        assert_eq!(loaded.tmux_window, "dev:0");
        assert_eq!(loaded.tmux_window_id, "@0");
        assert_eq!(loaded.agent_named("orch").unwrap().pane_id, "%0");
        assert_eq!(loaded.agent_named("claude").unwrap().pane_id, "%1");
    }

    #[test]
    fn test_team_load_restores_agent_cwd_from_pane_current_path() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        with_state(|st| {
            st.find_override = Some(Box::new(|_name, _prefer| {
                (
                    "dev:0".to_string(),
                    TeamWindowData {
                        workspace: "/tmp/ws".to_string(),
                        created: "0".to_string(),
                        ..Default::default()
                    },
                )
            }));
            st.list_panes_full_fn = Some(Box::new(|_target| {
                let mut pane = pane_info("%1", "claude", "agent", "claude", "team-a");
                pane.cli = "claude".to_string();
                vec![pane]
            }));
            st.display_values.insert(
                ("%1".to_string(), "#{pane_current_path}".to_string()),
                "/repo".to_string(),
            );
        });

        let loaded = Team::load("team-a", "").unwrap();

        assert_eq!(loaded.agent_named("claude").unwrap().cwd, "/repo");
    }

    #[test]
    fn test_team_lead_agent_uses_persisted_session_id() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        let team = Team {
            name: "team-a".to_string(),
            lead_pane_id: "%0".to_string(),
            lead_session_id: Some("sess-1".to_string()),
            ..Default::default()
        };

        let lead = team.lead_agent();

        let lead = lead.expect("lead agent");
        assert_eq!(lead.name, "orch");
        assert_eq!(lead.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn test_team_spawn_tags_agent_and_passes_skill() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        let agent = new_agent("claude", "team-a", "%9", "claude", "", "", None);
        let spawn_result = agent.clone();
        with_state(move |st| {
            st.spawn_fn = Some(Box::new(move |_n, _call| spawn_result.clone()));
            st.current_window_target = Some("dev:1".to_string());
            st.window_size = (200, 50);
            st.layout_panes = vec!["%1".to_string(), "%9".to_string()];
        });

        let mut team = Team {
            name: "team-a".to_string(),
            lead_pane_id: "%0".to_string(),
            ..Default::default()
        };
        let result = team
            .spawn("claude", "", "start now", "", "demo-review", None, "claude")
            .unwrap();

        assert_eq!(result.pane_id, agent.pane_id);
        assert_eq!(result.name, agent.name);
        let calls = with_state(|st| st.spawn_calls.clone());
        assert_eq!(calls[0].target_pane, "%0");
        assert_eq!(calls[0].skill, "demo-review");
        assert_eq!(calls[0].prompt, "start now");
        let tagged = with_state(|st| st.tagged.clone());
        assert_eq!(
            tagged,
            vec![(
                "%9".to_string(),
                "agent".to_string(),
                "claude".to_string(),
                "team-a".to_string(),
                "claude".to_string(),
                String::new()
            )]
        );
        assert!(with_state(|st| st.borders.clone()).contains(&"dev:1".to_string()));
    }

    /// Guards Bug 1 regression: portrait window must end on `even-vertical`,
    /// not the legacy hardcoded `main-vertical`.
    #[test]
    fn test_team_spawn_portrait_window_applies_even_vertical() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        let agent = new_agent("claude", "team-a", "%9", "claude", "", "", None);
        with_state(move |st| {
            st.spawn_fn = Some(Box::new(move |_n, _call| agent.clone()));
            st.current_window_target = Some("dev:1".to_string());
            st.window_size = (191, 171);
            st.layout_panes = vec!["%0".to_string(), "%9".to_string()];
        });

        let mut team = Team {
            name: "team-a".to_string(),
            lead_pane_id: "%0".to_string(),
            ..Default::default()
        };
        team.spawn("claude", "", "", "", "hive", None, "claude")
            .unwrap();

        let layouts = with_state(|st| st.layout_actions.clone());
        assert!(layouts.contains(&(
            "layout".to_string(),
            "dev:1".to_string(),
            "even-vertical".to_string(),
            String::new()
        )));
        // Portrait must not set main-pane-width.
        assert!(!layouts
            .iter()
            .any(|call| call.0 == "opt" && call.2 == "main-pane-width"));
        // Pre-spawn split should also follow portrait orientation (vertical = False).
        let calls = with_state(|st| st.spawn_calls.clone());
        assert!(!calls[0].split_horizontal);
    }

    #[test]
    fn test_team_spawn_second_agent_splits_from_last_agent() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        with_state(|st| {
            st.spawn_fn = Some(Box::new(|n, call| {
                super::new_agent(
                    &call.name,
                    "team-a",
                    &format!("%{}", n + 8),
                    "claude",
                    "",
                    "",
                    None,
                )
            }));
            st.current_window_target = None;
        });

        let mut team = Team {
            name: "team-a".to_string(),
            lead_pane_id: "%0".to_string(),
            ..Default::default()
        };
        team.agents
            .push(new_agent("claude", "team-a", "%9", "claude", "", "", None));
        team.spawn("gpt", "", "", "", "hive", None, "claude")
            .unwrap();

        let calls = with_state(|st| st.spawn_calls.clone());
        assert_eq!(calls[0].target_pane, "%9");
        assert!(!calls[0].split_horizontal);
        assert_eq!(calls[0].skill, "hive");
    }

    #[test]
    fn test_team_get_resolves_lead_and_members() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        let alive = new_agent("claude", "team-a", "%1", "claude", "", "", None);

        let mut team = Team {
            name: "team-a".to_string(),
            lead_pane_id: "%0".to_string(),
            ..Default::default()
        };
        team.agents.push(alive);

        assert_eq!(team.get("orch").unwrap().pane_id, "%0");
        assert_eq!(team.get("claude").unwrap().pane_id, "%1");
    }

    #[test]
    fn test_team_status_stays_local_only() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        with_state(|st| {
            st.default_command = "zsh".to_string();
            st.pane_commands
                .insert("%1".to_string(), "codex".to_string());
        });

        let mut team = Team {
            name: "team-a".to_string(),
            lead_pane_id: "%0".to_string(),
            ..Default::default()
        };
        team.agents
            .push(new_agent("claude", "team-a", "%1", "claude", "", "", None));

        let payload = team.status();

        let members = payload.get("members").unwrap().as_array().unwrap();
        let orch = members
            .iter()
            .find(|m| m.get("name").unwrap() == "orch")
            .unwrap()
            .as_object()
            .unwrap();
        let claude = members
            .iter()
            .find(|m| m.get("name").unwrap() == "claude")
            .unwrap()
            .as_object()
            .unwrap();
        for row in [orch, claude] {
            assert!(!row.contains_key("sessionId"));
            assert!(!row.contains_key("model"));
            assert!(!row.contains_key("alive"));
        }
    }

    /// When two windows claim the same team, the one containing prefer_pane wins.
    ///
    /// The losing duplicate here is stale (no live member panes), so it is cleared.
    #[test]
    fn test_find_team_window_prefers_pane_window_on_duplicate() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        with_state(|st| {
            st.run_fn = Some(run_stdout(
                "dev:2\tmy-team\t/tmp/ws\tdesc\t0\ndev:3\tmy-team\t/tmp/ws\tdesc\t0\n",
            ));
            st.pane_window_targets
                .insert("%99".to_string(), "dev:3".to_string());
            // No live member panes anywhere → the losing window dev:2 is provably stale.
            st.list_panes_full_or_none_fn = Some(Box::new(|_target| Some(Vec::new())));
        });

        let (wt, _data) = _find_team_window("my-team", "%99").unwrap();

        assert_eq!(wt, "dev:3");
        let cleared = with_state(|st| st.cleared.clone());
        assert!(cleared.iter().any(|(w, _)| w == "dev:2"));
    }

    /// Without prefer_pane, pick the window that actually has tagged panes.
    #[test]
    fn test_find_team_window_falls_back_to_tagged_panes() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        with_state(|st| {
            st.run_fn = Some(run_stdout(
                "dev:2\tmy-team\t/tmp/ws\tdesc\t0\ndev:3\tmy-team\t/tmp/ws\tdesc\t0\n",
            ));
            st.list_panes_full_or_none_fn = Some(Box::new(|target| {
                if target == "dev:3" {
                    Some(vec![pane_info("%50", "codex", "agent", "rev-a", "my-team")])
                } else {
                    Some(vec![pane_info("%40", "codex", "", "", "")])
                }
            }));
        });

        let (wt, _data) = _find_team_window("my-team", "").unwrap();

        assert_eq!(wt, "dev:3");
        let cleared = with_state(|st| st.cleared.clone());
        assert!(cleared.iter().any(|(w, _)| w == "dev:2"));
    }

    #[test]
    fn test_gc_stale_team_windows_clears_non_kept() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        // All duplicates are stale (no live member panes) → all non-kept get cleared.
        with_state(|st| {
            st.list_panes_full_or_none_fn = Some(Box::new(|_target| Some(Vec::new())));
        });

        _gc_stale_team_windows(
            "my-team",
            "dev:3",
            &[
                "dev:2".to_string(),
                "dev:3".to_string(),
                "dev:4".to_string(),
            ],
        );

        let cleared = with_state(|st| st.cleared.clone());
        let stale_windows: std::collections::HashSet<String> =
            cleared.iter().map(|(w, _)| w.clone()).collect();
        assert_eq!(
            stale_windows,
            ["dev:2".to_string(), "dev:4".to_string()]
                .into_iter()
                .collect()
        );
        assert!(!cleared.contains(&("dev:3".to_string(), "@hive-team".to_string())));
    }

    /// A duplicate window with live member panes is never cleared (Bug A safety).
    #[test]
    fn test_gc_stale_team_windows_skips_live_duplicate() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        with_state(|st| {
            st.list_panes_full_or_none_fn = Some(Box::new(|target| {
                if target == "dev:2" {
                    Some(vec![pane_info(
                        "%40",
                        "codex",
                        "agent",
                        "validator",
                        "my-team",
                    )])
                } else {
                    Some(Vec::new())
                }
            }));
        });

        _gc_stale_team_windows(
            "my-team",
            "dev:3",
            &[
                "dev:2".to_string(),
                "dev:3".to_string(),
                "dev:4".to_string(),
            ],
        );

        let cleared: Vec<String> =
            with_state(|st| st.cleared.iter().map(|(w, _)| w.clone()).collect());
        assert!(!cleared.contains(&"dev:2".to_string())); // live duplicate preserved
        assert!(cleared.contains(&"dev:4".to_string())); // stale duplicate still cleared
    }

    /// A failed pane listing is unknown, not proof of staleness — clear nothing.
    #[test]
    fn test_gc_stale_team_windows_skips_cleanup_on_tmux_failure() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        with_state(|st| {
            st.list_panes_full_or_none_fn = Some(Box::new(|_target| None));
        });

        _gc_stale_team_windows(
            "my-team",
            "dev:3",
            &[
                "dev:2".to_string(),
                "dev:3".to_string(),
                "dev:4".to_string(),
            ],
        );

        assert!(with_state(|st| st.cleared.clone()).is_empty());
    }

    /// Two live windows share a team name; prefer_pane picks one for routing and
    /// the other keeps its tags. Bug A: never clobber a live duplicate.
    #[test]
    fn test_find_team_window_keeps_live_duplicate() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        with_state(|st| {
            st.run_fn = Some(run_stdout(
                "dev:2\t0-2\t/tmp/ws2\tdesc\t0\ndev:3\t0-2\t/tmp/ws3\tdesc\t0\n",
            ));
            st.pane_window_targets
                .insert("%40".to_string(), "dev:3".to_string());
            st.list_panes_full_or_none_fn = Some(Box::new(|target| {
                if target == "dev:2" {
                    Some(vec![
                        pane_info("%10", "claude", "agent", "worker", "0-2"),
                        pane_info("%11", "codex", "agent", "validator", "0-2"),
                    ])
                } else if target == "dev:3" {
                    Some(vec![
                        pane_info("%40", "claude", "agent", "worker", "0-2"),
                        pane_info("%41", "codex", "agent", "validator", "0-2"),
                    ])
                } else {
                    Some(Vec::new())
                }
            }));
        });

        let (wt, _data) = _find_team_window("0-2", "%40").unwrap();

        assert_eq!(wt, "dev:3"); // prefer_pane window wins for routing
        let cleared: Vec<String> =
            with_state(|st| st.cleared.iter().map(|(w, _)| w.clone()).collect());
        assert!(!cleared.contains(&"dev:2".to_string())); // the other live duplicate keeps its tags
    }

    /// Two windows sharing a team name are reported with their ids + live members;
    /// a uniquely-named team is not.
    #[test]
    fn test_duplicate_team_bindings_reports_only_collisions() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        with_state(|st| {
            st.run_fn = Some(run_stdout(
                "0:2\t@2\t0-2\t/tmp/hive-0-w2\n0:3\t@3\t0-2\t/tmp/hive-0-w3\n0:5\t@5\tsolo\t/tmp/hive-0-w5\n",
            ));
            st.list_panes_full_fn = Some(Box::new(|target| match target {
                "0:2" => vec![pane_info("%42", "claude", "agent", "worker", "0-2")],
                "0:3" => vec![pane_info("%10", "claude", "agent", "worker", "0-2")],
                "0:5" => vec![pane_info("%80", "claude", "agent", "worker", "solo")],
                _ => Vec::new(),
            }));
        });

        let dupes = duplicate_team_bindings().unwrap();

        assert_eq!(dupes.len(), 1); // only the colliding team, not the unique "solo"
        assert_eq!(dupes[0].get("team").unwrap(), "0-2");
        let windows = dupes[0].get("windows").unwrap().as_array().unwrap();
        let window_ids: std::collections::HashSet<String> = windows
            .iter()
            .map(|w| w.get("windowId").unwrap().as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            window_ids,
            ["@2".to_string(), "@3".to_string()].into_iter().collect()
        );
        assert_eq!(
            windows[0].get("liveMembers").unwrap().as_array().unwrap()[0]
                .get("name")
                .unwrap(),
            "worker"
        );
        assert!(dupes[0]
            .get("repair")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("manual"));
    }

    // --- registry-first load (tmux is display, not truth) ---

    /// A team with a registry entry and no tmux window is loadable: members
    /// come back pane-less with their recorded engine identity (was
    /// FileNotFoundError when tmux was the truth layer).
    #[test]
    fn test_team_load_registry_only_team_loads_without_any_window() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        let members = vec![obj(json!({
            "name": "worker", "cli": "grok", "model": "grok-4.6",
            "sessionId": "sid-g", "cwd": "/repo",
        }))];
        assert_eq!(
            crate::registry::record_team("ghostteam", "/tmp/ws-g", "111.0", &members, "").unwrap(),
            "written"
        );
        with_state(|st| {
            st.find_override = Some(Box::new(|_name, _prefer| {
                (String::new(), TeamWindowData::default())
            }));
        });

        let loaded = Team::load("ghostteam", "").unwrap();

        assert_eq!(loaded.workspace, "/tmp/ws-g");
        assert_eq!(loaded.created_at, 111.0);
        assert_eq!(loaded.tmux_window, "");
        let worker = loaded.agent_named("worker").unwrap();
        assert_eq!(worker.pane_id, "");
        assert_eq!(worker.cli, "grok");
        assert_eq!(worker.session_id.as_deref(), Some("sid-g"));
        assert_eq!(worker.cwd, "/repo");
    }

    #[test]
    fn test_team_load_unknown_everywhere_still_raises() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        with_state(|st| {
            st.find_override = Some(Box::new(|_name, _prefer| {
                (String::new(), TeamWindowData::default())
            }));
        });

        let err = Team::load("nosuchteam", "").unwrap_err();
        assert!(
            err.to_string().contains("Team 'nosuchteam' not found"),
            "{err}"
        );
    }

    /// Union semantics: a live pane binds onto its registry row (display wins
    /// for pane-derived fields, the recorded engine identity survives a wiped
    /// pane record), and a registry row without a pane stays as a member.
    #[test]
    fn test_team_load_pane_binds_onto_registry_roster() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        let members = vec![
            obj(json!({
                "name": "alive", "cli": "codex", "model": "m0",
                "sessionId": "sid-a", "cwd": "/old",
            })),
            obj(json!({
                "name": "headless", "cli": "claude", "sessionId": "sid-h", "cwd": "/repo",
            })),
        ];
        assert_eq!(
            crate::registry::record_team("team-u", "/tmp/ws-u", "5.0", &members, "").unwrap(),
            "written"
        );
        with_state(|st| {
            st.find_override = Some(Box::new(|_name, _prefer| {
                (
                    "dev:0".to_string(),
                    TeamWindowData {
                        window_id: "@0".to_string(),
                        created: "5.0".to_string(),
                        ..Default::default()
                    },
                )
            }));
            st.list_panes_full_fn = Some(Box::new(|_target| {
                let mut pane = pane_info("%1", "codex", "agent", "alive", "team-u");
                pane.cli = "codex".to_string();
                vec![pane]
            }));
            st.default_display_value = Some("/fresh".to_string());
        });

        let loaded = Team::load("team-u", "").unwrap();

        let names: std::collections::HashSet<String> =
            loaded.agents.iter().map(|a| a.name.clone()).collect();
        assert_eq!(
            names,
            ["alive".to_string(), "headless".to_string()]
                .into_iter()
                .collect()
        );
        let alive = loaded.agent_named("alive").unwrap();
        assert_eq!(alive.pane_id, "%1");
        assert_eq!(alive.cwd, "/fresh"); // live pane is fresher than the registry row
        assert_eq!(alive.session_id.as_deref(), Some("sid-a")); // wiped pane record falls back to registry
        assert_eq!(loaded.agent_named("headless").unwrap().pane_id, "");
    }

    #[test]
    fn test_list_teams_unions_registry_and_windows() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        assert_eq!(
            crate::registry::record_team("headlessteam", "/tmp/ws-h", "1.0", &[], "").unwrap(),
            "written"
        );
        with_state(|st| {
            st.run_fn = Some(run_stdout("dev:0\twindowed\t/tmp/ws-w\n"));
        });

        let teams: HashMap<String, Map<String, Value>> = list_teams()
            .unwrap()
            .into_iter()
            .map(|t| (t.get("name").unwrap().as_str().unwrap().to_string(), t))
            .collect();

        assert_eq!(teams["headlessteam"].get("tmuxWindow").unwrap(), "");
        assert_eq!(teams["headlessteam"].get("workspace").unwrap(), "/tmp/ws-h");
        assert_eq!(teams["windowed"].get("tmuxWindow").unwrap(), "dev:0");
        assert_eq!(teams["windowed"].get("workspace").unwrap(), "/tmp/ws-w");
    }

    /// The registry is the name authority: a headless team owns its name.
    #[test]
    fn test_create_refuses_a_registry_claimed_name() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        assert_eq!(
            crate::registry::record_team("team-h", "/tmp/ws", "1.0", &[], "").unwrap(),
            "written"
        );

        let err = Team::create_for_window("team-h", "dev:0", "", LEAD_AGENT_NAME, "", "", "", true)
            .unwrap_err();
        assert!(
            err.to_string().contains("already exists in the registry"),
            "{err}"
        );
    }

    #[test]
    fn test_team_status_payload_shape() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        with_state(|st| {
            st.default_command = String::new();
            st.pane_commands
                .insert("%0".to_string(), "python3.12".to_string());
            st.pane_commands
                .insert("%1".to_string(), "codex".to_string());
            st.pane_commands.insert("%2".to_string(), "zsh".to_string());
        });
        let mut team = Team {
            name: "team-a".to_string(),
            workspace: "/tmp/ws".to_string(),
            lead_pane_id: "%0".to_string(),
            lead_session_id: Some("sess-1".to_string()),
            tmux_session: "dev".to_string(),
            ..Default::default()
        };
        team.agents.push(new_agent(
            "claude", "team-a", "%1", "claude", "", "m1", None,
        ));

        let payload = team.status();

        assert_eq!(payload.get("tmuxSession").unwrap(), "dev");
        assert_eq!(payload.get("tmuxWindow").unwrap(), "");
        let members = payload.get("members").unwrap().as_array().unwrap();
        let orch = members
            .iter()
            .find(|m| m.get("name").unwrap() == "orch")
            .unwrap();
        let claude = members
            .iter()
            .find(|m| m.get("name").unwrap() == "claude")
            .unwrap();
        assert_eq!(orch.get("role").unwrap(), "terminal");
        assert_eq!(claude.get("role").unwrap(), "agent");
    }

    // tests/cli/test_message_commands.py::test_flow_is_not_a_team_name
    #[test]
    fn test_flow_is_not_a_team_name() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");

        assert!(validate_team_name("flow").contains("flow"));
    }
}
