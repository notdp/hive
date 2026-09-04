// --------------------------------------------------------------------------
// per-CLI runtime payloads
// --------------------------------------------------------------------------

use std::collections::HashMap;
use std::fs;

use anyhow::Result;
use serde_json::{Map, Value};

use crate::agent::Agent;
use crate::runtime_snapshot::RuntimeSnapshot;

use super::*;

/// Native codex runtime from the shared daemon, or None if unmanaged.
pub fn _codex_app_server_runtime_impl(pane_id: &str) -> Option<Map<String, Value>> {
    let rt = hooked_cas_runtime_for_pane(pane_id)?;
    let input_state = if rt.input_state.is_empty() {
        "ready".to_string()
    } else {
        rt.input_state.clone()
    };
    let mut fields = Map::new();
    fields.insert("busy".to_string(), Value::Bool(rt.busy));
    fields.insert("turnPhase".to_string(), Value::from(rt.turn_phase.clone()));
    fields.insert("inputState".to_string(), Value::from(input_state.clone()));
    fields.insert(
        "inputReason".to_string(),
        Value::from(if input_state != "waiting_user" {
            ""
        } else {
            "app_server_active_flag"
        }),
    );
    fields.insert(
        "_runtimeSource".to_string(),
        Value::from("codex_app_server"),
    );
    Some(fields)
}

/// Native grok runtime from the pane's leader, or None if no daemon.
pub fn _grok_leader_runtime(pane_id: &str) -> Option<Map<String, Value>> {
    let rt = hooked_gl_runtime_for_pane(pane_id)?;
    let input_state = if rt.input_state.is_empty() {
        "ready".to_string()
    } else {
        rt.input_state.clone()
    };
    let mut fields = Map::new();
    fields.insert("busy".to_string(), Value::Bool(rt.busy));
    fields.insert("turnPhase".to_string(), Value::from(rt.turn_phase.clone()));
    fields.insert("inputState".to_string(), Value::from(input_state.clone()));
    fields.insert(
        "inputReason".to_string(),
        Value::from(if input_state != "waiting_user" {
            ""
        } else {
            "leader_permission_request"
        }),
    );
    fields.insert("_runtimeSource".to_string(), Value::from("grok-leader"));
    Some(fields)
}

/// Native claude runtime from the pane's bg job, or None if unmanaged.
pub fn _claude_bg_runtime_impl(pane_id: &str) -> Option<Map<String, Value>> {
    let (job_id, record_session, _cwd) = hooked_cb_read_pane_job(pane_id)?;
    Some(_claude_job_runtime(&job_id, &record_session))
}

/// Native claude runtime keyed by the job itself (pane optional).
///
/// Liveness is three-tier: a live engine entry (alive — its ``status`` is
/// the truth); a ledger row without a live engine (asleep — the supervisor
/// parks idle jobs after ~1h, delivery wakes them, so asleep is not dead
/// and is never reaped); no ledger row (gone). The ledger costs a CLI call
/// (~270ms), so it is consulted only when the engine entry is missing,
/// behind a short cache.
pub fn _claude_job_runtime(job_id: &str, record_session: &str) -> Map<String, Value> {
    if let Some(engine) = hooked_cb_engine_session_for_job(job_id) {
        let mut fields = crate::adapters::claude_bg::runtime_from_engine(&engine, None);
        fields.insert("cliAlive".to_string(), Value::Bool(true));
        let sid = if !engine.session_id.is_empty() {
            engine.session_id.clone()
        } else if !record_session.is_empty() {
            record_session.to_string()
        } else {
            "unresolved".to_string()
        };
        fields.insert("sessionId".to_string(), Value::from(sid));
        return fields;
    }
    let mut fields = Map::new();
    fields.insert("_runtimeSource".to_string(), Value::from("claude_bg"));
    fields.insert("busy".to_string(), Value::Bool(false));
    let fallback_sid = |value: &str| {
        if !value.is_empty() {
            value.to_string()
        } else if !record_session.is_empty() {
            record_session.to_string()
        } else {
            "unresolved".to_string()
        }
    };
    let Some(rows) = _claude_jobs_cached() else {
        fields.insert("cliAlive".to_string(), Value::Bool(true));
        fields.insert("inputState".to_string(), Value::from("unknown"));
        fields.insert("inputReason".to_string(), Value::from("ledger_unavailable"));
        fields.insert("sessionId".to_string(), Value::from(fallback_sid("")));
        return fields;
    };
    let Some(row) = rows.get(job_id) else {
        fields.insert("cliAlive".to_string(), Value::Bool(false));
        fields.insert("inputState".to_string(), Value::from("offline"));
        fields.insert("inputReason".to_string(), Value::from("engine_gone"));
        fields.insert("sessionId".to_string(), Value::from(fallback_sid("")));
        return fields;
    };
    // Asleep: parked engine. It still accepts input — delivery wakes it — so
    // it reads as an idle, reachable member, never as a dead one.
    let row_sid = row
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    fields.insert("cliAlive".to_string(), Value::Bool(true));
    fields.insert("_engineState".to_string(), Value::from("asleep"));
    fields.insert("inputState".to_string(), Value::from("ready"));
    fields.insert("inputReason".to_string(), Value::from(""));
    fields.insert("sessionId".to_string(), Value::from(fallback_sid(row_sid)));
    fields
}

/// What the pane's attach viewer is actually showing (the human can switch
/// it to any other bg session).
pub fn _claude_view_fields(pane_id: &str) -> Map<String, Value> {
    let view = hooked_cv_view_for_pane(pane_id, None);
    let mut fields = Map::new();
    fields.insert("_viewKind".to_string(), Value::from(view.kind));
    fields.insert("_viewCertainty".to_string(), Value::from(view.certainty));
    fields.insert("_viewedJob".to_string(), Value::from(view.job_id));
    fields.insert("_viewedMember".to_string(), Value::from(view.member));
    fields
}

/// Job ledger rows keyed by jobId, or None when the CLI call failed.
///
/// Cached briefly: the ledger is only read when an engine entry is missing
/// (rare state), and a ~270ms node start must not run per tick per pane.
pub fn _claude_jobs_cached() -> Option<HashMap<String, Map<String, Value>>> {
    let now = monotonic();
    {
        let cache = claude_jobs_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((expiry, indexed)) = cache.as_ref() {
            if now < *expiry {
                return indexed.clone();
            }
        }
    }
    let rows = hooked_cb_list_jobs();
    let indexed = rows.map(|rows| {
        let mut map = HashMap::new();
        for row in rows {
            let id = row.get("id").and_then(Value::as_str).unwrap_or_default();
            if !id.is_empty() {
                map.insert(id.to_string(), row);
            }
        }
        map
    });
    *claude_jobs_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some((now + _CLAUDE_JOBS_CACHE_TTL, indexed.clone()));
    indexed
}

pub fn _agent_runtime_payload(
    pane_id: &str,
    runtime_snapshot: Option<&RuntimeSnapshot>,
) -> Map<String, Value> {
    let mut runtime = Map::new();
    let alive = hooked_is_pane_alive(pane_id);
    runtime.insert("alive".to_string(), Value::Bool(alive));
    for (key, value) in hooked_busy_output_payload(pane_id) {
        runtime.insert(key, value);
    }
    if !alive {
        runtime.insert("cliAlive".to_string(), Value::Bool(false));
        runtime.insert("busy".to_string(), Value::Bool(false));
        runtime.insert("inputState".to_string(), Value::from("offline"));
        runtime.insert("inputReason".to_string(), Value::from("pane_dead"));
        return runtime;
    }

    // Liveness is runtime evidence only: a retained shell keeps the pane, a
    // stale title, the @hive-cli tag and a surviving thread/job record alive,
    // and none of that alone makes it an agent runtime. For claude the
    // evidence is the bg job's registry/ledger state — the engine never
    // lives on the pane tty, so the process table only proves the viewer.
    let profile = hooked_detect_cli_process_for_pane(pane_id);
    runtime.insert("cliAlive".to_string(), Value::Bool(profile.is_some()));
    runtime.insert(
        "_cli".to_string(),
        Value::from(profile.map(|p| p.name).unwrap_or("unknown")),
    );
    if profile.is_none() || profile.map(|p| p.name) == Some("claude") {
        if let Some(bg_runtime) = hooked_claude_bg_runtime(pane_id) {
            runtime.insert("_cli".to_string(), Value::from("claude"));
            let resolved_model = hooked_resolve_model_for_pane(pane_id, "claude", "");
            if !resolved_model.is_empty() {
                runtime.insert("model".to_string(), Value::from(resolved_model));
            }
            for (key, value) in bg_runtime {
                runtime.insert(key, value);
            }
            for (key, value) in _claude_view_fields(pane_id) {
                runtime.insert(key, value);
            }
            return runtime;
        }
    }
    let Some(profile) = profile else {
        runtime.insert("busy".to_string(), Value::Bool(false)); // shell output is not agent activity
        runtime.insert("inputState".to_string(), Value::from("offline"));
        runtime.insert("inputReason".to_string(), Value::from("cli_exited"));
        return runtime;
    };

    let resolved_model = hooked_resolve_model_for_pane(pane_id, profile.name, "");
    if !resolved_model.is_empty() {
        runtime.insert("model".to_string(), Value::from(resolved_model));
    }

    let Some(adapter) = hooked_adapters_get(profile.name) else {
        runtime.insert("inputState".to_string(), Value::from("unknown"));
        runtime.insert("inputReason".to_string(), Value::from("no_session"));
        return runtime;
    };

    // A hive-managed codex has a recorded thread on the shared app-server
    // daemon: read native runtime signals (busy / turn) over the socket
    // instead of reverse-engineering them from the transcript, and its
    // session id IS the recorded threadId — no probing. An unmanaged codex
    // (no record) falls through to the transcript path below.
    if profile.name == "codex" {
        if let Some(app_runtime) = hooked_codex_app_server_runtime(pane_id) {
            for (key, value) in app_runtime {
                runtime.insert(key, value);
            }
            runtime.insert(
                "sessionId".to_string(),
                Value::from(
                    hooked_cas_session_id_for_pane(pane_id)
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "unresolved".to_string()),
                ),
            );
            return runtime;
        }
    }

    // hive-spawned grok is the same shape over its per-pane leader daemon,
    // and its session id needs no probing: hive minted it at spawn time and
    // wrote it beside the socket. Unlike codex it never falls through to the
    // transcript path — that gate only knows claude/codex record shapes and
    // reads a pending grok permission request as clear — so with no leader
    // state the honest answer is unknown.
    if profile.name == "grok" {
        let leader_runtime = _grok_leader_runtime(pane_id);
        runtime.insert(
            "sessionId".to_string(),
            Value::from(
                hooked_gl_session_id_for_pane(pane_id)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "unresolved".to_string()),
            ),
        );
        match leader_runtime {
            Some(fields) => {
                for (key, value) in fields {
                    runtime.insert(key, value);
                }
            }
            None => {
                runtime.insert("inputState".to_string(), Value::from("unknown"));
                runtime.insert("inputReason".to_string(), Value::from("no_leader_runtime"));
            }
        }
        return runtime;
    }

    let session_id;
    let snapshot_fresh = runtime_snapshot
        .map(|s| !s.sessionId.value.is_empty() && s.sessionId.is_fresh(None))
        .unwrap_or(false);
    if snapshot_fresh {
        let snapshot = runtime_snapshot.unwrap();
        for (key, value) in snapshot.to_runtime_fields(None) {
            runtime.insert(key, value);
        }
        session_id = snapshot.sessionId.value.clone();
    } else {
        session_id = adapter
            .resolve_current_session_id(pane_id)
            .unwrap_or_default();
        let source = if session_id.is_empty() { "" } else { "adapter" };
        runtime.insert(
            "sessionId".to_string(),
            Value::from(if session_id.is_empty() {
                "unresolved".to_string()
            } else {
                session_id.clone()
            }),
        );
        if !session_id.is_empty() {
            let snapshot = runtime_snapshots()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .update_session_id(
                    pane_id,
                    &session_id,
                    source,
                    None,
                    Some(_SESSION_SNAPSHOT_FRESHNESS_S),
                );
            for (key, value) in snapshot.to_runtime_fields(None) {
                runtime.insert(key, value);
            }
        }
    }

    // An interactive claude reports its own state in the session registry —
    // the same fields the bg engine path maps. It is the authority when it
    // speaks: the transcript gate can only see an AskUserQuestion record, so
    // it reads every other wait (and a stale ask) wrong, and the send gate
    // refuses on that verdict.
    if profile.name == "claude" {
        if let Some((status, waiting_for)) =
            hooked_cs_session_status(hooked_claude_pid_for_pane(pane_id))
        {
            for (key, value) in
                crate::adapters::claude_sessions::runtime_from_status(&status, &waiting_for)
            {
                runtime.insert(key, value);
            }
            runtime.insert("_runtimeSource".to_string(), Value::from("claude_registry"));
            return runtime;
        }
    }

    if session_id.is_empty() {
        runtime.insert("inputState".to_string(), Value::from("unknown"));
        runtime.insert("inputReason".to_string(), Value::from("no_session"));
        return runtime;
    }

    let cwd_hint = hooked_display_value(pane_id, "#{pane_current_path}");
    let transcript = adapter.find_session_file(&session_id, cwd_hint.as_deref());
    runtime.insert(
        "_transcript".to_string(),
        match transcript.as_ref() {
            Some(path) => Value::from(path.to_string_lossy().to_string()),
            None => Value::Null,
        },
    );
    let Some(transcript) = transcript else {
        runtime.insert("inputState".to_string(), Value::from("unknown"));
        runtime.insert("inputReason".to_string(), Value::from("transcript_missing"));
        return runtime;
    };

    let exists = transcript.exists();
    runtime.insert("_transcriptExists".to_string(), Value::Bool(exists));
    if !exists {
        runtime.insert("inputState".to_string(), Value::from("unknown"));
        runtime.insert("inputReason".to_string(), Value::from("transcript_missing"));
        return runtime;
    }

    runtime.insert(
        "_transcriptSize".to_string(),
        Value::from(fs::metadata(&transcript).map(|m| m.len()).unwrap_or(0)),
    );
    let gate = hooked_check_input_gate(&transcript);
    runtime.insert("_gate".to_string(), Value::from(gate.status));
    runtime.insert("_gateReason".to_string(), Value::from(gate.reason.clone()));
    if gate.status == "waiting" {
        runtime.insert("inputState".to_string(), Value::from("waiting_user"));
        runtime.insert("inputReason".to_string(), Value::from("ask_pending"));
    } else if gate.status == "clear" {
        runtime.insert("inputState".to_string(), Value::from("ready"));
        runtime.insert("inputReason".to_string(), Value::from(""));
    } else {
        runtime.insert("inputState".to_string(), Value::from("unknown"));
        runtime.insert(
            "inputReason".to_string(),
            Value::from(if gate.reason.is_empty() {
                "read_error".to_string()
            } else {
                gate.reason
            }),
        );
    }
    runtime
}

/// Runtime of a claude member whose engine is a live interactive session
/// (a joined desktop Claude), or None when *session_id* names none.
///
/// Such a member has no bg job: its registry status is the runtime and its
/// channel liveness is the pulse.
pub fn _claude_session_runtime(session_id: &str) -> Option<Map<String, Value>> {
    let live = hooked_cs_list_sessions()
        .into_iter()
        .find(|s| s.session_id == session_id)?;
    let mut fields = Map::new();
    fields.insert("cliAlive".to_string(), Value::Bool(true));
    fields.insert("sessionId".to_string(), Value::from(session_id));
    fields.insert("_runtimeSource".to_string(), Value::from("claude_session"));
    match hooked_cs_session_status(Some(live.pid)) {
        Some((status, waiting_for)) => {
            for (key, value) in
                crate::adapters::claude_sessions::runtime_from_status(&status, &waiting_for)
            {
                fields.insert(key, value);
            }
        }
        None => {
            fields.insert("busy".to_string(), Value::Bool(false));
            fields.insert("inputState".to_string(), Value::from("ready"));
            fields.insert("inputReason".to_string(), Value::from(""));
        }
    }
    Some(fields)
}

/// Runtime for a registry member with no pane: the engine IS the member.
///
/// ``alive`` mirrors engine liveness (there is no pane to be alive), and
/// ``headless`` marks the row so consumers can tell a closed display from a
/// dead member.
pub fn _headless_member_runtime(agent: &Agent) -> Map<String, Value> {
    let mut runtime = Map::new();
    runtime.insert("alive".to_string(), Value::Bool(false));
    runtime.insert("headless".to_string(), Value::Bool(true));
    runtime.insert("busy".to_string(), Value::Bool(false));
    let sid = agent.session_id.clone().unwrap_or_default();
    let cli = agent.cli.as_str();
    if cli == "claude" && !sid.is_empty() {
        let mut job_rt = _claude_job_runtime(&sid, "");
        if job_rt.get("cliAlive") != Some(&Value::Bool(true)) {
            if let Some(session_rt) = _claude_session_runtime(&sid) {
                job_rt = session_rt;
            }
        }
        for (key, value) in job_rt {
            runtime.insert(key, value);
        }
    } else if cli == "codex" && !sid.is_empty() {
        match hooked_cas_runtime_for_thread(&sid) {
            None => {
                runtime.insert("cliAlive".to_string(), Value::Bool(false));
                runtime.insert("inputState".to_string(), Value::from("unknown"));
                runtime.insert("inputReason".to_string(), Value::from("no_daemon_runtime"));
            }
            Some(rt) => {
                let input_state = if rt.input_state.is_empty() {
                    "ready".to_string()
                } else {
                    rt.input_state.clone()
                };
                runtime.insert("cliAlive".to_string(), Value::Bool(true));
                runtime.insert("busy".to_string(), Value::Bool(rt.busy));
                runtime.insert("turnPhase".to_string(), Value::from(rt.turn_phase.clone()));
                runtime.insert("inputState".to_string(), Value::from(input_state.clone()));
                runtime.insert(
                    "inputReason".to_string(),
                    Value::from(if input_state != "waiting_user" {
                        ""
                    } else {
                        "app_server_active_flag"
                    }),
                );
                runtime.insert(
                    "_runtimeSource".to_string(),
                    Value::from("codex_app_server"),
                );
            }
        }
        runtime.insert("sessionId".to_string(), Value::from(sid));
    } else if cli == "grok" {
        let key = crate::adapters::grok_leader::member_key(&agent.team_name, &agent.name);
        match hooked_gl_runtime_for_key(&key) {
            None => {
                runtime.insert("cliAlive".to_string(), Value::Bool(false));
                runtime.insert("inputState".to_string(), Value::from("unknown"));
                runtime.insert("inputReason".to_string(), Value::from("no_leader_runtime"));
            }
            Some(rt) => {
                let input_state = if rt.input_state.is_empty() {
                    "ready".to_string()
                } else {
                    rt.input_state.clone()
                };
                runtime.insert("cliAlive".to_string(), Value::Bool(true));
                runtime.insert("busy".to_string(), Value::Bool(rt.busy));
                runtime.insert("turnPhase".to_string(), Value::from(rt.turn_phase.clone()));
                runtime.insert("inputState".to_string(), Value::from(input_state.clone()));
                runtime.insert(
                    "inputReason".to_string(),
                    Value::from(if input_state != "waiting_user" {
                        ""
                    } else {
                        "leader_permission_request"
                    }),
                );
                runtime.insert("_runtimeSource".to_string(), Value::from("grok-leader"));
            }
        }
        let record = hooked_gl_read_session_key(&key);
        let record_sid = record.map(|(sid, _)| sid).unwrap_or_default();
        let final_sid = if !record_sid.is_empty() {
            record_sid
        } else if !sid.is_empty() {
            sid
        } else {
            "unresolved".to_string()
        };
        runtime.insert("sessionId".to_string(), Value::from(final_sid));
    } else {
        runtime.insert("cliAlive".to_string(), Value::Bool(false));
        runtime.insert("inputState".to_string(), Value::from("unknown"));
        runtime.insert("inputReason".to_string(), Value::from("no_engine_identity"));
    }
    let alive = runtime.get("cliAlive") == Some(&Value::Bool(true));
    runtime.insert("alive".to_string(), Value::Bool(alive));
    runtime
}

pub fn _member_runtime_payload_impl(pane_id: &str, role: &str) -> Map<String, Value> {
    if role != "agent" {
        let mut payload = Map::new();
        payload.insert(
            "alive".to_string(),
            Value::Bool(hooked_is_pane_alive(pane_id)),
        );
        for (key, value) in hooked_busy_output_payload(pane_id) {
            payload.insert(key, value);
        }
        return payload;
    }
    let snapshot = runtime_snapshots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(pane_id)
        .cloned();
    _agent_runtime_payload(pane_id, snapshot.as_ref())
}

/// Overlay the engine's own runtime on a claude member whose pane is only a
/// mirror of it.
///
/// `hive render` renders an interactive claude member — a joined desktop
/// session — as a read-only `hive view` pane. No CLI process runs on that
/// tty, so the pane-keyed probe reports `cli_exited`; but for claude the
/// pane tty is never the evidence (see docs/runtime-model.md), the engine
/// is. The roster sessionId is that engine's identity: while it names a live
/// session, that session's runtime is the member's. `alive` stays the pane's
/// own fact.
fn _mirror_pane_runtime(agent: &Agent, mut runtime: Map<String, Value>) -> Map<String, Value> {
    if agent.cli != "claude" || runtime.get("cliAlive") == Some(&Value::Bool(true)) {
        return runtime;
    }
    let sid = agent.session_id.clone().unwrap_or_default();
    if sid.is_empty() {
        return runtime;
    }
    let Some(session_rt) = _claude_session_runtime(&sid) else {
        return runtime;
    };
    for (key, value) in session_rt {
        runtime.insert(key, value);
    }
    runtime
}

pub fn _team_runtime_payload(team_name: &str) -> Result<Map<String, Value>> {
    let team = hooked_team_load(team_name)?;
    let mut members = Map::new();
    let mut needs_answer: Vec<String> = Vec::new();

    if let Some(lead) = team.lead_agent() {
        let role = hooked_member_role_for_pane(&lead.pane_id);
        let runtime = hooked_member_runtime_payload(&lead.pane_id, role);
        if runtime.get("inputState").and_then(Value::as_str) == Some("waiting_user") {
            needs_answer.push(lead.name.clone());
        }
        members.insert(lead.name.clone(), Value::Object(runtime));
    }

    let mut sorted_agents: Vec<&Agent> = team.agents.iter().collect();
    sorted_agents.sort_by(|a, b| a.name.cmp(&b.name));
    for agent in sorted_agents {
        let runtime = if !agent.pane_id.is_empty() {
            _mirror_pane_runtime(
                agent,
                hooked_member_runtime_payload(&agent.pane_id, "agent"),
            )
        } else {
            _headless_member_runtime(agent)
        };
        if runtime.get("inputState").and_then(Value::as_str) == Some("waiting_user") {
            needs_answer.push(agent.name.clone());
        }
        members.insert(agent.name.clone(), Value::Object(runtime));
    }

    let mut payload = Map::new();
    payload.insert("ok".to_string(), Value::Bool(true));
    payload.insert("team".to_string(), Value::from(team_name));
    payload.insert("members".to_string(), Value::Object(members));
    if !needs_answer.is_empty() {
        payload.insert(
            "needsAnswer".to_string(),
            Value::Array(needs_answer.into_iter().map(Value::from).collect()),
        );
    }
    Ok(payload)
}

pub fn _runtime_snapshot_payload(pane_id: &str) -> Map<String, Value> {
    if pane_id.is_empty() {
        return err_response("pane required");
    }
    let snapshot = runtime_snapshots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(pane_id)
        .cloned();
    let mut payload = Map::new();
    payload.insert("ok".to_string(), Value::Bool(true));
    payload.insert("pane".to_string(), Value::from(pane_id));
    payload.insert(
        "snapshot".to_string(),
        match snapshot {
            Some(s) => Value::Object(s.to_runtime_fields(None)),
            None => Value::Null,
        },
    );
    payload
}

pub fn _team_member_bindings_impl(team_name: &str) -> Result<Vec<(String, Map<String, Value>)>> {
    let team = hooked_team_load(team_name)?;
    let mut members: Vec<(String, Map<String, Value>)> = Vec::new();
    let mut upsert = |name: String, row: Map<String, Value>| match members
        .iter_mut()
        .find(|(n, _)| *n == name)
    {
        Some(slot) => slot.1 = row,
        None => members.push((name, row)),
    };

    if let Some(lead) = team.lead_agent() {
        let mut row = Map::new();
        row.insert("name".to_string(), Value::from(lead.name.clone()));
        row.insert(
            "role".to_string(),
            Value::from(hooked_member_role_for_pane(&lead.pane_id)),
        );
        row.insert("pane".to_string(), Value::from(lead.pane_id.clone()));
        row.insert("cli".to_string(), Value::from(lead.cli.clone()));
        upsert(lead.name.clone(), row);
    }

    let mut sorted_agents: Vec<&Agent> = team.agents.iter().collect();
    sorted_agents.sort_by(|a, b| a.name.cmp(&b.name));
    for agent in sorted_agents {
        let mut row = Map::new();
        row.insert("name".to_string(), Value::from(agent.name.clone()));
        row.insert("role".to_string(), Value::from("agent"));
        row.insert("pane".to_string(), Value::from(agent.pane_id.clone()));
        row.insert("cli".to_string(), Value::from(agent.cli.clone()));
        upsert(agent.name.clone(), row);
    }

    Ok(members)
}

pub fn _idle_notify_agent_panes_impl(team_name: &str) -> Vec<String> {
    let bindings = hooked_team_member_bindings(team_name).unwrap_or_default();
    let mut panes: Vec<String> = Vec::new();
    for (_, member) in bindings {
        if member.get("role").and_then(Value::as_str) != Some("agent") {
            continue;
        }
        let pane_id = map_get_str(&member, "pane");
        if !pane_id.is_empty()
            && !panes.contains(&pane_id)
            && hooked_is_pane_alive(&pane_id)
            && hooked_detect_cli_process_for_pane(&pane_id).is_some()
        {
            panes.push(pane_id);
        }
    }
    panes
}
