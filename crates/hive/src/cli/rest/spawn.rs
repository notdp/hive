use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

use super::*;
use crate::agent::Agent;
use crate::team::{Team, LEAD_AGENT_NAME};
use crate::tmux;

// ---------------------------------------------------------------------------
// spawn
// ---------------------------------------------------------------------------

/// Spawn a member with no pane: engine first, registry as its existence.
#[allow(clippy::too_many_arguments)]
fn _spawn_headless_member(
    t: &mut Team,
    team_name: &str,
    agent_name: &str,
    model: &str,
    prompt: &str,
    cwd: &str,
    skill: &str,
    env_entries: &[String],
    cli_name: Option<&str>,
) -> Result<Agent> {
    let resolved_cli = match cli_name {
        Some(cli) if crate::agent_cli::AGENT_CLI_NAMES.contains(&cli) => cli.to_string(),
        _ => "claude".to_string(),
    };
    if let Some(model_error) = crate::agent_cli::validate_spawn_model(&resolved_cli, model) {
        bail!("{model_error}");
    }
    if agent_name == "flow" || agent_name.starts_with("flow.") {
        bail!(
            "'{agent_name}' collides with the flow runner's mailbox address kind (flow.run), not a member name"
        );
    }
    if t.agent_named(agent_name).is_some() {
        bail!("Agent '{agent_name}' already exists in team '{}'", t.name);
    }
    let resolved_cwd = if cwd.is_empty() {
        getcwd()
    } else {
        expanduser(cwd)
    };
    let extra_env: Map<String, Value> = if env_entries.is_empty() {
        Map::new()
    } else {
        _parse_entries(env_entries)
    };

    let profile = crate::agent_cli::get_profile(&resolved_cli);
    let mut initial_prompt = String::new();
    if !skill.is_empty() && skill != "none" {
        let skill_ref = if resolved_cli == "claude" {
            skill.to_string()
        } else {
            skill
                .rsplit_once(':')
                .map(|(_, rest)| rest.to_string())
                .unwrap_or_else(|| skill.to_string())
        };
        initial_prompt = match profile {
            Some(profile) => profile.skill_cmd_for(&skill_ref),
            None => format!("/{skill_ref}"),
        };
        // The skill takes the team as its argument — one entry form for
        // spawn bootstrap and manual joins alike.
        initial_prompt = format!("{initial_prompt} {team_name}");
    }
    if !prompt.is_empty() {
        initial_prompt = if initial_prompt.is_empty() {
            prompt.to_string()
        } else {
            format!("{initial_prompt}\n\n{prompt}")
        };
    }

    let mut session_id = String::new();
    if resolved_cli == "claude" {
        use crate::adapters::claude_bg;
        let mut extra_args: Vec<String> = Vec::new();
        if !model.is_empty() {
            extra_args.push("--model".to_string());
            extra_args.push(model.to_string());
        }
        let mut env: HashMap<String, String> = HashMap::new();
        env.insert("HIVE_TEAM".to_string(), team_name.to_string());
        env.insert("HIVE_MEMBER".to_string(), agent_name.to_string());
        for (key, value) in &extra_env {
            env.insert(key.clone(), value_as_env_string(value));
        }
        let job_id = claude_bg::spawn_job(
            &resolved_cwd,
            &format!("{team_name}.{agent_name}"),
            &initial_prompt,
            &extra_args,
            Some(&env),
            "claude",
        );
        let job_id = match job_id {
            Some(job_id) if !job_id.is_empty() => job_id,
            _ => bail!(
                "`claude --bg` returned no usable job id for '{agent_name}'; \
                 refusing to register a member without a job identity"
            ),
        };
        if claude_bg::wait_engine_entry(&job_id, crate::agent::AGENT_STARTUP_TIMEOUT).is_none() {
            claude_bg::stop_job(&job_id, "claude");
            bail!(
                "claude job '{job_id}' started but its engine never \
                 registered an inbox; refusing an undeliverable member"
            );
        }
        session_id = job_id;
    } else if resolved_cli == "codex" {
        use crate::adapters::codex_app_server;
        if !codex_app_server::spawn_daemon() {
            bail!("codex shared app-server daemon failed to start");
        }
        let _ = codex_app_server::ensure_dir_trusted(&resolved_cwd);
        let thread_id = codex_app_server::start_member_thread(
            &resolved_cwd,
            &format!("{team_name}.{agent_name}"),
            model,
        );
        let thread_id = match thread_id {
            Some(thread_id) if !thread_id.is_empty() => thread_id,
            _ => bail!("codex app-server refused to mint a thread for '{agent_name}'"),
        };
        if !initial_prompt.is_empty()
            && codex_app_server::send_to_thread(&thread_id, &initial_prompt).is_none()
        {
            bail!("codex thread '{thread_id}' refused the bootstrap turn");
        }
        session_id = thread_id;
    } else if resolved_cli == "grok" {
        if !model.is_empty() {
            bail!(
                "headless grok spawn cannot pick a model yet (the TUI flag \
                 has no verified ACP equivalent); omit --model"
            );
        }
        use crate::adapters::grok_leader;
        session_id = uuid4();
        if !grok_leader::create_member_session(team_name, agent_name, &session_id, &resolved_cwd) {
            bail!("grok leader for '{agent_name}' did not materialize the session");
        }
        if !initial_prompt.is_empty()
            && grok_leader::send_to_key(
                &grok_leader::member_key(team_name, agent_name),
                &initial_prompt,
            )
            .is_none()
        {
            grok_leader::kill_daemon_key(&grok_leader::member_key(team_name, agent_name));
            bail!("grok member '{agent_name}' refused the bootstrap prompt");
        }
    }

    let agent = Agent {
        name: agent_name.to_string(),
        team_name: team_name.to_string(),
        pane_id: String::new(),
        model: model.to_string(),
        prompt: String::new(),
        cwd: resolved_cwd,
        session_id: if session_id.is_empty() {
            None
        } else {
            Some(session_id)
        },
        spawned_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
        cli: resolved_cli,
    };
    t.upsert_agent(agent.clone());
    let ws = resolve_workspace(Some(&*t), false).unwrap_or_default();
    _remember_context(team_name, &ws, LEAD_AGENT_NAME);
    _registry_record_member(t, &agent);
    Ok(agent)
}

#[allow(clippy::too_many_arguments)]
pub fn spawn(
    agent_name: &str,
    model: &str,
    prompt: &str,
    cwd: &str,
    skill: &str,
    env: &[String],
    cli_name: Option<&str>,
    task_artifact: Option<&str>,
    team_arg: &str,
) {
    if task_artifact.is_some() && !prompt.is_empty() {
        fail("--task and --prompt are mutually exclusive (the task rides the message, not the birth prompt)");
    }
    let (team_name, t) = ok_or_fail(resolve_scoped_team(Some(team_arg), true));
    let team_name = team_name.expect("required resolve returned no team");
    let mut t = t.expect("required resolve returned no team");
    // A live display and a tmux-resident caller get a pane; anything else —
    // a ccd orch outside tmux, a team with no window — spawns engine-only.
    let headless = !(!t.tmux_window.is_empty() && tmux::is_inside_tmux());
    let use_prompt = if task_artifact.is_some() { "" } else { prompt };
    let use_skill = if task_artifact.is_some() {
        "hive:hive"
    } else {
        skill
    };
    let spawned: Result<Agent> = if headless {
        _spawn_headless_member(
            &mut t, &team_name, agent_name, model, use_prompt, cwd, use_skill, env, cli_name,
        )
    } else {
        let entries: Map<String, Value> = if env.is_empty() {
            Map::new()
        } else {
            _parse_entries(env)
        };
        let pairs: Vec<(String, String)> = entries
            .iter()
            .map(|(key, value)| (key.clone(), value_as_env_string(value)))
            .collect();
        spawn_team_agent(
            &mut t, &team_name, agent_name, model, use_prompt, cwd, use_skill, &pairs, cli_name,
        )
        .map(|agent| agent.clone())
    };
    let agent = match spawned {
        Ok(agent) => agent,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let Some(task_artifact) = task_artifact else {
        if !agent.pane_id.is_empty() {
            println!("Agent '{agent_name}' spawned in pane {}", agent.pane_id);
        } else {
            println!(
                "Agent '{agent_name}' spawned headless (engine only — `hive attach {team_name}` renders it)"
            );
        }
        return;
    };

    let workspace = ok_or_fail(resolve_workspace(Some(&t), true));
    let _ = _ensure_team_hived(&mut t, &workspace);
    if agent.cli != "claude" {
        // A claude member's inbox is a queue: the task can land while the
        // bootstrap turn is still running and waits its turn. Only CLIs
        // whose delivery injects into a live TUI need the ready gate.
        let agents: HashSet<String> = [agent_name.to_string()].into_iter().collect();
        let not_ready = wait_for_peer_ready(&workspace, &team_name, &agents, 30.0, 0.5);
        if !not_ready.is_empty() {
            println!(
                "{}",
                py_dumps(
                    &json!({
                        "status": "spawn_ready_timeout",
                        "agent": agent_name,
                        "pane": agent.pane_id,
                        "hint": "pane spawned but did not reach ready within 30s; dispatch manually via `hive send`",
                    }),
                    true,
                    Some(2),
                    false
                )
            );
            std::process::exit(1);
        }
    }

    let task_path = std::fs::canonicalize(task_artifact)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| task_artifact.to_string());
    let task_name = Path::new(&task_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let sender = _resolve_sender(None);
    let dispatch = request_send_payload(
        &workspace,
        &t,
        &sender,
        agent_name,
        &format!("task dispatch: {task_name}"),
        &task_path,
        "",
        "spawn-dispatch",
        false,
    );
    if let Err(exc) = dispatch {
        println!(
            "{}",
            py_dumps(
                &json!({
                    "status": "dispatch_failed",
                    "agent": agent_name,
                    "pane": agent.pane_id,
                    "error": exc.to_string(),
                    "hint": format!("member is ready but dispatch failed; retry: hive send {agent_name} ... --artifact {task_path}"),
                }),
                true,
                Some(2),
                false
            )
        );
        std::process::exit(1);
    }
    println!(
        "{}",
        py_dumps(
            &json!({
                "agent": agent_name,
                "pane": agent.pane_id,
                "task": task_path,
                "dispatched": true,
            }),
            true,
            Some(2),
            false
        )
    );
}

// ---------------------------------------------------------------------------
// flow
// ---------------------------------------------------------------------------

/// Flow scripts are trusted JavaScript (you or your orch wrote them),
/// evaluated by the embedded engine in `crate::flow_script` — no external
/// interpreter, no materialized client.
pub fn flow_run_cmd(script: &str, resume: Option<&str>) {
    let script_path = std::fs::canonicalize(script)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| script.to_string());
    std::process::exit(crate::flow_script::run_cmd(&script_path, resume));
}
