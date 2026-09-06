//! Member verbs: `spawn`, `send`, `kill`, `interrupt`, `thread`, `capture`,
//! `inject`, `compact`, `view`.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};

use super::util::{
    fail, json_pretty, ok_or_fail, parse_entries, resolve_artifact_path, resolve_sender,
    value_as_env_string,
};
use crate::identity;
use crate::json_fields::is_set;
use crate::team::{
    live_member_pids, load_team, resolve_scoped_team, resolve_workspace, spawn_team_agent,
    start_team_hived, Team,
};

/// Send a message to another agent — the only message verb.
pub(crate) fn send(to_agent: &str, body: &str, artifact: &str) {
    if let Some(label) = to_agent.strip_prefix("ccd.") {
        send_to_ccd_session(label, body, artifact);
        return;
    }
    // A dot splits the address only when the prefix names an existing team
    // (`honey.worker`); otherwise the address stays whole for qualified-name
    // resolution across pane tags.
    let (explicit_team, to_agent) = crate::send::split_team_address(to_agent);
    // The root gate admitted this call because the process runs inside a
    // Claude session (that session is the sender and its inbox socket is its
    // identity), or a codex/grok member's tool whose own session id keys
    // its roster row. The latter take the member lane, where the identity
    // ladder's session rung resolves them.
    let guest = if identity::is_inside_tmux() {
        None
    } else {
        crate::adapters::claude_sessions::self_session()
    };
    let (t, sender) = if let Some(guest) = guest {
        let (_team_name, t) = ok_or_fail(crate::send::resolve_guest_send_target(
            &to_agent,
            &explicit_team,
        ));
        let sender = match crate::registry::member_for_session(&guest.session_id, None) {
            // A joined session is a full member: its roster name is the
            // reply address, not the ccd guest label.
            Some((m_team, m_name)) => format!("{m_team}.{m_name}"),
            // The session NAME, never the title: a title may contain spaces,
            // which would break `<HIVE from=...>` attribute tokenization
            // downstream. The name addresses the session in
            // `hive send ccd.<name>` just the same.
            None => format!("ccd.{}", guest.name),
        };
        (t, sender)
    } else {
        if !explicit_team.is_empty()
            && explicit_team != identity::default_team().unwrap_or_default()
        {
            // Copying a teammate's `from=<team>.<member>` verbatim must just
            // work, so an own-team prefix reads as the bare name; only a
            // foreign-team prefix is refused.
            fail(
                "team members address teammates by bare name; \
                 `<team>.<member>` is for a Claude session outside tmux",
            );
        }
        let (_team_name, t) = ok_or_fail(crate::send::resolve_send_target_team(&to_agent));
        (t, resolve_sender(None))
    };
    let ws = ok_or_fail(resolve_workspace(Some(&t), true));
    if body.trim().is_empty() {
        fail("message body required");
    }
    let resolved_artifact = resolve_artifact_path(artifact, &ws);
    let payload = match crate::send::request_send_payload(
        &ws,
        &t,
        &sender,
        &to_agent,
        body,
        &resolved_artifact,
        "send",
        true,
    ) {
        Ok(payload) => payload,
        Err(e) => fail(&e.to_string()),
    };
    if is_set(payload.get("mailbox")) {
        // A mailbox has no peer runtime to go silent about: say so once,
        // in the sender's own tool result, so nobody invents a follow-up.
        println!("delivered to flow mailbox (not a member; no ack will arrive)");
    }
    // Peer sends stay silent (rule of silence).
}

/// `hive send ccd.<session>`: a member pushes into an outside Claude
/// session's cross-session inbox.
fn send_to_ccd_session(label: &str, message: &str, artifact: &str) {
    let team = identity::default_team();
    let agent = identity::default_agent();
    let (team, agent) = match (team, agent) {
        (Some(team), Some(agent)) => (team, agent),
        _ => fail(
            "`ccd.<session>` is a team member's outbound address; another \
             Claude session is messaged with the native SendMessage tool",
        ),
    };
    if !artifact.is_empty() {
        fail("a session push carries no --artifact; put the path in the body");
    }
    if message.is_empty() {
        fail("message body required");
    }
    let matches = crate::adapters::claude_sessions::resolve(label);
    if matches.is_empty() {
        fail(&format!(
            "no live Claude session named, titled or numbered '{label}' (see `hive ccd ls`)"
        ));
    }
    if matches.len() > 1 {
        let where_ = matches
            .iter()
            .map(|s| {
                format!(
                    "{} (pid {}, {})",
                    s.name,
                    s.pid,
                    if s.cwd.is_empty() { "?" } else { &s.cwd }
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        fail(&format!(
            "{} live sessions answer to '{label}': {where_}; use the name or pid",
            matches.len()
        ));
    }
    let target = &matches[0];
    if let Some((m_team, m_agent)) = live_member_pids().get(&target.pid) {
        if *m_team == team {
            fail(&format!(
                "'{label}' is your teammate {m_agent}; members talk over \
                 the bus: `hive send {m_agent}`"
            ));
        }
        fail(&format!(
            "'{label}' is {m_team}.{m_agent}, a member of another team, not an outside session"
        ));
    }
    let sender = format!("{team}.{agent}");
    // The frame's `from` reaches only the human's message card; the receiving
    // model sees just the text. Wrap the body in the ordinary <HIVE> envelope
    // so the sender travels in band and the receiver answers by copying it
    // verbatim: `hive send <team>.<agent>`. Not a bus thread.
    let envelope =
        crate::message::format_hive_envelope(&sender, &format!("ccd.{}", target.name), message, "");
    let outcome = crate::adapters::claude_sessions::send(
        &target.socket_path,
        &envelope,
        &sender,
        &target.session_id,
    );
    match outcome {
        None => fail(&format!(
            "session '{}' (pid {}) is not listening on {}; it may have just exited",
            target.name, target.pid, target.socket_path
        )),
        Some(outcome) if outcome == crate::adapters::claude_sessions::WRITE_TIMED_OUT => {
            fail(&format!(
                "session '{}' (pid {}) accepted the connection but did \
                 not read the message (~{} KB) in time; it looks \
                 stalled and may hold a truncated frame — retry once it is responsive",
                target.name,
                target.pid,
                std::cmp::max(1, message.len() / 1024)
            ))
        }
        // Fire-and-forget: success is silent (rule of silence); failures above
        // already exited non-zero with the reason.
        Some(_) => {}
    }
}

/// Read-only viewer for a Claude session transcript (follows live): the
/// TUI on a terminal, a plain ANSI stream into a pipe.
pub(crate) fn view_cmd(session_id: &str) {
    let Some(path) = crate::transcript_view::transcript_path(session_id) else {
        println!("no transcript for session '{session_id}'");
        std::process::exit(1);
    };
    let code = if unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1 {
        match crate::transcript_tui::run(&path) {
            Ok(code) => code,
            Err(err) => {
                eprintln!("{}: {}", path.display(), err);
                1
            }
        }
    } else {
        crate::transcript_view::follow_plain(session_id, &path)
    };
    std::process::exit(code);
}

/// Interrupt an agent's running turn.
pub(crate) fn interrupt(agent_name: &str) {
    let (_, t) = ok_or_fail(resolve_scoped_team(None, true));
    let t = t.expect("required resolve returned no team");
    let agent = match t.get(agent_name) {
        Ok(agent) => agent,
        Err(_) => fail(&format!(
            "member '{agent_name}' not found in team '{}'",
            t.name
        )),
    };
    if let Err(e) = agent.interrupt() {
        fail(&e.to_string());
    }
    let mut result = Map::new();
    result.insert("member".to_string(), Value::String(agent_name.to_string()));
    result.insert("action".to_string(), Value::String("interrupt".to_string()));
    result.insert("pane".to_string(), Value::String(agent.pane_id.clone()));
    result.insert("success".to_string(), Value::Bool(true));
    println!("{}", json_pretty(&Value::Object(result)));
}

/// (team, bare member) a kill addresses; empty team means "the pane's".
///
/// `-t` is the caller's own intent, so it outranks a team prefix in the
/// address; without it the `<team>.<member>` form still names its own team.
fn kill_address(agent_name: &str, team_arg: &str) -> (String, String) {
    let (address_team, bare_name) = crate::send::split_team_address(agent_name);
    if team_arg.is_empty() {
        (address_team, bare_name)
    } else {
        (team_arg.to_string(), bare_name)
    }
}

/// Kill an agent pane and remove it from the team.
pub(crate) fn kill(agent_name: &str, team_arg: &str) {
    let (explicit_team, bare_name) = kill_address(agent_name, team_arg);
    let (mut t, agent_name) = if !explicit_team.is_empty() {
        (ok_or_fail(load_team(&explicit_team, "")), bare_name)
    } else {
        let (_, t) = ok_or_fail(crate::send::resolve_send_target_team(agent_name));
        (t, agent_name.to_string())
    };
    let agent = match t.get(&agent_name) {
        Ok(agent) => agent,
        Err(_) => fail(&format!("agent '{agent_name}' not found")),
    };
    // Team::retire is the one retirement path (roster + registry + layout).
    let removed_from_team = t.retire(&agent_name);
    let mut result = Map::new();
    result.insert("member".to_string(), Value::String(agent_name));
    result.insert("action".to_string(), Value::String("kill".to_string()));
    result.insert("pane".to_string(), Value::String(agent.pane_id.clone()));
    result.insert(
        "removedFromTeam".to_string(),
        Value::Bool(removed_from_team),
    );
    result.insert("success".to_string(), Value::Bool(true));
    println!("{}", json_pretty(&Value::Object(result)));
}

pub(crate) fn inject_cmd(agent_name: &str, text: &str) {
    let (_, t) = ok_or_fail(resolve_scoped_team(None, true));
    let t = t.expect("required resolve returned no team");
    let result = ok_or_fail(inject_report(&t, agent_name, text));
    println!("{}", json_pretty(&Value::Object(result)));
}

/// Type *text* into the member's composer and describe the delivery.
///
/// Documented low-level bypass: raw composer keystrokes for every CLI, so
/// delivery paths (channel/RPC) can be debugged from outside themselves.
fn inject_report(t: &Team, agent_name: &str, text: &str) -> Result<Map<String, Value>> {
    let agent = t
        .get(agent_name)
        .map_err(|_| anyhow!("member '{agent_name}' not found in team '{}'", t.name))?;
    crate::agent::submit_interactive_text(&agent.pane_id, text, &agent.cli)?;
    let mut result = Map::new();
    result.insert("member".to_string(), Value::String(agent_name.to_string()));
    result.insert("action".to_string(), Value::String("inject".to_string()));
    result.insert("pane".to_string(), Value::String(agent.pane_id.clone()));
    result.insert("success".to_string(), Value::Bool(true));
    Ok(result)
}

/// Run `/compact` on the literal pane. Returns the compaction status.
fn compact_target(target: &crate::agent_cli::PaneTarget) -> String {
    if target.cli == "codex" || target.cli == "grok" {
        // Daemon-backed CLIs: an idle agent compacts via the dedicated RPC;
        // when busy we keystroke `/compact` into the CLI's own TUI so it can
        // refuse visibly instead of a silent background compaction.
        let status = if target.cli == "codex" {
            crate::adapters::codex_app_server::compact_pane(&target.pane_id)
        } else {
            crate::adapters::grok_leader::compact_pane(&target.pane_id)
        };
        if status != "compacted" {
            ok_or_fail(crate::agent::submit_interactive_text(
                &target.pane_id,
                "/compact",
                &target.cli,
            ));
        }
        return status.to_string();
    }
    // claude (and embedded codex without a daemon): `/compact` is a TUI
    // slash command, so it must go through the composer.
    if let Err(exc) =
        crate::agent::submit_interactive_text(&target.pane_id, "/compact", &target.cli)
    {
        fail(&exc.to_string());
    }
    "compacted".to_string()
}

pub(crate) fn compact_cmd(pane_id: &str) {
    // Resolve the pane straight from its tmux options — never re-resolve
    // through Team state (the cross-window same-name bug PR #8 fixed).
    let target = ok_or_fail(crate::agent_cli::resolve_pane_target(pane_id));
    let status = compact_target(&target);
    let mut result = Map::new();
    result.insert(
        "member".to_string(),
        Value::String(target.member_label.clone()),
    );
    result.insert("action".to_string(), Value::String("compact".to_string()));
    result.insert("pane".to_string(), Value::String(target.pane_id.clone()));
    result.insert("status".to_string(), Value::String(status.clone()));
    result.insert("success".to_string(), Value::Bool(status == "compacted"));
    if !target.is_team_bound {
        // Pane-only compact has no team identity; `member` is the pane id.
        result.insert("team".to_string(), Value::Null);
    }
    println!("{}", json_pretty(&Value::Object(result)));
}

pub(crate) fn capture(member_name: &str, lines: i64) {
    let (_, t) = ok_or_fail(resolve_scoped_team(None, true));
    let t = t.expect("required resolve returned no team");
    println!("{}", ok_or_fail(capture_text(&t, member_name, lines)));
}

/// The last *lines* of the member's own pane (the pane its roster row
/// resolved to), or the not-found refusal.
fn capture_text(t: &Team, member_name: &str, lines: i64) -> Result<String> {
    let agent = t
        .get(member_name)
        .map_err(|_| anyhow!("member '{member_name}' not found in team '{}'", t.name))?;
    agent.capture(lines.max(0) as u32)
}

/// Workspace the `--task` dispatch will ride, or None without `--task`.
///
/// Split out so the requirement is checked before the spawn: the dispatch
/// needs a workspace, and discovering that after the member is registered
/// and its engine minted leaves a half-born member on the roster.
fn task_dispatch_workspace(t: &Team, task_artifact: Option<&str>) -> Result<Option<String>> {
    match task_artifact {
        Some(_) => resolve_workspace(Some(t), true).map(Some),
        None => Ok(None),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn(
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
        ok_or_fail(crate::team_display::ensure_team_display(&entry));
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
        let not_ready =
            crate::send::wait_for_peer_ready(&workspace, &team_name, &agents, 30.0, 0.5);
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
    let dispatch = crate::send::request_send_payload(
        &workspace,
        &t,
        &sender,
        agent_name,
        &format!("task dispatch: {task_name}"),
        &task_path,
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

#[cfg(test)]
mod tests {
    use serde_json::{json, Map, Value};

    use super::*;
    use crate::paths::getcwd;
    use crate::testenv::EnvGuard;
    use crate::testkit::{
        args, count, display_env, fake_tmux, fake_tmux_tagged, hived_answering_ping, member_row,
    };

    #[test]
    fn test_kill_address_prefers_the_explicit_team_over_the_prefix() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::TempDir::new().unwrap();
        env.set("HIVE_HOME", tmp.path().join("hive"));
        crate::registry::record_team("hornet", "/tmp/ws-hn", "1.0", &[], "").unwrap();

        // bare name: the pane's team decides, unless -t names one
        assert_eq!(kill_address("ant", ""), (String::new(), "ant".to_string()));
        assert_eq!(
            kill_address("ant", "hornet"),
            ("hornet".to_string(), "ant".to_string())
        );
        // the qualified form keeps working on its own
        assert_eq!(
            kill_address("hornet.ant", ""),
            ("hornet".to_string(), "ant".to_string())
        );
    }

    /// A hived stand-in on the workspace socket: records every request it is
    /// sent and answers each with `{ok: true, seq: <n>}`.
    struct FakeHived {
        path: std::path::PathBuf,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        requests: std::sync::Arc<std::sync::Mutex<Vec<Map<String, Value>>>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeHived {
        fn bind(workspace: &str) -> FakeHived {
            use std::io::{Read, Write};
            let path = crate::hived::socket_path(workspace);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            let _ = std::fs::remove_file(&path);
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let (stop_seen, log) = (
                std::sync::Arc::clone(&stop),
                std::sync::Arc::clone(&requests),
            );
            let thread = std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop_seen.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    let Ok(mut stream) = stream else { break };
                    let mut body = Vec::new();
                    let _ = stream.read_to_end(&mut body);
                    let request: Map<String, Value> = serde_json::from_slice(&body).unwrap();
                    let mut log = log.lock().unwrap();
                    log.push(request);
                    let reply = json!({"ok": true, "seq": log.len()});
                    let _ = stream.write_all(reply.to_string().as_bytes());
                }
            });
            FakeHived {
                path,
                stop,
                requests,
                thread: Some(thread),
            }
        }

        fn requests(&self) -> Vec<Map<String, Value>> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for FakeHived {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            // Wake the accept loop so it sees the flag.
            let _ = std::os::unix::net::UnixStream::connect(&self.path);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn test_spawn_with_a_task_rosters_the_member_on_a_pane_and_dispatches_the_artifact() {
        let env = display_env();
        let ws = env._tmp.path().join("ws").to_string_lossy().into_owned();
        std::fs::create_dir_all(&ws).unwrap();
        let task = env._tmp.path().join("task.md");
        std::fs::write(&task, "review the diff\n").unwrap();
        let task_path = std::fs::canonicalize(&task)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        crate::registry::record_team(
            "honey",
            &ws,
            "100.0",
            &[member_row("orch", "grok", "sid-orch")],
            "@7",
        )
        .unwrap();
        // The team window is up (`Team::load` resolves it through `team/mod.rs`'s
        // fake), so no heal runs. The caller's own pane is orch's, which is who
        // signs the dispatch.
        let argv = fake_tmux_tagged(
            "dev:2\t@7\thoney\t\t\t\n",
            &[],
            &[("%0", "hive-team", "honey"), ("%0", "hive-agent", "orch")],
        );
        let _agent = crate::agent::testhook::install(crate::agent::testhook::Hook::new());
        let _hived = hived_answering_ping("honey");
        let fake_hived = FakeHived::bind(&ws);

        spawn(
            "bee",
            "",
            "",
            "",
            "",
            &[],
            Some("claude"),
            Some(task.to_string_lossy().as_ref()),
            "honey",
        );

        // The member exists in the registry: claude, the spawner's cwd (no
        // --cwd was given). Its identity is the pane→job record the spawn
        // wrote — the roster row reads its sessionId back from that record,
        // which the agent seam captured here instead of writing.
        let entry = crate::registry::load("honey").unwrap();
        let bee = entry["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == "bee")
            .expect("bee on the roster");
        assert_eq!(bee["cli"], Value::from("claude"));
        assert_eq!(bee["cwd"], Value::from(getcwd()));
        let records = crate::agent::testhook::with(|h| h.records.clone()).unwrap();
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].0, "%0");
        assert_eq!(records[0].1, "abcd1234");
        // The engine was minted under the member's label with the bootstrap
        // prompt alone — the task rides the message, not the birth prompt.
        let spawns = crate::agent::testhook::with(|h| h.spawns.clone()).unwrap();
        assert_eq!(spawns.len(), 1, "{spawns:?}");
        assert_eq!(spawns[0].name, "honey.bee");
        assert_eq!(
            spawns[0].prompt,
            crate::agent::compose_initial_prompt("claude", "hive:hive", "", "honey")
        );
        assert!(!spawns[0].prompt.contains(&task_path));
        // One send reached the hived: orch → bee, the artifact being the task
        // file by its canonical path.
        let requests = fake_hived.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        let sent = &requests[0];
        assert_eq!(sent["action"], Value::from("send"));
        assert_eq!(sent["team"], Value::from("honey"));
        assert_eq!(sent["senderAgent"], Value::from("orch"));
        assert_eq!(sent["targetAgent"], Value::from("bee"));
        assert_eq!(sent["body"], Value::from("task dispatch: task.md"));
        assert_eq!(sent["artifact"], Value::from(task_path.as_str()));
        assert!(sent.get("replyTo").is_none());
        // The window was there: no heal. The member's pane came from the agent
        // seam's split echo (it never reaches the tmux facade), tagged for the
        // team.
        assert_eq!(count(&argv, "new-window"), 0);
        let tags = crate::agent::testhook::with(|h| h.tags.clone()).unwrap();
        assert!(
            tags.iter()
                .any(|(_, role, agent, team)| role == "agent" && agent == "bee" && team == "honey"),
            "{tags:?}"
        );
    }

    #[test]
    fn test_spawn_rebuilds_a_missing_window_before_splitting() {
        let env = display_env();
        let ws = env._tmp.path().join("ws").to_string_lossy().into_owned();
        std::fs::create_dir_all(&ws).unwrap();
        // `@3` is the dead window's id: the heal must record the new one.
        crate::registry::record_team(
            "honey",
            &ws,
            "100.0",
            &[member_row("orch", "grok", "sid-orch")],
            "@3",
        )
        .unwrap();
        // No window claims the team: the display is gone (server restart,
        // window closed by hand).
        let argv = fake_tmux_tagged(
            "",
            &[],
            &[("%0", "hive-team", "honey"), ("%0", "hive-agent", "orch")],
        );
        let _agent = crate::agent::testhook::install(crate::agent::testhook::Hook::new());
        let _hived = hived_answering_ping("honey");

        spawn("bee", "", "", "", "", &[], Some("claude"), None, "honey");

        // The heal rebuilt the window first, and the member landed on the
        // roster with the new window's id in the display cache.
        assert_eq!(count(&argv, "new-window"), 1);
        let entry = crate::registry::load("honey").unwrap();
        assert_eq!(entry["display"], Value::from("@7"));
        assert!(entry["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "bee"));
        // The split anchored on the healed window's first pane (`%1`, the pane
        // `new-window` minted), not on the caller's own `%0`: the re-resolve
        // after the heal is what puts the member in the team window.
        let records = crate::agent::testhook::with(|h| h.records.clone()).unwrap();
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].0, "%1");
    }

    /// A rendered team as `Team::load` resolves it: orch and sage (grok) and
    /// bee (claude) each on a pane of the team window. Built in memory — the
    /// registry-plus-window resolution is `team/mod.rs`'s own contract.
    fn rendered_team() -> Team {
        use crate::agent::testhook::fake_agent;
        Team {
            name: "honey".to_string(),
            agents: vec![
                fake_agent("orch", "honey", "%1", "grok"),
                fake_agent("sage", "honey", "%2", "grok"),
                fake_agent("bee", "honey", "%3", "claude"),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn test_inject_types_into_the_members_pane_and_refuses_a_pane_with_no_composer() {
        let _env = display_env();
        let t = rendered_team();
        let _agent = crate::agent::testhook::install(crate::agent::testhook::Hook::new());
        let calls = || crate::agent::testhook::with(|h| std::mem::take(&mut h.calls)).unwrap();

        // A grok member: the text and Enter go to that member's pane.
        crate::agent::testhook::with(|h| h.resolve_profile_name = Some("grok".to_string()));
        let report = inject_report(&t, "sage", "hello sage").unwrap();
        assert_eq!(report["pane"], Value::from("%2"));
        assert_eq!(report["member"], Value::from("sage"));
        assert_eq!(report["success"], Value::Bool(true));
        assert_eq!(calls(), vec!["hello sage", "<Enter>"]);

        // A claude member whose pane has no job record and no interactive
        // claude process (an attach viewer): refused by pane id, nothing typed.
        crate::agent::testhook::with(|h| {
            h.resolve_profile_name = Some("claude".to_string());
            h.job_id_for_pane = None;
            h.interactive_claude_pid = None;
        });
        let err = inject_report(&t, "bee", "hello bee")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no interactive claude process on pane %3"),
            "{err}"
        );
        assert_eq!(calls(), Vec::<String>::new());

        // Not on the roster: named, and no pane is touched.
        let err = inject_report(&t, "ghost", "hello").unwrap_err().to_string();
        assert!(
            err.contains("member 'ghost' not found in team 'honey'"),
            "{err}"
        );
        assert_eq!(calls(), Vec::<String>::new());
    }

    #[test]
    fn test_capture_reads_the_members_own_pane() {
        let _env = display_env();
        let argv = fake_tmux("", &[]);
        let t = rendered_team();

        let text = capture_text(&t, "sage", 40).unwrap();

        // One capture, of sage's pane — not the caller's (%0) nor orch's.
        assert_eq!(
            argv.borrow().as_slice(),
            &[args(&["capture-pane", "-t", "%2", "-p", "-S", "-40"])]
        );
        assert_eq!(text, "");
        let err = capture_text(&t, "ghost", 40).unwrap_err().to_string();
        assert!(
            err.contains("member 'ghost' not found in team 'honey'"),
            "{err}"
        );
        assert_eq!(argv.borrow().len(), 1);
    }

    #[test]
    fn test_task_dispatch_workspace_fails_before_the_spawn_when_none_resolves() {
        let mut env = EnvGuard::cleared(&crate::testenv::IDENTITY_VARS);
        let tmp = tempfile::TempDir::new().unwrap();
        env.set("HIVE_HOME", tmp.path().join(".hive"));
        let workspaceless = Team {
            name: "hornet".to_string(),
            ..Default::default()
        };

        // no --task: nothing is required, nothing is resolved
        assert_eq!(task_dispatch_workspace(&workspaceless, None).unwrap(), None);

        let err = task_dispatch_workspace(&workspaceless, Some("/tmp/task.md"))
            .expect_err("a task dispatch with no workspace must refuse");
        assert!(err.to_string().contains("workspace not found"), "{err}");

        let with_workspace = Team {
            name: "hornet".to_string(),
            workspace: "/tmp/ws-hn".to_string(),
            ..Default::default()
        };
        assert_eq!(
            task_dispatch_workspace(&with_workspace, Some("/tmp/task.md")).unwrap(),
            Some("/tmp/ws-hn".to_string())
        );
    }
}
