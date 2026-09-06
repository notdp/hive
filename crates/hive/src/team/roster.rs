//! Roster membership writes: registering a pane or a session as a member,
//! spawning a member engine onto the roster, the registry row for a member,
//! and the roster reads (`sorted_member_rows`, live member pids) beside them.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};

use super::{remember_context, resolve_workspace, Team, LEAD_AGENT_NAME};
use crate::agent::Agent;
use crate::json_fields::map_str;
use crate::paths::getcwd;
use crate::tmux;

/// Clear a stale current context naming a team that no longer exists.
///
/// "Known" is the registry union with live windows, so a team whose window
/// is gone is never treated as dead. On a failed team listing nothing is
/// touched (conservative: cannot prove any team dead). Team directories are
/// never swept here: one without a `team.json` is a workspace `hive delete`
/// deliberately kept.
pub(crate) fn gc_dead_teams() {
    let live_names: HashSet<String> = match crate::team::list_teams() {
        Ok(teams) => teams.iter().map(|t| map_str(t, "name")).collect(),
        Err(_) => return,
    };
    let ctx = crate::context::load_current_context();
    if let Some(team) = ctx.get("team").filter(|t| !t.is_empty()) {
        if !live_names.contains(team) {
            let _ = crate::context::clear_current_context();
        }
    }
}

fn hive_join_message(agent_name: &str, team_name: &str) -> String {
    format!(
        "You are '{agent_name}' in hive team '{team_name}'. \
         Context is pre-bound. Run `/hive:hive {team_name}` first and follow \
         that protocol. Hive messages will arrive inline as \
         <HIVE ...> ... </HIVE> blocks. \
         Use `hive team` to inspect the team; message any peer with \
         `hive send <name> \"<summary>\" --artifact -`."
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn register_agent_member(
    t: &mut Team,
    pane_id: &str,
    team_name: &str,
    agent_name: &str,
    pane_cli: &str,
    cwd: &str,
    notify: bool,
    group: &str,
) -> Result<Agent> {
    let agent = Agent {
        name: agent_name.to_string(),
        team_name: team_name.to_string(),
        pane_id: pane_id.to_string(),
        model: String::new(),
        cwd: cwd.to_string(),
        session_id: None,
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
        if let Err(e) = agent.send(&hive_join_message(agent_name, team_name)) {
            rollback(t);
            bail!(
                "pane {pane_id} is not reachable over its native transport ({}); \
                 nothing was registered. Fix the inbox/daemon and retry, \
                 or use --no-notify to register without a reachability check.",
                e.0
            );
        }
    }
    if registry_record_member(t, &agent) == RecordVerdict::Missing {
        rollback(t);
        bail!("team '{team_name}' has no registry entry (deleted?); nothing was registered");
    }
    Ok(agent)
}

/// Registry roster row keyed by engine session rather than pane — the orch
/// at create, a Claude session that joins — with the caller's cwd.
pub(crate) fn session_member_row(name: &str, cli: &str, session_id: &str) -> Map<String, Value> {
    let mut row = Map::new();
    row.insert("name".to_string(), Value::String(name.to_string()));
    row.insert("cli".to_string(), Value::String(cli.to_string()));
    row.insert("model".to_string(), Value::String(String::new()));
    row.insert(
        "sessionId".to_string(),
        Value::String(session_id.to_string()),
    );
    row.insert("cwd".to_string(), Value::String(getcwd()));
    row
}

/// Registry roster row for *agent*, resolving its engine identity.
pub(crate) fn member_registry_row(agent: &Agent) -> Map<String, Value> {
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

/// What `registry_record_member` did with the roster row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordVerdict {
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
fn registry_record_member(t: &Team, agent: &Agent) -> RecordVerdict {
    let row = member_registry_row(agent);
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_team_agent<'a>(
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
    let resolved_cli_name = crate::agent_cli::resolve_spawn_cli_name(cli_name);
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
    remember_context(team_name, &ws, LEAD_AGENT_NAME);
    if registry_record_member(t, &agent) == RecordVerdict::Missing {
        t.retire(agent_name);
        bail!("team '{team_name}' has no registry entry (deleted?); '{agent_name}' retired");
    }
    t.agent_named(agent_name)
        .ok_or_else(|| anyhow!("Agent '{agent_name}' not found"))
}

/// pid -> (team, agent) for every live claude team-member engine.
pub(crate) fn live_member_pids() -> HashMap<i32, (String, String)> {
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

pub(crate) fn sorted_member_rows(rows: Vec<Map<String, Value>>) -> Vec<Map<String, Value>> {
    let mut rows = rows;
    rows.sort_by_key(|m| {
        let name = map_str(m, "name");
        (name != LEAD_AGENT_NAME, name)
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::iso;
    use crate::testkit::registry_team;

    fn paneless_agent(name: &str, cli: &str) -> Agent {
        Agent {
            session_id: Some("sid-1".to_string()),
            ..crate::agent::testhook::fake_agent(name, "honey", "", cli)
        }
    }

    fn roster(name: &str) -> Vec<String> {
        crate::registry::load(name)
            .map(|e| {
                let mut v: Vec<String> = crate::naming::roster_names(&e).into_iter().collect();
                v.sort();
                v
            })
            .unwrap_or_default()
    }

    #[test]
    fn test_record_member_never_resurrects_a_deleted_team() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _env = iso(tmp.path());
        let t = registry_team("honey", 100.0, &[]);
        crate::registry::delete_team("honey").unwrap();
        let agent = paneless_agent("worker", "claude");

        assert_eq!(registry_record_member(&t, &agent), RecordVerdict::Missing);

        assert!(!crate::registry::entry_path("honey").unwrap().exists());
    }

    #[test]
    fn test_record_member_leaves_a_recreated_team_alone() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _env = iso(tmp.path());
        let stale = registry_team("honey", 100.0, &["old"]);
        let _fresh = registry_team("honey", 200.0, &["new"]);
        let agent = paneless_agent("worker", "claude");

        assert_eq!(registry_record_member(&stale, &agent), RecordVerdict::Stale);

        let entry = crate::registry::load("honey").unwrap();
        assert_eq!(entry["createdAt"], "200");
        assert_eq!(roster("honey"), vec!["new".to_string()]);
    }

    #[test]
    fn test_headless_created_at_round_trips_through_record_member() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _env = iso(tmp.path());
        // an integral epoch formats without a fraction; the registry compares
        // the key numerically, so it still names the same instance
        let t = registry_team("honey", 1_700_000_000.0, &[]);
        let agent = paneless_agent("worker", "codex");

        assert_eq!(registry_record_member(&t, &agent), RecordVerdict::Written);

        assert_eq!(roster("honey"), vec!["worker".to_string()]);
    }

    #[test]
    fn test_sorted_member_rows_puts_orch_first() {
        let row = |name: &str| {
            let mut m = Map::new();
            m.insert("name".to_string(), Value::String(name.to_string()));
            m
        };
        let sorted = sorted_member_rows(vec![row("zed"), row("orch"), row("abe")]);
        let names: Vec<String> = sorted.iter().map(|m| map_str(m, "name")).collect();
        assert_eq!(names, vec!["orch", "abe", "zed"]);
    }
}
