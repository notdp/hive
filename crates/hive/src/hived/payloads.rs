// --------------------------------------------------------------------------
// send / doctor payloads
// --------------------------------------------------------------------------

use std::path::Path;

use anyhow::{bail, Result};
use serde_json::{Map, Value};

use crate::agent::Agent;
use crate::message::{format_hive_envelope, format_node_envelope};
use crate::team::Team;
use crate::{bus, devlog};

use super::*;

pub(crate) fn resolve_live_agent_impl(team_name: &str, agent_name: &str) -> Result<(Team, Agent)> {
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
pub(crate) fn check_send_gate_impl(target: &Agent) -> Result<()> {
    let runtime = if !target.pane_id.is_empty() {
        hooked_member_runtime_payload(&target.pane_id, "agent")
    } else {
        headless_member_runtime(target)
    };
    if runtime.get("inputState").and_then(Value::as_str) != Some("waiting_user") {
        return Ok(());
    }
    let reason = map_get_str(&runtime, "inputReason");
    if SEND_GATE_WAIVED_REASONS.contains(&reason.as_str()) {
        return Ok(());
    }
    bail!("target agent is waiting for a user answer; answer it in the target pane")
}

/// Who a send is from. A member (or guest) send carries its sender on the
/// ledger row and in the envelope; a `hive node run` dispatch has no sender
/// at all — the runner reads the member's turn instead of waiting for a
/// message back — so its row's `from_agent` is empty and its envelope has
/// no `from`. The mode is explicit: an empty member name is never a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOrigin<'a> {
    Member(&'a str),
    Node,
}

pub(crate) fn send_payload(
    workspace: &str,
    team_name: &str,
    origin: SendOrigin<'_>,
    target_agent: &str,
    body: &str,
    artifact: &str,
) -> Result<Map<String, Value>> {
    let (_team, target) = hooked_resolve_live_agent(team_name, target_agent)?;

    // Side effect only: errors if target is waiting for a user answer.
    hooked_check_send_gate(&target)?;

    let from_agent = match origin {
        SendOrigin::Member(sender) => sender,
        SendOrigin::Node => "",
    };
    let seq = bus::write_send_event(workspace, from_agent, target_agent, body, artifact)?;
    let envelope = match origin {
        SendOrigin::Member(sender) => format_hive_envelope(sender, target_agent, body, artifact),
        SendOrigin::Node => format_node_envelope(target_agent, body, artifact),
    };

    let mut payload = Map::new();
    payload.insert("ok".to_string(), Value::Bool(true));
    payload.insert("to".to_string(), Value::from(target_agent));
    payload.insert("seq".to_string(), Value::from(seq));
    // Fire-and-forget past this point: the transport verdict is the only
    // delivery state. The daemon/channel either accepted the message (its
    // own contract queues and processes it) or refused it — there is no
    // tracked in-between, no confirmation oracle, and nothing to poll. A
    // claude member mid-turn queues the message itself (`priority: next`
    // folds it in at the next tool boundary) — no hived hold on top.
    // The transport's origin label is the message author, qualified so a
    // Claude session outside the team can address it back verbatim. A guest
    // or ccd sender already carries its prefix; a node dispatch has no
    // author, so the team itself is the label.
    let sender_label = match origin {
        SendOrigin::Member(sender) if sender.contains('.') => sender.to_string(),
        SendOrigin::Member(sender) => format!("{team_name}.{sender}"),
        SendOrigin::Node => team_name.to_string(),
    };
    if let Err(exc) = hooked_agent_send(&target, &envelope, &sender_label) {
        let mut refused = Map::new();
        refused.insert("ok".to_string(), Value::Bool(false));
        refused.insert(
            "error".to_string(),
            Value::from(format!("transport refused {target_agent}: {exc}")),
        );
        refused.insert("seq".to_string(), Value::from(seq));
        return Ok(refused);
    }
    // Accepted for a pane member: unread on the status bar until the
    // status tick next sees that pane busy.
    if !target.pane_id.is_empty() {
        unread_pending()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(target.pane_id.clone());
    }

    if !artifact.is_empty() {
        payload.insert("artifact".to_string(), Value::from(artifact));
    }
    Ok(payload)
}

pub(crate) fn doctor_payload(
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
