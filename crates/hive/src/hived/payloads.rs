// --------------------------------------------------------------------------
// thread / send / doctor payloads
// --------------------------------------------------------------------------

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{bail, Result};
use serde_json::{Map, Value};

use crate::agent::Agent;
use crate::runtime_state::{format_hive_envelope, project_thread_event};
use crate::team::Team;
use crate::{bus, devlog};

use super::*;

pub fn _thread_payload(workspace: &str, message_id: &str) -> Result<Map<String, Value>> {
    let events = bus::read_events_with_ns(workspace)?;
    let mut send_events: HashMap<String, (i64, Map<String, Value>)> = HashMap::new();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();

    for (seq, event) in events {
        let event_map = match serde_json::to_value(&event) {
            Ok(Value::Object(map)) => map,
            _ => continue,
        };
        let event_msg_id = event.msg_id.clone();
        if event_msg_id.is_empty() {
            continue;
        }
        if event.intent == "send" {
            let parent = event.in_reply_to.clone();
            send_events.insert(event_msg_id.clone(), (seq, event_map));
            if !parent.is_empty() {
                children.entry(parent).or_default().push(event_msg_id);
            }
        }
    }

    if !send_events.contains_key(message_id) {
        return Ok(err_response(format!(
            "no send event found with msgId '{message_id}'"
        )));
    }

    let mut root_id = message_id.to_string();
    let mut seen: HashSet<String> = HashSet::new();
    loop {
        let (_, event) = &send_events[&root_id];
        let parent = map_get_str(event, "inReplyTo");
        if parent.is_empty() || !send_events.contains_key(&parent) || seen.contains(&parent) {
            break;
        }
        seen.insert(root_id.clone());
        root_id = parent;
    }

    let mut depth_map: HashMap<String, i64> = HashMap::new();
    let mut thread_ids: HashSet<String> = HashSet::new();

    fn walk(
        current_id: &str,
        depth: i64,
        thread_ids: &mut HashSet<String>,
        depth_map: &mut HashMap<String, i64>,
        children: &HashMap<String, Vec<String>>,
        send_events: &HashMap<String, (i64, Map<String, Value>)>,
    ) {
        if thread_ids.contains(current_id) {
            return;
        }
        thread_ids.insert(current_id.to_string());
        depth_map.insert(current_id.to_string(), depth);
        let mut child_ids = children.get(current_id).cloned().unwrap_or_default();
        child_ids.sort_by_key(|item| send_events[item].0);
        for child_id in child_ids {
            walk(
                &child_id,
                depth + 1,
                thread_ids,
                depth_map,
                children,
                send_events,
            );
        }
    }

    walk(
        &root_id,
        0,
        &mut thread_ids,
        &mut depth_map,
        &children,
        &send_events,
    );

    let mut sorted_ids: Vec<String> = thread_ids.into_iter().collect();
    sorted_ids.sort_by_key(|item| send_events[item].0);
    let mut items: Vec<Value> = Vec::new();
    for thread_msg_id in sorted_ids {
        let (_, event) = &send_events[&thread_msg_id];
        let mut item = project_thread_event(event);
        item.insert(
            "depth".to_string(),
            Value::from(depth_map.get(&thread_msg_id).copied().unwrap_or(0)),
        );
        if thread_msg_id == message_id {
            item.insert("focus".to_string(), Value::Bool(true));
        }
        items.push(Value::Object(item));
    }

    let mut payload = Map::new();
    payload.insert("ok".to_string(), Value::Bool(true));
    payload.insert("rootMsgId".to_string(), Value::from(root_id));
    payload.insert("focusMsgId".to_string(), Value::from(message_id));
    payload.insert("messages".to_string(), Value::Array(items));
    Ok(payload)
}

pub fn _resolve_live_agent_impl(team_name: &str, agent_name: &str) -> Result<(Team, Agent)> {
    let team = hooked_team_load(team_name)?;
    let agent = team.get(agent_name)?;
    if !hooked_agent_is_alive(&agent) {
        bail!("agent '{agent_name}' is not alive");
    }
    Ok((team, agent))
}

/// Raise when the target agent is waiting on its human.
///
/// Reads the member's runtime (native daemon / registry state for
/// codex, grok and claude; transcript gate for unmanaged panes) instead of
/// re-deriving it — one judgement for every CLI, and no silent skip when a
/// transcript cannot be resolved.
pub fn _check_send_gate_impl(target: &Agent) -> Result<()> {
    let runtime = if !target.pane_id.is_empty() {
        hooked_member_runtime_payload(&target.pane_id, "agent")
    } else {
        _headless_member_runtime(target)
    };
    if runtime.get("inputState").and_then(Value::as_str) != Some("waiting_user") {
        return Ok(());
    }
    let reason = map_get_str(&runtime, "inputReason");
    if _SEND_GATE_WAIVED_REASONS.contains(&reason.as_str()) {
        return Ok(());
    }
    bail!("target agent is waiting for a user answer; answer it in the target pane")
}

#[allow(clippy::too_many_arguments)]
pub fn _send_payload(
    workspace: &str,
    team_name: &str,
    sender_agent: &str,
    _sender_pane: &str,
    target_agent: &str,
    body: &str,
    artifact: &str,
    reply_to: &str,
) -> Result<Map<String, Value>> {
    if target_agent == FLOW_MAILBOX_AGENT {
        // The flow runner's mailbox: it owns no pane and no transport —
        // the durable bus row IS the delivery, and the runner polls for
        // it. Members answer a flow dispatch with an ordinary
        // `hive send flow`, which lands here.
        let event = bus::write_send_event(
            workspace,
            sender_agent,
            target_agent,
            body.trim(),
            artifact,
            None,
            reply_to,
        )?;
        let mut payload = Map::new();
        payload.insert("ok".to_string(), Value::Bool(true));
        payload.insert("to".to_string(), Value::from(target_agent));
        payload.insert("msgId".to_string(), Value::from(event.msg_id));
        payload.insert("mailbox".to_string(), Value::Bool(true));
        return Ok(payload);
    }

    let (_team, target) = hooked_resolve_live_agent(team_name, target_agent)?;
    let normalized_body = body.trim();

    // Side effect only: errors if target is waiting for a user answer.
    hooked_check_send_gate(&target)?;

    let event = bus::write_send_event(
        workspace,
        sender_agent,
        target_agent,
        normalized_body,
        artifact,
        None,
        reply_to,
    )?;
    let message_id = event.msg_id;
    let envelope = format_hive_envelope(
        sender_agent,
        target_agent,
        body,
        artifact,
        &message_id,
        reply_to,
    );

    let mut payload = Map::new();
    payload.insert("ok".to_string(), Value::Bool(true));
    payload.insert("to".to_string(), Value::from(target_agent));
    payload.insert("msgId".to_string(), Value::from(message_id.clone()));
    // Fire-and-forget past this point: the transport verdict is the only
    // delivery state. The daemon/channel either accepted the message (its
    // own contract queues and processes it) or refused it — there is no
    // tracked in-between, no confirmation oracle, and nothing to poll. A
    // claude member mid-turn queues the message itself (`priority: next`
    // folds it in at the next tool boundary) — no hived hold on top.
    if let Err(exc) = hooked_agent_send(&target, &envelope) {
        let mut refused = Map::new();
        refused.insert("ok".to_string(), Value::Bool(false));
        refused.insert(
            "error".to_string(),
            Value::from(format!("transport refused {target_agent}: {exc}")),
        );
        refused.insert("msgId".to_string(), Value::from(message_id));
        return Ok(refused);
    }

    if !artifact.is_empty() {
        payload.insert("artifact".to_string(), Value::from(artifact));
    }
    Ok(payload)
}

pub fn _doctor_payload(
    workspace: &str,
    team_name: &str,
    target_agent: &str,
    verbose: bool,
    hived: Option<&Map<String, Value>>,
) -> Result<Map<String, Value>> {
    let team = hooked_team_load(team_name)?;
    let target = team.get(target_agent)?;

    let alive = hooked_agent_is_alive(&target);
    let mut diag = Map::new();
    diag.insert("ok".to_string(), Value::Bool(true));
    diag.insert("agent".to_string(), Value::from(target_agent));
    diag.insert("team".to_string(), Value::from(team.name.clone()));
    if let Some(hived) = hived {
        if !hived.is_empty() {
            diag.insert("hived".to_string(), Value::Object(hived.clone()));
        }
    }
    let runtime = hooked_member_runtime_payload(&target.pane_id, "agent");
    diag.insert(
        "alive".to_string(),
        Value::Bool(
            runtime
                .get("alive")
                .and_then(Value::as_bool)
                .unwrap_or(alive),
        ),
    );
    if let Some(cli_alive) = runtime.get("cliAlive") {
        diag.insert(
            "cliAlive".to_string(),
            Value::Bool(cli_alive.as_bool().unwrap_or(false)),
        );
    }
    for key in ["model", "sessionId", "inputState"] {
        let value = map_get_str(&runtime, key);
        if !value.is_empty() {
            diag.insert(key.to_string(), Value::from(value));
        }
    }
    if let Some(busy) = runtime.get("busy") {
        diag.insert(
            "busy".to_string(),
            Value::Bool(busy.as_bool().unwrap_or(false)),
        );
    }
    let turn_phase = map_get_str(&runtime, "turnPhase");
    if !turn_phase.is_empty() {
        diag.insert("turnPhase".to_string(), Value::from(turn_phase));
    }
    if verbose {
        diag.insert("pane".to_string(), Value::from(target.pane_id.clone()));
        diag.insert("teamMembers".to_string(), Value::from(team.agents.len()));
        let cli = map_get_str(&runtime, "_cli");
        if !cli.is_empty() {
            diag.insert("cli".to_string(), Value::from(cli.clone()));
        }
        if cli == "codex" {
            let mut codex = Map::new();
            codex.insert(
                "socket".to_string(),
                Value::from(
                    hooked_cas_shared_socket_path()
                        .to_string_lossy()
                        .to_string(),
                ),
            );
            codex.insert("alive".to_string(), Value::Bool(hooked_cas_daemon_alive()));
            codex.insert(
                "threadId".to_string(),
                match hooked_cas_thread_id_for_pane(&target.pane_id) {
                    Some(tid) => Value::from(tid),
                    None => Value::Null,
                },
            );
            diag.insert("codexDaemon".to_string(), Value::Object(codex));
        }
        if cli == "claude" {
            let job_id = hooked_cb_job_id_for_pane(&target.pane_id).unwrap_or_default();
            if !job_id.is_empty() {
                let mut job = Map::new();
                job.insert("jobId".to_string(), Value::from(job_id.clone()));
                job.insert(
                    "engineAlive".to_string(),
                    Value::Bool(hooked_cb_engine_session_for_job(&job_id).is_some()),
                );
                diag.insert("claudeJob".to_string(), Value::Object(job));
            }
            if runtime.contains_key("_viewKind") {
                // What the pane's viewer is showing right now — the member's
                // own job, another session, or the panel list.
                let mut view = Map::new();
                view.insert(
                    "kind".to_string(),
                    Value::from(map_get_str(&runtime, "_viewKind")),
                );
                view.insert(
                    "certainty".to_string(),
                    Value::from(map_get_str(&runtime, "_viewCertainty")),
                );
                view.insert(
                    "jobId".to_string(),
                    Value::from(map_get_str(&runtime, "_viewedJob")),
                );
                view.insert(
                    "member".to_string(),
                    Value::from(map_get_str(&runtime, "_viewedMember")),
                );
                view.insert(
                    "onMember".to_string(),
                    Value::Bool(
                        !job_id.is_empty() && map_get_str(&runtime, "_viewedJob") == job_id,
                    ),
                );
                diag.insert("claudeView".to_string(), Value::Object(view));
            }
        }
        if let Some(engine_state) = runtime.get("_engineState") {
            diag.insert("engineState".to_string(), engine_state.clone());
        }
        if let Some(input_reason) = runtime.get("inputReason") {
            diag.insert("inputReason".to_string(), input_reason.clone());
        }
        if let Some(transcript) = runtime.get("_transcript") {
            diag.insert("transcript".to_string(), transcript.clone());
        }
        if let Some(exists) = runtime.get("_transcriptExists") {
            diag.insert("transcriptExists".to_string(), exists.clone());
        }
        if let Some(size) = runtime.get("_transcriptSize") {
            diag.insert("transcriptSize".to_string(), size.clone());
        }
        if let Some(gate_reason) = runtime.get("_gateReason") {
            diag.insert("gateReason".to_string(), gate_reason.clone());
        }
        let phase_observed = map_get_str(&runtime, "phaseObservedAt");
        if !phase_observed.is_empty() {
            diag.insert("phaseObservedAt".to_string(), Value::from(phase_observed));
        }
        if let Some(evidence) = runtime.get("_safetyEvidence") {
            diag.insert("safetyEvidence".to_string(), evidence.clone());
        }
        diag.insert("workspace".to_string(), Value::from(workspace));
        diag.insert(
            "runDir".to_string(),
            Value::from(
                devlog::run_dir(Path::new(workspace))
                    .to_string_lossy()
                    .to_string(),
            ),
        );
        diag.insert(
            "logs".to_string(),
            Value::Object(devlog::log_paths(Path::new(workspace))),
        );
        diag.insert(
            "eventCount".to_string(),
            Value::from(bus::count_events(workspace)?),
        );
    }
    Ok(diag)
}
