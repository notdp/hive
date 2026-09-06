//! Send addressing and the hived send seam: which team owns the target of
//! a `hive send` (a qualified `<group>.<name>` across windows, a bare name
//! in the caller's team, a Claude-session guest's target), the long-body
//! hint, and the request every dispatch — `hive send`, a spawn's `--task`,
//! a `hive workflow run` task — hands the hived.

use std::collections::HashSet;
use std::fmt;

use anyhow::{bail, Result};
use serde_json::{Map, Value};

use crate::agent::Agent;
use crate::hived::RequestFailure;
use crate::json_fields::{is_set, map_str};
use crate::team::{ensure_team_hived, load_team, resolve_scoped_team, Team};
use crate::tmux;
use crate::tmux::PaneInfo;

#[allow(clippy::too_many_arguments)]
pub(crate) fn request_send_payload(
    workspace: &str,
    team: &Team,
    sender_agent: &str,
    target_agent: &str,
    body: &str,
    artifact: &str,
    command_name: &str,
    warn_on_long_body: bool,
) -> Result<Map<String, Value>> {
    if warn_on_long_body {
        maybe_warn_long_body(body, command_name);
    }
    ensure_team_hived(team, std::path::Path::new(workspace))?;
    let payload = crate::hived::request_send(
        workspace,
        &team.name,
        sender_agent,
        target_agent,
        body,
        artifact,
    );
    hived_send_result(workspace, payload, command_name)
}

/// Why a node dispatch has no seq. `Refused`: the hived answered `ok:false`
/// (a transport refusal, an unknown member, the send gate) or the request
/// never reached it — the task is definitely not with the member.
/// `Unknown`: the request went out and no usable answer came back (a read
/// timeout, a dropped connection, an empty or unparsable reply) — the
/// hived may have injected the task, and only the transcript can say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchFailure {
    Refused(String),
    Unknown(String),
}

impl fmt::Display for DispatchFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DispatchFailure::Refused(reason) | DispatchFailure::Unknown(reason) => {
                f.write_str(reason)
            }
        }
    }
}

/// The dispatch of a `hive workflow run` task: the hived's node-dispatch action, which
/// writes a senderless ledger row and injects a from-less envelope.
pub(crate) fn request_node_dispatch(
    workspace: &str,
    team: &Team,
    target_agent: &str,
    body: &str,
    artifact: &str,
) -> Result<Map<String, Value>, DispatchFailure> {
    ensure_team_hived(team, std::path::Path::new(workspace))
        .map_err(|err| DispatchFailure::Refused(err.to_string()))?;
    let answer =
        crate::hived::request_node_dispatch(workspace, &team.name, target_agent, body, artifact);
    hived_answer(workspace, answer, "node dispatch")
}

fn hived_send_result(
    workspace: &str,
    payload: Option<Map<String, Value>>,
    command_name: &str,
) -> Result<Map<String, Value>> {
    // An ordinary send has nothing to keep pending on a lost answer: both
    // failure kinds are one refusal to the caller.
    let answer = payload.ok_or_else(|| RequestFailure::NotSent(String::new()));
    hived_answer(workspace, answer, command_name).map_err(|e| anyhow::anyhow!("{e}"))
}

/// The hived's answer as a result: `ok:false` and an unsent request are
/// `Refused`, a lost answer is `Unknown`, and `ok` is stripped from a
/// success.
fn hived_answer(
    workspace: &str,
    answer: Result<Map<String, Value>, RequestFailure>,
    command_name: &str,
) -> Result<Map<String, Value>, DispatchFailure> {
    let payload = match answer {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => {
            return Err(DispatchFailure::Unknown(format!(
                "{command_name}: empty answer from the hived"
            )))
        }
        Err(RequestFailure::AnswerLost(reason)) => {
            return Err(DispatchFailure::Unknown(format!(
                "{command_name}: answer lost ({reason})"
            )))
        }
        Err(RequestFailure::NotSent(_)) => {
            return Err(DispatchFailure::Refused(
                crate::devlog::hived_unavailable_message(std::path::Path::new(workspace)),
            ))
        }
    };
    if payload.get("ok") == Some(&Value::Bool(false)) {
        let error = match payload.get("error") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => format!("{command_name} failed"),
        };
        return Err(DispatchFailure::Refused(error));
    }
    let mut normalized = payload;
    normalized.shift_remove("ok");
    Ok(normalized)
}

/// Poll hived team-runtime until every agent's first skill turn completes
/// (`inputState == 'ready'`). Returns the agents still not ready at deadline.
pub(crate) fn wait_for_peer_ready(
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

/// Locate a pane by qualified agent name `<prefix>.<name>` across a pane
/// listing. Pure core of `find_qualified_agent_target` for tests.
fn find_qualified_agent_target_in(
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

fn find_qualified_agent_target(
    qualified: &str,
) -> std::result::Result<Option<(String, String)>, String> {
    find_qualified_agent_target_in(&tmux::list_panes_all(), qualified)
}

/// Split `<team>.<member>` when the prefix names an existing team.
///
/// Team existence is the registry first (a team whose window is gone still exists),
/// the window scan second (a live pre-registry team). Returns
/// `(team, member)` or `("", addr)` when the prefix names no team.
pub(crate) fn split_team_address(addr: &str) -> (String, String) {
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
    let window_claims = crate::team::find_team_window(prefix, "")
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
pub(crate) fn resolve_send_target_team(to_agent: &str) -> Result<(String, Team)> {
    if to_agent.contains('.') {
        let resolved = match find_qualified_agent_target(to_agent) {
            Ok(resolved) => resolved,
            Err(err) => bail!("{err}"),
        };
        let (target_team_name, _) = match resolved {
            Some(pair) => pair,
            None => bail!(
                "agent '{to_agent}' not found in any team \
                 (check @hive-agent tag on the target pane)"
            ),
        };
        let team = load_team(&target_team_name, "")?;
        return Ok((target_team_name, team));
    }
    let (team_name, t) = resolve_scoped_team(None, true)?;
    Ok((
        team_name.expect("required resolve returned no team"),
        t.expect("required resolve returned no team"),
    ))
}

/// Target resolution for a Claude-session guest (outside tmux).
pub(crate) fn resolve_guest_send_target(to_agent: &str, team: &str) -> Result<(String, Team)> {
    if !team.is_empty() {
        let t = load_team(team, "")?;
        if existing_team_agent(&t, to_agent).is_none() {
            bail!("agent '{to_agent}' not found in team '{team}'");
        }
        let name = t.name.clone();
        return Ok((name, t));
    }
    let candidates: Vec<PaneInfo> = tmux::list_panes_all()
        .into_iter()
        .filter(|p| p.agent == to_agent && !p.team.is_empty())
        .collect();
    let registry_teams: HashSet<String> = crate::registry::list_entries()
        .into_iter()
        .filter(|e| !is_set(e.get("corrupt")))
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
        .chain(registry_teams)
        .collect::<HashSet<String>>()
        .into_iter()
        .collect();
    teams.sort();
    if teams.is_empty() {
        bail!("agent '{to_agent}' not found in any team (see `hive ls`)");
    }
    if teams.len() > 1 {
        let addresses = teams
            .iter()
            .map(|name| format!("{name}.{to_agent}"))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "agent '{to_agent}' exists in {} teams; address one of: {addresses}",
            teams.len()
        );
    }
    let team_name = teams.remove(0);
    let loaded = load_team(&team_name, "")?;
    Ok((team_name, loaded))
}

fn existing_team_agent(t: &Team, agent_name: &str) -> Option<Agent> {
    t.get(agent_name).ok()
}

fn maybe_warn_long_body(body: &str, command: &str) {
    if let Some(hint) = crate::message::body_warning_hint(body) {
        eprintln!("{}", crate::message::format_body_warning(command, &hint));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(agent: &str, team: &str, group: &str, pane_id: &str) -> PaneInfo {
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

    #[test]
    fn test_hived_answer_splits_refused_from_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_string_lossy().to_string();
        let refused = |reason: &str| {
            Map::from_iter([
                ("ok".to_string(), Value::Bool(false)),
                ("error".to_string(), Value::from(reason)),
            ])
        };
        // The hived said no: the task never left.
        assert_eq!(
            hived_answer(&ws, Ok(refused("transport refused")), "node dispatch"),
            Err(DispatchFailure::Refused("transport refused".to_string()))
        );
        assert_eq!(
            hived_answer(
                &ws,
                Ok(Map::from_iter([("ok".to_string(), Value::Bool(false))])),
                "node dispatch"
            ),
            Err(DispatchFailure::Refused("node dispatch failed".to_string()))
        );
        // The request never reached the hived: refused, with the
        // unavailable diagnosis.
        assert_eq!(
            hived_answer(
                &ws,
                Err(RequestFailure::NotSent("connect refused".to_string())),
                "node dispatch"
            ),
            Err(DispatchFailure::Refused("hived unavailable".to_string()))
        );
        // The request went out and the answer did not come back: unknown.
        assert_eq!(
            hived_answer(
                &ws,
                Err(RequestFailure::AnswerLost("read timed out".to_string())),
                "node dispatch"
            ),
            Err(DispatchFailure::Unknown(
                "node dispatch: answer lost (read timed out)".to_string()
            ))
        );
        assert_eq!(
            hived_answer(&ws, Ok(Map::new()), "node dispatch"),
            Err(DispatchFailure::Unknown(
                "node dispatch: empty answer from the hived".to_string()
            ))
        );
        // A success is handed on without its `ok`.
        let ok = Map::from_iter([
            ("ok".to_string(), Value::Bool(true)),
            ("seq".to_string(), Value::from(3)),
        ]);
        assert_eq!(
            hived_answer(&ws, Ok(ok), "node dispatch"),
            Ok(Map::from_iter([("seq".to_string(), Value::from(3))]))
        );
        // An ordinary send folds both kinds into one error string.
        let err = hived_send_result(&ws, None, "send").unwrap_err();
        assert_eq!(err.to_string(), "hived unavailable");
        let err = hived_send_result(&ws, Some(refused("gate closed")), "send").unwrap_err();
        assert_eq!(err.to_string(), "gate closed");
    }

    #[test]
    fn test_find_qualified_returns_none_for_bare_name() {
        assert_eq!(find_qualified_agent_target_in(&[], "orch"), Ok(None));
    }

    #[test]
    fn test_find_qualified_finds_unique_match() {
        let panes = vec![
            pane("kraken.worker-1", "peer-1", "kraken", "%1"),
            pane("kraken.judge-1", "peer-1", "kraken", "%2"),
            pane("other", "peer-1", "", "%3"),
        ];
        assert_eq!(
            find_qualified_agent_target_in(&panes, "kraken.worker-1"),
            Ok(Some(("peer-1".to_string(), "kraken.worker-1".to_string())))
        );
    }

    #[test]
    fn test_find_qualified_supports_public_squad_name_namespace() {
        let panes = vec![
            pane("peaky.worker-1000", "dev-0-duo-1000", "peaky", "%1"),
            pane("shelby.worker-1000", "dev-1-duo-1000", "shelby", "%2"),
            pane("peaky.orch", "dev-0", "peaky", "%3"),
        ];
        assert_eq!(
            find_qualified_agent_target_in(&panes, "peaky.worker-1000"),
            Ok(Some((
                "dev-0-duo-1000".to_string(),
                "peaky.worker-1000".to_string()
            )))
        );
    }

    #[test]
    fn test_find_qualified_returns_none_when_agent_missing() {
        let panes = vec![pane("kraken.worker-1", "peer-1", "kraken", "%1")];
        assert_eq!(
            find_qualified_agent_target_in(&panes, "kraken.worker-2"),
            Ok(None)
        );
    }

    #[test]
    fn test_find_qualified_raises_on_ambiguous() {
        let panes = vec![
            pane("kraken.worker-1", "peer-1", "kraken", "%1"),
            pane("kraken.worker-1", "peer-2", "kraken", "%5"),
        ];
        let err = find_qualified_agent_target_in(&panes, "kraken.worker-1").unwrap_err();
        assert!(err.contains("unique"));
    }

    #[test]
    fn test_find_qualified_resolves_missing_group() {
        // A pane with matching @hive-agent but no @hive-group is still routable.
        let panes = vec![pane("kraken.worker-1", "peer-1", "", "%1")];
        assert_eq!(
            find_qualified_agent_target_in(&panes, "kraken.worker-1"),
            Ok(Some(("peer-1".to_string(), "kraken.worker-1".to_string())))
        );
    }

    #[test]
    fn test_find_qualified_rejects_conflicting_group() {
        // A pane with @hive-agent=kraken.worker-1 but @hive-group=mafia is a
        // tagging mistake — the resolver must error, not silently route.
        let panes = vec![pane("kraken.worker-1", "peer-1", "mafia", "%1")];
        let err = find_qualified_agent_target_in(&panes, "kraken.worker-1").unwrap_err();
        assert!(err.contains("conflicting"));
    }

    #[test]
    fn test_find_qualified_ignores_same_suffix_in_other_public_squad() {
        let panes = vec![pane("shelby.worker-1000", "dev-1-duo-1000", "shelby", "%2")];
        assert_eq!(
            find_qualified_agent_target_in(&panes, "peaky.worker-1000"),
            Ok(None)
        );
    }

    #[test]
    fn test_find_qualified_requires_non_empty_group_prefix() {
        assert_eq!(find_qualified_agent_target_in(&[], ".worker-1"), Ok(None));
    }

    #[test]
    fn test_find_qualified_ambiguous_with_missing_groups() {
        // Duplicate @hive-agent across panes is ambiguous even when both lack group.
        let panes = vec![
            pane("kraken.worker-1", "peer-1", "", "%1"),
            pane("kraken.worker-1", "peer-2", "", "%5"),
        ];
        let err = find_qualified_agent_target_in(&panes, "kraken.worker-1").unwrap_err();
        assert!(err.contains("unique"));
    }

    #[test]
    fn test_find_qualified_skips_pane_without_team() {
        // A pane with matching agent name but empty team is not a valid target.
        let panes = vec![pane("kraken.worker-1", "", "kraken", "%1")];
        assert_eq!(
            find_qualified_agent_target_in(&panes, "kraken.worker-1"),
            Ok(None)
        );
    }

    #[test]
    fn test_split_team_address_passes_through_bare_and_malformed() {
        assert_eq!(
            split_team_address("plain"),
            ("".to_string(), "plain".to_string())
        );
        assert_eq!(split_team_address(".x"), ("".to_string(), ".x".to_string()));
        assert_eq!(split_team_address("x."), ("".to_string(), "x.".to_string()));
    }
}
