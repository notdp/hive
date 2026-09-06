//! Team: a registry roster with an optional tmux display window.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

use crate::agent::Agent;
use crate::pyval::{py_float_str, truthy};
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
        cwd: cwd.to_string(),
        session_id,
        cli: cli.to_string(),
    }
}

/// Drop every `@hive-*` window tag `_write_window_options` (or a Python-era
/// hive, which also wrote `@hive-peers`) left on *window*, with the display
/// carriers the hived and notify wrote on it.
pub fn clear_window_tags(window: &str) {
    for key in [
        "hive-team",
        "hive-workspace",
        "hive-desc",
        "hive-created",
        "hive-peers",
        "hive-built",
        "hive-mirror",
        "hive-hidden",
        "hive-ticker",
        "hive-notify-token",
        "hive-notify-hook",
        "hive-notify-text",
    ] {
        tmux::clear_window_option(window, &format!("@{key}"));
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
    split_horizontal: bool,
    split_size: Option<String>,
    skill: String,
    extra_env: Option<HashMap<String, String>>,
    cli: String,
}

// --- cross-module seams: each wrapper runs the real call unless the test
// installed an answer for that seam (`tests::Hook`); the tmux those real
// calls reach is `crate::tmux`, answered in tests by `_set_run_override`.

fn agent_spawn(call: SpawnCall) -> Result<Agent> {
    #[cfg(test)]
    if let Some(answer) = tests::hook(|h| h.spawn(&call)).flatten() {
        return answer;
    }
    Agent::spawn(
        &call.name,
        &call.team_name,
        &call.target_pane,
        crate::agent::SpawnOptions {
            model: call.model.clone(),
            prompt: call.prompt.clone(),
            cwd: call.cwd.clone(),
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

fn detect_current_session_id(pane_id: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(id) = tests::hook(|h| h.session_id.clone()).flatten() {
        return Some(id);
    }
    crate::agent::detect_current_session_id(pane_id)
}

/// Name of the CLI profile live on *pane_id* (`agent_cli`'s process and
/// title probe), None when nothing recognizable runs there.
fn detect_profile_name_for_pane(pane_id: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(answer) = tests::hook(|h| h.profile_for_pane.as_ref().map(|f| f(pane_id))).flatten()
    {
        return answer;
    }
    crate::agent_cli::detect_profile_for_pane(pane_id).map(|profile| profile.name.to_string())
}

/// Python Team.load's cli resolution for a member pane: the pane tag, the
/// pane command, then live profile detection, then the "claude" default.
fn resolve_member_cli(pane: &PaneInfo) -> String {
    let resolved = if !pane.cli.is_empty() {
        pane.cli.clone()
    } else {
        crate::agent_cli::normalize_command(&pane.command)
    };
    if crate::agent_cli::AGENT_CLI_NAMES.contains(&resolved.as_str()) {
        return resolved;
    }
    detect_profile_name_for_pane(&pane.pane_id).unwrap_or_else(|| "claude".to_string())
}

fn request_team_runtime(workspace: &str, team_name: &str) -> Option<Map<String, Value>> {
    #[cfg(test)]
    if let Some(answer) =
        tests::hook(|h| h.team_runtime.as_ref().map(|f| f(workspace, team_name))).flatten()
    {
        return answer;
    }
    crate::hived::request_team_runtime(workspace, team_name)
}

/// The hived's team-runtime answer, or None when there is nothing to trust:
/// no socket, an empty body, or an `{ok: false, error}` envelope (the hived
/// failed to load the team — that says nothing about the members). Every
/// reader that turns the answer into per-member state — `Team::member_alive`,
/// the flow board's roster, the `cli/util.rs` payload augmenter — goes
/// through this so an error never reads as "everyone offline".
pub(crate) fn usable_runtime(response: Option<Map<String, Value>>) -> Option<Map<String, Value>> {
    response
        .filter(|r| !r.is_empty())
        .filter(|r| r.get("ok") != Some(&Value::Bool(false)))
}

/// Test seam for callers outside this module: `_find_team_window` resolves
/// tmux through `tests::fake_tmux` in test builds, so a command-layer test
/// that needs a window listing has to answer *this* double, not
/// `crate::tmux`'s.
#[cfg(test)]
pub(crate) fn _set_fake_tmux_run(
    f: impl Fn(&[String], bool) -> Result<crate::tmux::Run, crate::tmux::TmuxError> + 'static,
) {
    tests::with_state(|st| st.run_fn = Some(Box::new(f)));
}

/// Sibling seam for `Team::anchor_pane`'s window listing, which reads
/// `list_panes_full` off the same fake.
#[cfg(test)]
pub(crate) fn _set_fake_tmux_panes(f: impl Fn(&str) -> Vec<PaneInfo> + 'static) {
    tests::with_state(|st| st.list_panes_full_fn = Some(Box::new(f)));
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
        workspace: &str,
        tag_lead: bool,
    ) -> Result<Team> {
        // An explicit window target is addressable from anywhere (`hive
        // flow rig` binds a team to a session it just created, from outside
        // tmux); only the implicit "the window I am in" needs a client.
        if window_target.is_empty() && !tmux::is_inside_tmux() {
            bail!("{}", _TMUX_REQUIRED_MESSAGE);
        }
        let error = validate_team_name(name);
        if !error.is_empty() {
            bail!("{}", error);
        }
        if crate::registry::load(name).is_some() {
            // The registry is the name authority: a team whose window is
            // gone owns its name (its engines may still be running) until
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
        team.lead_session_id = detect_current_session_id(&team.lead_pane_id);
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
                crate::agent_cli::member_role_for_pane(&team.lead_pane_id),
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
    pub fn create(name: &str, description: &str, workspace: &str) -> Result<Team> {
        if !tmux::is_inside_tmux() {
            bail!("{}", _TMUX_REQUIRED_MESSAGE);
        }
        Team::create_for_window(
            name,
            &tmux::get_current_window_target().unwrap_or_default(),
            &tmux::get_current_pane_id().unwrap_or_default(),
            LEAD_AGENT_NAME,
            description,
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
        let (window_target, window_data) = _find_team_window(name, &hint)?;
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
            if pane.is_member_pane() {
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

    pub fn lead_agent(&self) -> Option<Agent> {
        if self.lead_pane_id.is_empty() {
            return None;
        }
        Some(Agent {
            name: self.lead_name.clone(),
            team_name: self.name.clone(),
            pane_id: self.lead_pane_id.clone(),
            model: String::new(),
            cwd: tmux::display_value(&self.lead_pane_id, "#{pane_current_path}")
                .unwrap_or_else(getcwd),
            session_id: self.lead_session_id.clone(),
            cli: tmux::get_pane_option(&self.lead_pane_id, "hive-cli").unwrap_or_default(),
        })
    }

    // --- Agent management ---

    /// Spawn a new agent in the team.
    #[allow(clippy::too_many_arguments)]
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
        // Addressability is the gate, not $TMUX: a spawn lands on an anchor
        // pane resolved from registry-known state, and targeted tmux
        // commands need no client context. No anchor means no display to
        // land on — that is the only reason to refuse.
        let target = self.anchor_pane();
        if target.is_empty() {
            bail!(
                "team '{}' has no pane to split from (no live member pane, lead pane, display window, or current pane)",
                self.name
            );
        }
        // Cross-process name claim under the registry lock: the in-memory
        // check above cannot see a concurrent spawner (a workflow fanning out
        // one `hive flow node run` process per node). The claim is a paneless
        // placeholder row — replaced by the real row after the spawn, removed
        // if the spawn fails.
        let claimed = self.claim_name(name, cli, model)?;

        let window_for_split = self.display_window();
        let split_horizontal =
            self.agents.is_empty() && layout::split_horizontal(&window_for_split, 2);
        let split_size = "50%";

        let spawned = agent_spawn(SpawnCall {
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
            split_horizontal,
            split_size: Some(split_size.to_string()),
            skill: skill.to_string(),
            extra_env: extra_env.cloned(),
            cli: cli.to_string(),
        });
        let agent = match spawned {
            Ok(agent) => agent,
            Err(err) => {
                if claimed {
                    let _ =
                        crate::registry::remove_member(&self.name, name, &self.created_at_key());
                }
                return Err(err);
            }
        };

        tmux::tag_pane(&agent.pane_id, "agent", name, &self.name, cli, "");
        self.upsert_agent(agent.clone());
        if !window_for_split.is_empty() {
            tmux::configure_hive_window(&window_for_split);
            let _ = layout::apply_adaptive(&window_for_split);
        }

        Ok(agent)
    }

    /// The team's display window: its own bound window, or — for a team
    /// without one — the caller's focused window.
    fn display_window(&self) -> String {
        if !self.tmux_window.is_empty() {
            self.tmux_window.clone()
        } else {
            tmux::get_current_window_target().unwrap_or_default()
        }
    }

    /// Where the next spawn splits from — the one answer to "is this team
    /// addressable right now". In order: the last member that still has a
    /// live pane, the lead pane, the display window's own first pane (a
    /// registry-loaded team carries no lead pane, and roster rows without a
    /// live pane are the norm under `record_member`'s retain+push), and
    /// finally the caller's current pane. Empty means nothing to land on.
    pub fn anchor_pane(&self) -> String {
        if let Some(a) = self.agents.iter().rev().find(|a| !a.pane_id.is_empty()) {
            return a.pane_id.clone();
        }
        if !self.lead_pane_id.is_empty() {
            return self.lead_pane_id.clone();
        }
        if !self.tmux_window.is_empty() {
            if let Some(p) = tmux::list_panes_full(&self.tmux_window).first() {
                return p.pane_id.clone();
            }
        }
        tmux::get_current_pane_id().unwrap_or_default()
    }

    /// Whether a member can still receive a dispatch and answer. The hived's
    /// team runtime is the authority (`cliAlive` — an engine that is gone
    /// reads offline even if a pane still shows its last screen); without a
    /// usable hived answer (`usable_runtime`), a bound live pane stands in.
    pub fn member_alive(&self, name: &str) -> bool {
        let Some(agent) = self.agent_named(name) else {
            return false;
        };
        if !self.workspace.is_empty() {
            if let Some(runtime) = usable_runtime(request_team_runtime(&self.workspace, &self.name))
            {
                if let Some(member) = runtime
                    .get("members")
                    .and_then(Value::as_object)
                    .and_then(|m| m.get(name))
                    .and_then(Value::as_object)
                {
                    return member
                        .get("cliAlive")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                }
                return false;
            }
        }
        !agent.pane_id.is_empty()
    }

    /// Retire a member: kill its engine/pane, drop the roster row here and
    /// in the registry, re-tile the display. The one retirement path —
    /// `hive kill`, flow's `kill()`, and a failed node start all come here.
    /// Returns whether the member was on the roster.
    pub fn retire(&mut self, name: &str) -> bool {
        let Some(pos) = self.agents.iter().position(|a| a.name == name) else {
            return false;
        };
        self.agents[pos].kill();
        self.agents.remove(pos);
        if !self.name.is_empty() {
            let _ = crate::registry::remove_member(&self.name, name, &self.created_at_key());
        }
        let window = self.display_window();
        if !window.is_empty() {
            let _ = layout::apply_adaptive(&window);
        }
        true
    }

    pub fn created_at_key(&self) -> String {
        if self.created_at == 0.0 {
            String::new()
        } else {
            py_float_str(self.created_at)
        }
    }

    /// Reserve `name` in the registry roster (paneless placeholder). Ok(true)
    /// when this call made the claim; Err when another process holds the
    /// name. Ok(false) means no claim was made and the spawn goes on guarded
    /// only by the in-memory check: the team has no registry entry (or a
    /// recycled successor's), or the store lock/write failed — best-effort,
    /// like the spawn path's other registry writes (`_registry_record_member`
    /// warns, the rollback `remove_member` is discarded).
    fn claim_name(&self, name: &str, cli: &str, model: &str) -> Result<bool> {
        if self.name.is_empty() {
            return Ok(false);
        }
        let claim: Map<String, Value> = [("name", name), ("cli", cli), ("model", model)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect();
        match crate::registry::reserve_member(&self.name, &claim, &self.created_at_key()) {
            Ok("exists") => bail!("Agent '{name}' already exists in team '{}'", self.name),
            Ok(verdict) => Ok(verdict == "reserved"),
            Err(_) => Ok(false),
        }
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
                Value::String(crate::agent_cli::member_role_for_pane(&lead.pane_id).to_string()),
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
            agent.kill();
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
    let fmt = format!(
        "#{{session_name}}:#{{window_index}}\t#{{window_id}}\t{}\t#{{@hive-workspace}}\t#{{@hive-desc}}\t#{{@hive-created}}",
        crate::tmux::_WINDOW_TEAM_FMT
    );
    let r = tmux::_run(&["list-windows", "-a", "-F", &fmt], false, 5)?;

    let mut candidates: Vec<(String, TeamWindowData)> = Vec::new();
    for line in r.stdout.trim().split('\n') {
        if line.is_empty() {
            continue;
        }
        let mut parts: Vec<String> = line.split('\t').map(str::to_string).collect();
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
        clear_window_tags(wt);
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
    let fmt = format!(
        "#{{session_name}}:#{{window_index}}\t#{{window_id}}\t{}\t#{{@hive-workspace}}",
        crate::tmux::_WINDOW_TEAM_FMT
    );
    let r = tmux::_run(&["list-windows", "-a", "-F", &fmt], false, 5)?;

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

    let fmt = format!(
        "#{{session_name}}:#{{window_index}}\t{}\t#{{@hive-workspace}}",
        crate::tmux::_WINDOW_TEAM_FMT
    );
    let r = tmux::_run(&["list-windows", "-a", "-F", &fmt], false, 5)?;
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
    use crate::testenv::EnvGuard;
    use crate::tmux::{Run, TmuxError};
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    // ------------------------------------------------------------------
    // Seam hook: what a test pins on this module's cross-module calls. An
    // unset field runs the real call (`configure_hive_home` installs an
    // empty hook, and points the real tmux facade at a no-server double).
    // ------------------------------------------------------------------

    type SpawnFn = Box<dyn Fn(usize, &SpawnCall) -> Agent>;
    type TeamRuntimeFn = Box<dyn Fn(&str, &str) -> Option<Map<String, Value>>>;
    type ProfileFn = Box<dyn Fn(&str) -> Option<String>>;

    #[derive(Default)]
    pub struct Hook {
        pub spawn_calls: Vec<SpawnCall>,
        pub spawn_fn: Option<SpawnFn>,
        pub session_id: Option<String>,
        pub team_runtime: Option<TeamRuntimeFn>,
        pub profile_for_pane: Option<ProfileFn>,
    }

    impl Hook {
        /// Record the spawn; answer it (1-based call number, the call) when
        /// `spawn_fn` is set, else let `Agent::spawn` run.
        pub fn spawn(&mut self, call: &SpawnCall) -> Option<Result<Agent>> {
            self.spawn_calls.push(call.clone());
            let n = self.spawn_calls.len();
            self.spawn_fn.as_ref().map(|f| Ok(f(n, call)))
        }
    }

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    pub fn hook<T>(f: impl FnOnce(&mut Hook) -> T) -> Option<T> {
        HOOK.with(|cell| cell.borrow_mut().as_mut().map(f))
    }

    // ------------------------------------------------------------------
    // Fake state shared by fake_tmux / fake_layout; `configure_hive_home`
    // resets it per test.
    // ------------------------------------------------------------------

    pub struct FakeState {
        pub tmux_inside: bool,
        pub current_pane: String,
        pub session_name: String,
        pub current_window_target: Option<String>,
        /// Ordered so the rendered `list-windows` output is deterministic.
        pub window_options: BTreeMap<String, HashMap<String, String>>,
        pub pane_options: HashMap<String, HashMap<String, String>>,
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
    }

    impl Default for FakeState {
        fn default() -> Self {
            FakeState {
                tmux_inside: true,
                current_pane: "%0".to_string(),
                session_name: "dev".to_string(),
                current_window_target: Some("dev:0".to_string()),
                window_options: BTreeMap::new(),
                pane_options: HashMap::new(),
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

    /// What `list-windows -a -F <fmt>` prints for the fake window options:
    /// `#{session_name}:#{window_index}` is the target, `#{window_id}` its
    /// derived id, `#{@key}` the option — so `_find_team_window` reads back
    /// what `_write_window_options` wrote. Any other `_run` prints nothing.
    fn fake_run_stdout(st: &FakeState, args: &[String]) -> String {
        if args.first().map(String::as_str) != Some("list-windows") {
            return String::new();
        }
        let Some(fmt) = args
            .iter()
            .position(|a| a == "-F")
            .and_then(|i| args.get(i + 1))
        else {
            return String::new();
        };
        let mut out = String::new();
        for (target, opts) in &st.window_options {
            // The hidden-window mask, as tmux would resolve it.
            let team_tag = if opts.get("hive-hidden").is_some_and(|h| !h.is_empty()) {
                ""
            } else {
                "#{@hive-team}"
            };
            let mut line = fmt
                .replace(crate::tmux::_WINDOW_TEAM_FMT, team_tag)
                .replace("#{session_name}:#{window_index}", target)
                .replace("#{window_id}", &window_id_for_target(target));
            while let Some(start) = line.find("#{@") {
                let Some(len) = line[start..].find('}') else {
                    break;
                };
                let key = line[start + 3..start + len].to_string();
                let value = opts.get(&key).cloned().unwrap_or_default();
                line.replace_range(start..start + len + 1, &value);
            }
            out.push_str(&line);
            out.push('\n');
        }
        out
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
                    if opts.get("hive-team").is_some_and(|t| !t.is_empty()) {
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
                    stdout: super::fake_run_stdout(st, &owned),
                    stderr: String::new(),
                }),
            })
        }
    }

    /// Fake layout runs the REAL `layout::pick` over fake tmux state, so
    /// only the tmux calls are faked, not the preset choice.
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

    // ------------------------------------------------------------------
    // Test helpers
    // ------------------------------------------------------------------

    /// Isolate every engine home under a temp dir, reset the fake tmux
    /// state, install an empty seam hook, and make every `crate::tmux::_run`
    /// the seams' real calls reach fail as it would with no server — holding
    /// the env lock for the test's lifetime.
    fn configure_hive_home(tmux_inside: bool, current_pane: &str) -> (tempfile::TempDir, EnvGuard) {
        let mut env = EnvGuard::cleared(&[
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CODEX_THREAD_ID",
            "GROK_SESSION_ID",
        ]);
        let tmp = tempfile::tempdir().unwrap();
        env.set("HIVE_HOME", tmp.path().join(".hive"));
        env.set("CODEX_HOME", tmp.path().join(".codex"));
        env.set("CLAUDE_HOME", tmp.path().join(".claude"));
        env.set("GROK_HOME", tmp.path().join(".grok"));
        env.set("XDG_CACHE_HOME", tmp.path().join(".cache"));
        env.set("CLAUDE_CONFIG_DIR", tmp.path().join("claude-env-isolation"));
        with_state(|st| {
            *st = FakeState::default();
            st.tmux_inside = tmux_inside;
            st.current_pane = current_pane.to_string();
        });
        HOOK.with(|cell| *cell.borrow_mut() = Some(Hook::default()));
        real_tmux_pane_commands(&[]);
        (tmp, env)
    }

    fn no_tmux_server() -> TmuxError {
        TmuxError::Os("no tmux server in unit tests".to_string())
    }

    /// Answer the real tmux facade's `display-message -t <pane> -p
    /// #{pane_current_command}` from *commands* (the read
    /// `agent_cli::member_role_for_pane` probes first); every other real
    /// tmux call fails as it would with no server.
    fn real_tmux_pane_commands(commands: &[(&str, &str)]) {
        let commands: HashMap<String, String> = commands
            .iter()
            .map(|(pane, command)| (pane.to_string(), command.to_string()))
            .collect();
        crate::tmux::_set_run_override(move |args, _check, _timeout| {
            let pane = args
                .iter()
                .position(|a| a == "-t")
                .and_then(|i| args.get(i + 1));
            let command = match (args.first().map(String::as_str), args.last()) {
                (Some("display-message"), Some(fmt)) if fmt == "#{pane_current_command}" => {
                    pane.and_then(|p| commands.get(p))
                }
                _ => None,
            };
            match command {
                Some(command) => Ok(Run {
                    returncode: 0,
                    stdout: format!("{command}\n"),
                    stderr: String::new(),
                }),
                None => Err(no_tmux_server()),
            }
        });
    }

    fn set_hive_window(target: &str, team: &str, workspace: &str, desc: &str, created: &str) {
        for (key, value) in [
            ("@hive-team", team),
            ("@hive-workspace", workspace),
            ("@hive-desc", desc),
            ("@hive-created", created),
        ] {
            fake_tmux::set_window_option(target, key, value);
        }
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
        hook(|h| h.session_id = Some("sess-123".to_string()));
        real_tmux_pane_commands(&[("%7", "claude")]);

        let team = Team::create("team-a", "demo", "/tmp/ws").unwrap();

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

        let err = Team::create("team-a", "", "").unwrap_err();
        assert!(err.to_string().contains("requires tmux"), "{err}");
    }

    #[test]
    fn test_team_create_rejects_reserved_or_dotted_names() {
        // `hive send` parses `<team>.<member>` / `ccd.<session>`: a team named
        // ccd, or one carrying a dot, would be unaddressable
        let (_tmp, _guard) = configure_hive_home(true, "%0");

        for name in ["ccd", "ccd.desk", "a.b"] {
            let err = Team::create(name, "", "").unwrap_err();
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

        team._write_window_options();
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
        set_hive_window("dev:0", "team-a", "/tmp/ws", "", "0");
        with_state(|st| {
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

    /// The mirror is the member's pane as much as an engine pane is: a verb
    /// addressing the orch (kill, capture, inject) lands on it.
    #[test]
    fn test_team_load_binds_a_member_to_its_mirror_pane() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        set_hive_window("dev:0", "team-a", "/tmp/ws", "", "0");
        with_state(|st| {
            st.list_panes_full_fn = Some(Box::new(|_target| {
                let mut pane = pane_info("%1", "hive", "mirror", "orch", "team-a");
                pane.cli = "claude".to_string();
                vec![pane]
            }));
        });

        let loaded = Team::load("team-a", "").unwrap();

        assert_eq!(loaded.get("orch").unwrap().pane_id, "%1");
        assert_eq!(loaded.agent_named("orch").unwrap().cli, "claude");
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
        hook(move |h| h.spawn_fn = Some(Box::new(move |_n, _call| spawn_result.clone())));
        with_state(|st| {
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
            .spawn(
                "claude",
                "",
                "start now",
                "",
                "demo-review",
                Some(&HashMap::from([("FOO".to_string(), "bar".to_string())])),
                "claude",
            )
            .unwrap();

        assert_eq!(result.pane_id, agent.pane_id);
        assert_eq!(result.name, agent.name);
        let calls = hook(|h| h.spawn_calls.clone()).unwrap();
        assert_eq!(calls[0].target_pane, "%0");
        assert_eq!(calls[0].skill, "demo-review");
        assert_eq!(calls[0].prompt, "start now");
        assert_eq!(calls[0].split_size.as_deref(), Some("50%"));
        assert_eq!(
            calls[0].extra_env.as_ref().and_then(|env| env.get("FOO")),
            Some(&"bar".to_string())
        );
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
        hook(move |h| h.spawn_fn = Some(Box::new(move |_n, _call| agent.clone())));
        with_state(|st| {
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
        let calls = hook(|h| h.spawn_calls.clone()).unwrap();
        assert!(!calls[0].split_horizontal);
    }

    #[test]
    fn test_team_spawn_second_agent_splits_from_last_agent() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        hook(|h| {
            h.spawn_fn = Some(Box::new(|n, call| {
                super::new_agent(
                    &call.name,
                    "team-a",
                    &format!("%{}", n + 8),
                    "claude",
                    "",
                    "",
                    None,
                )
            }))
        });
        with_state(|st| st.current_window_target = None);

        let mut team = Team {
            name: "team-a".to_string(),
            lead_pane_id: "%0".to_string(),
            ..Default::default()
        };
        team.agents
            .push(new_agent("claude", "team-a", "%9", "claude", "", "", None));
        team.spawn("gpt", "", "", "", "hive", None, "claude")
            .unwrap();

        let calls = hook(|h| h.spawn_calls.clone()).unwrap();
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
                "dev:2\t@2\tmy-team\t/tmp/ws\tdesc\t0\ndev:3\t@3\tmy-team\t/tmp/ws\tdesc\t0\n",
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
                "dev:2\t@2\tmy-team\t/tmp/ws\tdesc\t0\ndev:3\t@3\tmy-team\t/tmp/ws\tdesc\t0\n",
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
                "dev:2\t@2\t0-2\t/tmp/ws2\tdesc\t0\ndev:3\t@3\t0-2\t/tmp/ws3\tdesc\t0\n",
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
        set_hive_window("dev:0", "team-u", "", "", "5.0");
        with_state(|st| {
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

    /// A parked mirror's hidden window answers `@hive-team` through its
    /// pane; every team-window scan masks it.
    #[test]
    fn test_window_scans_mask_the_hidden_mirror_window() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        with_state(|st| {
            st.window_options.insert(
                "dev:1".to_string(),
                HashMap::from([
                    ("hive-team".to_string(), "honey".to_string()),
                    ("hive-workspace".to_string(), "/tmp/ws".to_string()),
                ]),
            );
            st.window_options.insert(
                "honey:9".to_string(),
                HashMap::from([
                    ("hive-hidden".to_string(), "honey".to_string()),
                    ("hive-team".to_string(), "honey".to_string()),
                ]),
            );
            st.list_panes_full_fn = Some(Box::new(|target| match target {
                "dev:1" => vec![pane_info("%2", "grok", "agent", "sage", "honey")],
                "honey:9" => vec![pane_info("%1", "hive", "mirror", "orch", "honey")],
                _ => Vec::new(),
            }));
        });

        let teams = list_teams().unwrap();
        assert_eq!(teams.len(), 1);
        assert_eq!(teams[0].get("tmuxWindow").unwrap(), "dev:1");
        assert!(duplicate_team_bindings().unwrap().is_empty());
        let (wt, _) = _find_team_window("honey", "").unwrap();
        assert_eq!(wt, "dev:1");
        assert!(with_state(|st| st.cleared.is_empty()));
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

    /// The registry is the name authority: a windowless team owns its name.
    #[test]
    fn test_create_refuses_a_registry_claimed_name() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        assert_eq!(
            crate::registry::record_team("team-h", "/tmp/ws", "1.0", &[], "").unwrap(),
            "written"
        );

        let err = Team::create_for_window("team-h", "dev:0", "", LEAD_AGENT_NAME, "", "", true)
            .unwrap_err();
        assert!(
            err.to_string().contains("already exists in the registry"),
            "{err}"
        );
    }

    #[test]
    fn test_team_status_payload_shape() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        real_tmux_pane_commands(&[("%0", "python3.12")]);
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

    fn team_with_pane_member(pane_id: &str) -> Team {
        let mut team = Team {
            name: "team-a".to_string(),
            workspace: "/ws".to_string(),
            ..Default::default()
        };
        team.agents.push(new_agent(
            "worker", "team-a", pane_id, "claude", "", "", None,
        ));
        team
    }

    fn stub_team_runtime(response: Value) {
        hook(|h| h.team_runtime = Some(Box::new(move |_, _| response.as_object().cloned())));
    }

    #[test]
    fn test_usable_runtime_rejects_error_envelope_and_empty_body() {
        assert!(usable_runtime(None).is_none());
        assert!(usable_runtime(Some(Map::new())).is_none());
        let err = json!({"ok": false, "error": "team not found"});
        assert!(usable_runtime(err.as_object().cloned()).is_none());
        let ok = json!({"ok": true, "members": {}});
        assert_eq!(
            usable_runtime(ok.as_object().cloned()),
            ok.as_object().cloned()
        );
    }

    #[test]
    fn test_member_alive_hived_error_falls_back_to_pane_liveness() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        stub_team_runtime(json!({"ok": false, "error": "load failed"}));

        assert!(team_with_pane_member("%1").member_alive("worker"));
        assert!(!team_with_pane_member("").member_alive("worker"));
    }

    #[test]
    fn test_member_alive_hived_answer_is_authoritative() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        let team = team_with_pane_member("%1");

        stub_team_runtime(json!({"ok": true, "members": {"worker": {"cliAlive": false}}}));
        assert!(!team.member_alive("worker"));

        stub_team_runtime(json!({"ok": true, "members": {"worker": {"cliAlive": true}}}));
        assert!(team.member_alive("worker"));

        stub_team_runtime(json!({"ok": true, "members": {}}));
        assert!(!team.member_alive("worker"));
    }

    #[test]
    fn test_member_alive_no_hived_uses_pane_liveness() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        // No hived answers: pin the seam instead of probing a real socket path.
        hook(|h| h.team_runtime = Some(Box::new(|_, _| None)));

        assert!(team_with_pane_member("%1").member_alive("worker"));
        assert!(!team_with_pane_member("").member_alive("worker"));
    }

    /// The real `resolve_member_cli` ladder: the pane tag answers first, then
    /// the pane command (normalized), then the live profile probe, and
    /// "claude" when the probe finds nothing. The probe runs only for the
    /// last two rungs.
    #[test]
    fn test_resolve_member_cli_ladder_tag_then_command_then_probe_then_claude() {
        let (_tmp, _guard) = configure_hive_home(true, "%0");
        let probed: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::clone(&probed);
        hook(|h| {
            h.profile_for_pane = Some(Box::new(move |pane_id| {
                seen.borrow_mut().push(pane_id.to_string());
                (pane_id == "%3").then(|| "grok".to_string())
            }))
        });

        let mut tagged = pane_info("%1", "node", "agent", "a", "team-a");
        tagged.cli = "codex".to_string();
        let by_command = pane_info("%2", "/usr/local/bin/codex", "agent", "b", "team-a");
        let by_probe = pane_info("%3", "node", "agent", "c", "team-a");
        let unknown = pane_info("%4", "zsh", "agent", "d", "team-a");

        assert_eq!(resolve_member_cli(&tagged), "codex");
        assert_eq!(resolve_member_cli(&by_command), "codex");
        assert_eq!(resolve_member_cli(&by_probe), "grok");
        assert_eq!(resolve_member_cli(&unknown), "claude");
        assert_eq!(*probed.borrow(), vec!["%3".to_string(), "%4".to_string()]);
    }
}
