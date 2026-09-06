use std::collections::HashSet;
use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};

use super::*;
use crate::team::Team;

// ---------------------------------------------------------------------------
// spawn
// ---------------------------------------------------------------------------

/// Workspace the `--task` dispatch will ride, or None without `--task`.
///
/// Split out so the requirement is checked before the spawn: the dispatch
/// needs a workspace, and discovering that after the member is registered
/// and its engine minted leaves a half-born member on the roster.
pub(crate) fn task_dispatch_workspace(
    t: &Team,
    task_artifact: Option<&str>,
) -> Result<Option<String>> {
    match task_artifact {
        Some(_) => resolve_workspace(Some(t), true).map(Some),
        None => Ok(None),
    }
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
    // Before any spawn side effect: a `--task` spawn that cannot resolve its
    // workspace must fail while the roster is still clean, not after the
    // member is registered and its engine minted.
    let task_workspace = ok_or_fail(task_dispatch_workspace(&t, task_artifact));
    if t.tmux_window.is_empty() {
        // The display is gone (server restart, window closed by hand):
        // rebuild it before splitting.
        let entry = ok_or_fail(
            crate::registry::load(&team_name)
                .ok_or_else(|| anyhow!("team '{team_name}' has no registry entry (deleted?)")),
        );
        let _ = ensure_team_display(&entry);
        // re-resolve: the anchor is now the new window's first pane
        t = ok_or_fail(load_team(&team_name, ""));
    }
    let use_prompt = if task_artifact.is_some() { "" } else { prompt };
    let use_skill = if task_artifact.is_some() {
        "hive:hive"
    } else {
        skill
    };
    let entries: Map<String, Value> = if env.is_empty() {
        Map::new()
    } else {
        parse_entries(env)
    };
    let pairs: Vec<(String, String)> = entries
        .iter()
        .map(|(key, value)| (key.clone(), value_as_env_string(value)))
        .collect();
    let agent = match spawn_team_agent(
        &mut t, &team_name, agent_name, model, use_prompt, cwd, use_skill, &pairs, cli_name,
    )
    .cloned()
    {
        Ok(agent) => agent,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let Some(task_artifact) = task_artifact else {
        println!("Agent '{agent_name}' spawned in pane {}", agent.pane_id);
        return;
    };

    let workspace = task_workspace.expect("--task resolved its workspace before spawning");
    let _ = start_team_hived(&mut t, &workspace);
    if agent.cli != "claude" {
        // A claude member's inbox is a queue: the task can land while the
        // bootstrap turn is still running and waits its turn. Only CLIs
        // whose delivery injects into a live TUI need the ready gate.
        let agents: HashSet<String> = [agent_name.to_string()].into_iter().collect();
        let not_ready = wait_for_peer_ready(&workspace, &team_name, &agents, 30.0, 0.5);
        if !not_ready.is_empty() {
            println!(
                "{}",
                json_pretty(&json!({
                    "status": "spawn_ready_timeout",
                    "agent": agent_name,
                    "pane": agent.pane_id,
                    "hint": "pane spawned but did not reach ready within 30s; dispatch manually via `hive send`",
                }))
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
    let sender = resolve_sender(None);
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
            json_pretty(&json!({
                "status": "dispatch_failed",
                "agent": agent_name,
                "pane": agent.pane_id,
                "error": exc.to_string(),
                "hint": format!("member is ready but dispatch failed; retry: hive send {agent_name} ... --artifact {task_path}"),
            }))
        );
        std::process::exit(1);
    }
    println!(
        "{}",
        json_pretty(&json!({
            "agent": agent_name,
            "pane": agent.pane_id,
            "task": task_path,
            "dispatched": true,
        }))
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

/// `hive flow node run`: the task is stdin (no shell quoting to get wrong),
/// progress goes to stderr, the single JSON result to stdout.
pub fn flow_node_run_cmd(
    name: &str,
    cli: Option<&str>,
    model: &str,
    phase: &str,
    team: Option<&str>,
) {
    let mut task = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut task).is_err()
        || task.trim().is_empty()
    {
        fail("flow node run reads the task from stdin — pipe or heredoc the task text");
    }
    let env = crate::flow::RealEnv::for_team(team.map(str::to_string));
    let spec = crate::flow::NodeSpec {
        name: name.to_string(),
        cli: cli.map(str::to_string),
        model: model.to_string(),
        phase: phase.to_string(),
        task: task.trim_end().to_string(),
    };
    match crate::flow::run_node(&env, &spec) {
        Ok(result) => println!("{}", serde_json::Value::Object(result)),
        Err(e) => fail(&e.0),
    }
}
