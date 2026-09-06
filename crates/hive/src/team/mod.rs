//! Team: a registry roster with an optional tmux display window.
//!
//! `Team` itself (load, create, spawn, retire) lives here; `scope` resolves
//! which team a verb acts on and its workspace/hived, `roster` writes
//! membership (register, spawn onto the roster, the registry row), and
//! `delete` is the delete body. The cli and the flow engine reach them as
//! `team::*`.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

use crate::agent::Agent;
use crate::json_fields::is_set;
use crate::paths::getcwd;
use crate::tmux::PaneInfo;

#[cfg(test)]
use self::tests::fake_identity as identity;
#[cfg(test)]
use self::tests::fake_layout as layout;
#[cfg(test)]
use self::tests::fake_tmux as tmux;
#[cfg(not(test))]
use crate::identity;
#[cfg(not(test))]
use crate::layout;
#[cfg(not(test))]
use crate::tmux;

pub const LEAD_AGENT_NAME: &str = "orch";
const TMUX_REQUIRED_MESSAGE: &str = "Hive requires tmux. Start or attach to a tmux session first.";

/// The registry's instance key for a team created at *created_at*
/// (epoch seconds): an empty key for a team with no known creation time,
/// which every registry write treats as "no instance check".
pub fn created_at_key(created_at: f64) -> String {
    if created_at == 0.0 {
        String::new()
    } else {
        format!("{created_at}")
    }
}

fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
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

/// Drop every `@hive-*` window tag `write_window_options` (or an older
/// hive, which also wrote `@hive-peers`) left on *window*, with the display
/// carriers the hived and notify wrote on it, and the layout hooks with
/// their `@hive-layout` key: a window that stops being hive's keeps its
/// layout to itself.
pub fn clear_window_tags(window: &str) {
    layout::remove_hooks(window);
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

/// What Team.load reads back from `find_team_window` (window_id /
/// workspace / desc / created).
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
// calls reach is `crate::tmux`, answered in tests by `set_run_override`.

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

/// Team.load's cli resolution for a member pane: the pane tag, the
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
/// the `team/scope.rs` payload augmenter — goes
/// through this so an error never reads as "everyone offline".
pub(crate) fn usable_runtime(response: Option<Map<String, Value>>) -> Option<Map<String, Value>> {
    response
        .filter(|r| !r.is_empty())
        .filter(|r| r.get("ok") != Some(&Value::Bool(false)))
}

/// Test seam for callers outside this module: `find_team_window` resolves
/// tmux through `tests::fake_tmux` in test builds, so a command-layer test
/// that needs a window listing has to answer *this* double, not
/// `crate::tmux`'s.
#[cfg(test)]
pub(crate) fn set_fake_tmux_run(
    f: impl Fn(&[String], bool) -> Result<crate::tmux::Run, crate::tmux::TmuxError> + 'static,
) {
    tests::with_state(|st| st.run_fn = Some(Box::new(f)));
}

/// Sibling seam for `Team::anchor_pane`'s window listing, which reads
/// `list_panes_full` off the same fake.
#[cfg(test)]
pub(crate) fn set_fake_tmux_panes(f: impl Fn(&str) -> Vec<PaneInfo> + 'static) {
    tests::with_state(|st| st.list_panes_full_fn = Some(Box::new(f)));
}

#[derive(Debug, Clone)]
pub struct Team {
    pub name: String,
    pub description: String,
    pub workspace: String,
    pub lead_name: String,
    /// Insertion-ordered roster; spawn splits
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
    /// insertion position) or append.
    pub fn upsert_agent(&mut self, agent: Agent) {
        match self.agents.iter_mut().find(|a| a.name == agent.name) {
            Some(slot) => *slot = agent,
            None => self.agents.push(agent),
        }
    }

    // --- Window-level tmux options ---

    fn write_window_options(&self) {
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
        tmux::set_window_option(target, "@hive-created", &created_at_key(self.created_at));
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
        // create` outside tmux binds a team to a session it just created);
        // only the implicit "the window I am in" needs a client.
        if window_target.is_empty() && !identity::is_inside_tmux() {
            bail!("{}", TMUX_REQUIRED_MESSAGE);
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
            identity::current_pane_id().unwrap_or_default()
        };
        team.lead_session_id = detect_current_session_id(&team.lead_pane_id);
        team.tmux_session = if window_target.contains(':') {
            window_target.split(':').next().unwrap_or("").to_string()
        } else {
            identity::current_session_name().unwrap_or_default()
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

        team.write_window_options();
        Ok(team)
    }

    /// Create a new team in the currently-focused tmux window.
    pub fn create(name: &str, description: &str, workspace: &str) -> Result<Team> {
        if !identity::is_inside_tmux() {
            bail!("{}", TMUX_REQUIRED_MESSAGE);
        }
        Team::create_for_window(
            name,
            &identity::current_window_target().unwrap_or_default(),
            &identity::current_pane_id().unwrap_or_default(),
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
            identity::current_pane_id().unwrap_or_default()
        };
        let (window_target, window_data) = find_team_window(name, &hint)?;
        if snap.is_none() && window_target.is_empty() {
            bail!("Team '{name}' not found");
        }

        let snap_workspace = snap
            .as_ref()
            .map(|s| row_str(s, "workspace"))
            .unwrap_or_default();
        let snap_created = match snap.as_ref() {
            Some(s) if is_set(s.get("createdAt")) => match s.get("createdAt") {
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
        // one `hive workflow run` process per node). The claim is a paneless
        // placeholder row — replaced by the real row after the spawn, removed
        // if the spawn fails.
        let claimed = self.claim_name(name, cli, model)?;

        let window_for_split = self.display_window();
        let split_horizontal =
            self.agents.is_empty() && layout::split_horizontal(&window_for_split);
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
            let _ = layout::ensure(&window_for_split, false);
        }

        Ok(agent)
    }

    /// The team's display window: its own bound window, or — for a team
    /// without one — the caller's focused window.
    fn display_window(&self) -> String {
        if !self.tmux_window.is_empty() {
            self.tmux_window.clone()
        } else {
            identity::current_window_target().unwrap_or_default()
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
        identity::current_pane_id().unwrap_or_default()
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
    /// `hive kill`, `hive delete --down`, and a failed workflow start all come here.
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
        if !self.workspace.is_empty() {
            crate::workflow::remove_record(&self.workspace, name);
        }
        let window = self.display_window();
        if !window.is_empty() {
            let _ = layout::ensure(&window, false);
        }
        true
    }

    pub fn created_at_key(&self) -> String {
        created_at_key(self.created_at)
    }

    /// Reserve `name` in the registry roster (paneless placeholder). Ok(true)
    /// when this call made the claim; Err when another process holds the
    /// name. Ok(false) means no claim was made and the spawn goes on guarded
    /// only by the in-memory check: the team has no registry entry (or a
    /// recycled successor's), or the store lock/write failed — best-effort,
    /// like the spawn path's other registry writes (`registry_record_member`
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
    /// Kill every engine on the roster but `keep` (the member running this
    /// very delete, whose engine hosts the process), then clear the lead
    /// pane's tags.
    pub fn cleanup(&self, keep: Option<&str>) {
        for agent in &self.agents {
            if Some(agent.name.as_str()) != keep {
                agent.kill();
            }
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
pub(crate) fn window_has_live_team_members(window_target: &str, team_name: &str) -> bool {
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
pub(crate) fn find_team_window(name: &str, prefer_pane: &str) -> Result<(String, TeamWindowData)> {
    let fmt = format!(
        "#{{session_name}}:#{{window_index}}\t#{{window_id}}\t{}\t#{{@hive-workspace}}\t#{{@hive-desc}}\t#{{@hive-created}}",
        crate::tmux::WINDOW_TEAM_FMT
    );
    let r = tmux::run(&["list-windows", "-a", "-F", &fmt], false, 5)?;

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
                    gc_stale_team_windows(name, wt, &all_windows);
                    return Ok((wt.clone(), data.clone()));
                }
            }
        }
    }

    // 2) Prefer the window that has panes actually tagged for this team.
    for (wt, data) in &candidates {
        if window_has_live_team_members(wt, name) {
            gc_stale_team_windows(name, wt, &all_windows);
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
pub(crate) fn gc_stale_team_windows(name: &str, keep: &str, all_windows: &[String]) {
    for wt in all_windows {
        if wt == keep {
            continue;
        }
        if window_has_live_team_members(wt, name) {
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
        crate::tmux::WINDOW_TEAM_FMT
    );
    let r = tmux::run(&["list-windows", "-a", "-F", &fmt], false, 5)?;

    // serde_json's preserve_order Map keeps team insertion order.
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
        if is_set(entry.get("corrupt")) || team.is_empty() {
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
        crate::tmux::WINDOW_TEAM_FMT
    );
    let r = tmux::run(&["list-windows", "-a", "-F", &fmt], false, 5)?;
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

mod delete;
mod roster;
mod scope;

pub(crate) use delete::delete_team;
pub(crate) use roster::{
    gc_dead_teams, live_member_pids, member_registry_row, register_agent_member,
    session_member_row, sorted_member_rows, spawn_team_agent,
};
pub(crate) use scope::{
    add_runtime_location_fields, ensure_pane_in_scope, ensure_team_hived, load_team,
    remember_context, resolve_scoped_team, resolve_workspace, start_team_hived,
    start_team_hived_or_warn, team_status_payload,
};

#[cfg(test)]
mod tests;
