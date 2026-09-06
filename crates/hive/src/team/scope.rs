//! Which team a verb acts on, and the workspace and hived that come with
//! it: explicit `-t`, else the caller's binding; the current-context file;
//! the team's window identity for the hived; the `hive team` payload.

use std::path::Path;

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};

use super::Team;
use crate::identity;
use crate::json_fields::map_str;
use crate::paths::getcwd;
use crate::tmux;

pub(crate) fn load_team(team: &str, prefer_pane: &str) -> Result<Team> {
    Team::load(team, prefer_pane).map_err(|_| anyhow!("team '{team}' not found"))
}

/// Addressing order: explicit team -> binding discovery (pane tags, then the
/// engine's own session row). An explicit team is the caller's intent — it
/// loads from the registry wherever the caller happens to be.
pub(crate) fn resolve_scoped_team(
    team: Option<&str>,
    required: bool,
) -> Result<(Option<String>, Option<Team>)> {
    if let Some(team) = team.filter(|t| !t.is_empty()) {
        let loaded = load_team(team, "")?;
        return Ok((Some(team.to_string()), Some(loaded)));
    }
    if let Some(discovered) = identity::default_team() {
        let prefer_pane = identity::current_pane_id().unwrap_or_default();
        let loaded = load_team(&discovered, &prefer_pane)?;
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

pub(crate) fn resolve_workspace(team: Option<&Team>, required: bool) -> Result<String> {
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

pub(crate) fn ensure_pane_in_scope(t: &Team, pane_id: &str) -> Result<()> {
    if pane_id.is_empty() {
        return Ok(());
    }
    let pane_window = tmux::get_pane_window_target(pane_id).unwrap_or_default();
    let team_window = t.tmux_window.clone();
    if !team_window.is_empty() && !pane_window.is_empty() && pane_window != team_window {
        bail!(
            "pane '{pane_id}' is in tmux window '{pane_window}', not team '{}' window '{team_window}'",
            t.name
        );
    }
    if let Some(pane_team) = tmux::get_pane_option(pane_id, "hive-team") {
        if !pane_team.is_empty() && pane_team != t.name {
            bail!("pane '{pane_id}' already belongs to team '{pane_team}'");
        }
    }
    Ok(())
}

pub(crate) fn add_runtime_location_fields(payload: &mut Map<String, Value>) {
    if !payload.contains_key("runtimeWorkspace") && payload.contains_key("workspace") {
        if let Some(ws) = payload.shift_remove("workspace") {
            payload.insert("runtimeWorkspace".to_string(), ws);
        }
    }
    payload.insert("cwd".to_string(), Value::String(getcwd()));
}

pub(crate) fn remember_context(team: &str, workspace: &str, agent: &str) {
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

fn team_window_identity(t: &mut Team) -> (String, String) {
    let window_target = if !t.tmux_window.is_empty() {
        t.tmux_window.clone()
    } else {
        identity::current_window_target().unwrap_or_default()
    };
    let mut window_id = t.tmux_window_id.clone();
    if window_id.is_empty() && !window_target.is_empty() {
        window_id = tmux::get_window_id(&window_target).unwrap_or_default();
    }
    if window_id.is_empty() {
        window_id = identity::current_window_id().unwrap_or_default();
    }
    if !window_target.is_empty() && t.tmux_window.is_empty() {
        t.tmux_window = window_target.clone();
    }
    if !window_id.is_empty() && t.tmux_window_id.is_empty() {
        t.tmux_window_id = window_id.clone();
    }
    (window_target, window_id)
}

/// Start (or find) the team's hived, filling the team's window identity.
pub(crate) fn start_team_hived(t: &mut Team, workspace: &str) -> Option<i32> {
    let (window_target, window_id) = team_window_identity(t);
    crate::hived::ensure_hived(workspace, &t.name, &window_target, &window_id)
}

/// Seam used by node.rs (return ignored there; team not mutated).
pub(crate) fn ensure_team_hived(t: &Team, workspace: &Path) {
    let mut clone = t.clone();
    let _ = start_team_hived(&mut clone, &workspace.to_string_lossy());
}

fn augment_team_payload_with_runtime(
    t: &mut Team,
    mut payload: Map<String, Value>,
) -> Map<String, Value> {
    let ws = resolve_workspace(Some(&*t), false).unwrap_or_default();
    if ws.is_empty() {
        return payload;
    }
    let _ = start_team_hived(t, &ws);
    let Some(runtime) = super::usable_runtime(crate::hived::request_team_runtime(&ws, &t.name))
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

fn should_show_description(desc: Option<&Value>) -> bool {
    match desc {
        Some(Value::String(s)) if !s.is_empty() => !s.starts_with("auto-init from "),
        _ => false,
    }
}

pub(crate) fn team_status_payload(t: &mut Team) -> Map<String, Value> {
    let status = t.status();
    let mut payload = augment_team_payload_with_runtime(t, status);
    if !should_show_description(payload.get("description")) {
        payload.shift_remove("description");
    }
    let me = identity::self_member_for_team(&t.name);
    if !me.is_empty() {
        payload.insert("self".to_string(), Value::String(me));
    }
    add_runtime_location_fields(&mut payload);
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_show_description_filters_auto_init() {
        assert!(!should_show_description(None));
        assert!(!should_show_description(Some(
            &Value::String(String::new())
        )));
        assert!(!should_show_description(Some(&Value::String(
            "auto-init from tmux dev (dev:1)".to_string()
        ))));
        assert!(should_show_description(Some(&Value::String(
            "real description".to_string()
        ))));
    }
}
