use std::collections::HashMap;

use anyhow::bail;

use crate::adapters::claude_bg::EngineSession;

use super::seams::*;
use super::support::*;

/// Keyword arguments of Python `Agent.spawn` beyond name/team/target pane.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    pub model: String,
    pub prompt: String,
    pub cwd: String,
    pub session_id: Option<String>,
    pub is_first: bool,
    pub split_horizontal: bool,
    pub split_size: Option<String>,
    pub split_window: bool,
    pub skill: String,
    pub extra_env: Option<Vec<(String, String)>>,
    pub cli: String,
    pub workspace: String,
    pub session_mode: String,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        SpawnOptions {
            model: String::new(),
            prompt: String::new(),
            cwd: String::new(),
            session_id: None,
            is_first: false,
            split_horizontal: true,
            split_size: None,
            split_window: true,
            skill: "hive".to_string(),
            extra_env: None,
            cli: "claude".to_string(),
            workspace: String::new(),
            session_mode: "fork".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Agent {
    pub name: String,
    pub team_name: String,
    pub pane_id: String,
    pub model: String,
    pub prompt: String,
    pub cwd: String,
    pub session_id: Option<String>,
    pub spawned_at: f64,
    pub cli: String,
}

impl Agent {
    /// Python dataclass defaults: model/prompt empty, cwd = os.getcwd(),
    /// spawned_at = now, cli = "claude".
    pub fn new(name: &str, team_name: &str, pane_id: &str) -> Agent {
        Agent {
            name: name.to_string(),
            team_name: team_name.to_string(),
            pane_id: pane_id.to_string(),
            model: String::new(),
            prompt: String::new(),
            cwd: std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            session_id: None,
            spawned_at: now_epoch(),
            cli: "claude".to_string(),
        }
    }

    // --- Lifecycle ---

    /// Spawn an agent CLI (claude/codex/grok) in a tmux pane.
    ///
    /// If split_window is true (default), splits *target_pane* and runs the
    /// CLI in the new pane. If false, runs the CLI in *target_pane* itself
    /// (target must be a shell pane, not already running an agent).
    ///
    /// With a *session_id*, *session_mode* picks the semantics: `fork`
    /// (default, existing behavior) branches a copy of the session; `resume`
    /// continues it — claude drops `--fork-session`, codex runs the
    /// daemon-native `resume` subcommand (a resumed team member is
    /// first-class, so the embedded shortcut fork uses is not allowed), grok
    /// drops `--fork-session` and keeps the resumed session's own id.
    pub fn spawn(
        name: &str,
        team_name: &str,
        target_pane: &str,
        opts: SpawnOptions,
    ) -> anyhow::Result<Agent> {
        let cli = opts.cli.as_str();
        if !SUPPORTED_CLIS.contains(&cli) {
            bail!(
                "unsupported cli '{}', must be one of: {}",
                cli,
                SUPPORTED_CLIS.join(", ")
            );
        }
        if opts.session_mode != "fork" && opts.session_mode != "resume" {
            bail!(
                "unsupported session_mode '{}', must be fork or resume",
                opts.session_mode
            );
        }
        let cwd = if opts.cwd.is_empty() {
            std::env::current_dir()?.to_string_lossy().to_string()
        } else {
            opts.cwd.clone()
        };
        if !hooked_is_inside_tmux() {
            bail!("{}", _TMUX_REQUIRED_MESSAGE);
        }

        let profile = crate::agent_cli::get_profile(cli);

        let pane_id = if opts.split_window {
            let pane_id = hooked_split_window(
                target_pane,
                opts.split_horizontal,
                opts.split_size.as_deref(),
            )?;
            // Re-tile the moment the pane exists — the CLI boot below can
            // block for tens of seconds, and a 50% split left un-tiled that
            // long is the distorted-window the human stares at.
            let window_for_tile = hooked_get_pane_window_target(&pane_id);
            if !window_for_tile.is_empty() {
                crate::layout::apply_adaptive(&window_for_tile);
            }
            pane_id
        } else {
            target_pane.to_string()
        };
        hooked_set_pane_title(&pane_id, &format!("[{name}]"));
        hooked_tag_pane(&pane_id, "agent", name, team_name, cli);

        // Give the pane back after a daemon failure: a split pane is ours
        // to kill, an in-place one only loses the tags/title just written.
        let undo_pane_side_effects = |pane_id: &str| {
            if opts.split_window {
                hooked_kill_pane(pane_id);
            } else {
                hooked_clear_pane_tags(pane_id);
                hooked_set_pane_title(pane_id, "");
            }
        };

        // Every CLI accepts a positional [prompt] arg (also on resume/fork).
        // Skill activation + optional user prompt are composed here, before
        // the CLI branches: a claude member's prompt goes into the bg spawn
        // itself, codex/grok pass it on the launch command line — either way
        // the CLI auto-submits at startup, bypassing TUI keystroke injection
        // entirely.
        let mut initial_prompt = String::new();
        if !opts.skill.is_empty() && opts.skill != "none" {
            // claude addresses plugin skills fully qualified (/hive:hive);
            // codex and grok register them by bare skill name ($hive, /hive).
            let skill_ref = if cli == "claude" {
                opts.skill.clone()
            } else {
                opts.skill.rsplit(':').next().unwrap_or("").to_string()
            };
            initial_prompt = match &profile {
                Some(p) => p.skill_cmd.replace("{name}", &skill_ref),
                None => format!("/{skill_ref}"),
            };
            // The skill takes the team as its argument — one entry form for
            // spawn bootstrap and manual joins alike.
            initial_prompt = format!("{initial_prompt} {team_name}");
        }
        if !opts.prompt.is_empty() {
            initial_prompt = if initial_prompt.is_empty() {
                opts.prompt.clone()
            } else {
                format!("{initial_prompt}\n\n{}", opts.prompt)
            };
        }
        // The launch goes through `hive <cli>`, whose parser strips any `--`
        // separator, so a prompt cannot be protected from being read as a
        // flag; refuse the one shape that would be.
        if initial_prompt.starts_with('-') {
            bail!("initial prompt must not start with '-'");
        }

        // The pane runs hive's managed launcher (`hive claude` / `hive codex` /
        // `hive grok`), the same path a human's `hclaude` / `hcodex` / `hgrok`
        // takes — but invoked as the binary, not the shell function, so a spawn
        // never depends on the pane shell's rc having sourced `hive shell-init`.
        // No `exec`: the CLI runs as the pane shell's foreground child, so the
        // pane (and a usable shell) survives the CLI exiting.
        let mut cmd_parts: Vec<String> = vec!["hive".to_string(), cli.to_string()];
        let mut grok_session_id = String::new();
        if cli == "claude" {
            // A claude member is a `claude --bg` job: the engine runs on
            // claude's own supervisor, the pane only watches it through the
            // managed launcher's attach loop. The job is minted (or woken)
            // up front — like codex's thread — so the member has a durable
            // identity and a deliverable inbox before the pane even draws.
            let claude_job_id: String;
            let engine: EngineSession;
            if opts.session_id.is_some() && opts.session_mode == "resume" {
                // The member IS the job: attach wakes a parked/stopped
                // engine with the same jobId/sessionId, so resume is just
                // rebinding the pane to it.
                claude_job_id = opts.session_id.clone().unwrap_or_default();
                match hooked_ensure_engine(&claude_job_id, Some(AGENT_STARTUP_TIMEOUT)) {
                    Some(e) => engine = e,
                    None => {
                        undo_pane_side_effects(&pane_id);
                        bail!(
                            "claude job '{claude_job_id}' did not come back up \
                             (removed from the job ledger, or the wake failed); \
                             cannot resume this member"
                        );
                    }
                }
                if !initial_prompt.is_empty() {
                    // Resume carries no launch prompt; hand it over on the
                    // daemon reply lane, inbox as fallback (best-effort).
                    if hooked_daemon_reply(&engine.session_id, &initial_prompt).is_none() {
                        hooked_claude_sessions_send(
                            &engine.socket_path,
                            &initial_prompt,
                            &format!("{team_name}.{name}"),
                            &engine.session_id,
                        );
                    }
                }
            } else {
                let mut extra_args: Vec<String> = Vec::new();
                if !opts.model.is_empty() {
                    extra_args.push("--model".to_string());
                    extra_args.push(opts.model.clone());
                }
                if let Some(sid) = &opts.session_id {
                    // session_mode == "fork": branch a copy
                    extra_args.push("-r".to_string());
                    extra_args.push(sid.clone());
                    extra_args.push("--fork-session".to_string());
                }
                // The engine's env carries the member identity so its tool
                // subprocesses can resolve who they are without a pane.
                let mut env_map: HashMap<String, String> = HashMap::new();
                env_map.insert("HIVE_TEAM".to_string(), team_name.to_string());
                env_map.insert("HIVE_MEMBER".to_string(), name.to_string());
                if let Some(extra) = &opts.extra_env {
                    for (k, v) in extra {
                        env_map.insert(k.clone(), v.clone());
                    }
                }
                let job_id = hooked_spawn_job(
                    &cwd,
                    &format!("{team_name}.{name}"),
                    &initial_prompt,
                    &extra_args,
                    &env_map,
                );
                match job_id {
                    Some(jid) if !jid.is_empty() => claude_job_id = jid,
                    _ => {
                        undo_pane_side_effects(&pane_id);
                        bail!(
                            "`claude --bg` returned no usable job id for '{name}' \
                             (it failed, or announced one hive could not read); \
                             cwd {cwd}. Refusing to spawn a claude member \
                             without a job identity (needs a Claude Code with \
                             background sessions, 2.1.240+)"
                        );
                    }
                }
                match hooked_wait_engine_entry(&claude_job_id, AGENT_STARTUP_TIMEOUT) {
                    Some(e) => engine = e,
                    None => {
                        hooked_stop_job(&claude_job_id);
                        undo_pane_side_effects(&pane_id);
                        bail!(
                            "claude job '{claude_job_id}' started but its engine \
                             never registered an inbox; claude delivery is \
                             inbox-only, refusing to keep an undeliverable member"
                        );
                    }
                }
            }
            hooked_write_pane_job(&pane_id, &claude_job_id, &engine.session_id, &cwd)?;
            // The managed launcher recognizes a jobId and runs the attach
            // watch loop (auto-reattach across engine respawns/upgrades).
            cmd_parts.push("--resume".to_string());
            cmd_parts.push(_shell_escape(&claude_job_id));
        } else if cli == "codex" {
            cmd_parts.push("-c".to_string());
            cmd_parts.push("check_for_update_on_startup=false".to_string());
            if opts.session_id.is_some() && opts.session_mode == "fork" {
                // The managed launcher forks server-side (`hive codex fork
                // <sid>` → thread/fork → resume of the fork) and records the
                // pane's thread itself; nothing to mint here.
                cmd_parts.push("fork".to_string());
                cmd_parts.push(_shell_escape(opts.session_id.as_deref().unwrap_or("")));
            } else {
                // Every codex member runs on the shared app-server daemon and
                // owns exactly one thread. A new member's thread is minted by
                // hive up front (thread/start + name/set flush), a resumed
                // member's thread is its recorded sessionId (== threadId), and
                // the TUI attaches with `resume <threadId>` through the
                // managed launcher (which injects --remote/--cd).
                if !hooked_codex_spawn_daemon() {
                    // Codex runtime state is daemon-native only (embedded codex
                    // is unsupported), so a pane without a daemon would join the
                    // team stateless. Undo the pane side effects instead of
                    // leaving a tagged inert member behind.
                    undo_pane_side_effects(&pane_id);
                    bail!(
                        "codex shared app-server daemon failed to start; \
                         codex runtime is daemon-only, refusing to spawn an \
                         embedded codex team member"
                    );
                }
                hooked_ensure_dir_trusted(&cwd)?;
                let codex_thread_id: String;
                if let Some(sid) = &opts.session_id {
                    // session_mode == "resume"
                    codex_thread_id = sid.clone();
                } else {
                    match hooked_start_member_thread(
                        &cwd,
                        &format!("{team_name}.{name}"),
                        &opts.model,
                    ) {
                        Some(tid) if !tid.is_empty() => codex_thread_id = tid,
                        _ => {
                            undo_pane_side_effects(&pane_id);
                            bail!(
                                "codex app-server refused to mint a thread for \
                                 '{name}' (cwd {cwd}); refusing to spawn a codex \
                                 member without a thread identity"
                            );
                        }
                    }
                }
                hooked_write_pane_thread(&pane_id, &codex_thread_id, &cwd)?;
                cmd_parts.push("resume".to_string());
                cmd_parts.push(_shell_escape(&codex_thread_id));
                // Bring the hived's client online now so it holds the
                // broadcast stream before the member's first turn.
                // Best-effort: a down/slow hived just falls back to the
                // lazy connect on the next runtime tick.
                if !opts.workspace.is_empty() {
                    hooked_request_connect_codex(&opts.workspace);
                }
            }
        } else if cli == "grok" {
            if !hooked_grok_spawn_daemon(&pane_id) {
                // Grok runtime state lives on the per-pane leader; without one
                // the TUI would run detached from hive. Same deal as codex: give
                // the pane back rather than tag an unreachable member.
                undo_pane_side_effects(&pane_id);
                bail!(
                    "grok leader daemon failed to start for pane {pane_id}; \
                     grok runtime is leader-only, refusing to spawn an \
                     unattached grok team member"
                );
            }
            // The leader cannot say which of the cwd's sessions is this pane's,
            // so hive mints the id, hands it to the TUI and records it beside
            // the socket. A resume keeps the resumed session's own id.
            if opts.session_id.is_some() && opts.session_mode == "resume" {
                grok_session_id = opts.session_id.clone().unwrap_or_default();
            } else {
                grok_session_id = _uuid4();
                cmd_parts.push("--session-id".to_string());
                cmd_parts.push(grok_session_id.clone());
            }
            hooked_write_pane_session(&pane_id, &grok_session_id, &cwd)?;
        }

        // claude pins model/resume/prompt at bg-spawn time and codex at
        // thread/start; only grok takes them on the launch command line.
        if cli == "grok" {
            if !opts.model.is_empty() && opts.session_id.is_none() {
                cmd_parts.push("-m".to_string());
                cmd_parts.push(_shell_escape(&opts.model));
            }
            if let Some(sid) = &opts.session_id {
                // Resume/fork uses the original session's model.
                cmd_parts.push("--resume".to_string());
                cmd_parts.push(_shell_escape(sid));
                if opts.session_mode == "fork" {
                    // `--session-id` (already on cmd_parts) names the fork.
                    cmd_parts.push("--fork-session".to_string());
                }
            }
        }

        // codex/grok take the composed prompt as the launch's positional arg
        // (codex rides `resume`'s own [PROMPT] positional); claude's already
        // went into the bg spawn.
        if !initial_prompt.is_empty() && cli != "claude" {
            cmd_parts.push(_shell_escape(&initial_prompt));
        }

        let mut env_parts: Vec<String> = Vec::new();
        if let Some(extra) = &opts.extra_env {
            for (k, v) in extra {
                env_parts.push(format!("{k}={}", _shell_escape(v)));
            }
        }

        let mut cmd = format!("cd {}", _shell_escape(&cwd));
        if !env_parts.is_empty() {
            cmd = format!("{cmd} && export {}", env_parts.join(" "));
        }
        // After the CLI exits the pane keeps its shell, so print the cd-ready
        // resume hint there — the same tail `hclaude` / `hcodex` run.
        cmd = format!(
            "{cmd} && {}; hive resume-hint {cli} 2>/dev/null || true",
            cmd_parts.join(" ")
        );
        hooked_send_keys(&pane_id, &cmd, true)?;

        let agent = Agent {
            name: name.to_string(),
            team_name: team_name.to_string(),
            pane_id: pane_id.clone(),
            model: opts.model.clone(),
            prompt: opts.prompt.clone(),
            cwd: cwd.clone(),
            session_id: opts.session_id.clone(),
            spawned_at: now_epoch(),
            cli: cli.to_string(),
        };

        // Readiness comes from runtime signals, not screen text: the codex TUI
        // process on the pane TTY and the minted session directory (grok) can
        // only appear once the agent is actually up. A claude member needs no
        // wait at all — its engine entry was proven before the pane command
        // was even typed, and the pane only watches.
        if cli == "codex" {
            hooked_wait_codex_attached(&pane_id);
        } else if cli == "grok" {
            // The 2nd client can only load a session the TUI has opened, so the
            // connect follows readiness instead of racing it.
            if hooked_wait_grok_session_ready(&pane_id, &grok_session_id)
                && !opts.workspace.is_empty()
            {
                hooked_request_connect_grok(&opts.workspace, &pane_id);
            }
        }

        Ok(agent)
    }
}
