use std::collections::HashMap;

use anyhow::bail;

use crate::adapters::claude_bg::EngineSession;
use crate::agent_cli::AGENT_CLI_NAMES;

use super::seams::*;
use super::support::*;

/// Spawn options beyond name/team/target pane.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    pub model: String,
    pub prompt: String,
    pub cwd: String,
    pub session_id: Option<String>,
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
    pub cwd: String,
    pub session_id: Option<String>,
    pub cli: String,
}

impl Agent {
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
        if !AGENT_CLI_NAMES.contains(&cli) {
            bail!(
                "unsupported cli '{}', must be one of: {}",
                cli,
                AGENT_CLI_NAMES.join(", ")
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
        // Outside tmux, a concrete target pane still addresses the shared
        // tmux server (targeted commands need no $TMUX) — that is how an
        // external orchestrator (`hive flow node --team`) spawns visible
        // members. Only an un-addressable spawn has to be inside tmux.
        if !hooked_is_inside_tmux() && target_pane.is_empty() {
            bail!("{}", TMUX_REQUIRED_MESSAGE);
        }

        let initial_prompt = compose_spawn_prompt(cli, &opts, team_name)?;

        let pane_id = open_member_pane(name, team_name, target_pane, &opts)?;

        // The pane runs hive's managed launcher (`hive claude` / `hive codex` /
        // `hive grok`), the same path a human's `hclaude` / `hcodex` / `hgrok`
        // takes — but invoked as the binary, not the shell function, so a spawn
        // never depends on the pane shell's rc having sourced `hive shell-init`.
        // No `exec`: the CLI runs as the pane shell's foreground child, so the
        // pane (and a usable shell) survives the CLI exiting.
        let mut cmd_parts: Vec<String> = vec!["hive".to_string(), cli.to_string()];
        let mut grok_session_id = String::new();
        let mint = _MintContext {
            name,
            team_name,
            pane_id: &pane_id,
            cwd: &cwd,
            opts: &opts,
            initial_prompt: &initial_prompt,
        };
        if cli == "claude" {
            cmd_parts.extend(mint.mint_claude()?);
        } else if cli == "codex" {
            cmd_parts.extend(mint.mint_codex()?);
        } else if cli == "grok" {
            let (parts, sid) = mint.mint_grok()?;
            cmd_parts.extend(parts);
            grok_session_id = sid;
        }

        // codex/grok take the composed prompt as the launch's positional arg
        // (codex rides `resume`'s own [PROMPT] positional); claude's already
        // went into the bg spawn.
        if !initial_prompt.is_empty() && cli != "claude" {
            cmd_parts.push(shell_escape(&initial_prompt));
        }

        let mut env_parts: Vec<String> = Vec::new();
        if let Some(extra) = &opts.extra_env {
            for (k, v) in extra {
                env_parts.push(format!("{k}={}", shell_escape(v)));
            }
        }

        let mut cmd = format!("cd {}", shell_escape(&cwd));
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
            cwd: cwd.clone(),
            session_id: opts.session_id.clone(),
            cli: cli.to_string(),
        };

        // Readiness comes from runtime signals, not screen text: the codex
        // TUI process on the pane TTY, and for grok the member's session
        // directory plus a grok process on the pane, can only be there once
        // the agent is actually up. A claude member needs no wait at all —
        // its engine entry was proven before the pane command was even
        // typed, and the pane only watches.
        if cli == "codex" {
            hooked_wait_codex_attached(&pane_id);
        } else if cli == "grok" {
            // Client order on the session: the mint's stdio client bound
            // it first (and stays in this process's pool while the pane
            // comes up), the pane's TUI `--resume` is the next, and the
            // hived's own client connects only once the TUI is up, so its
            // session/load replay does not race the TUI's.
            if hooked_wait_grok_session_ready(&pane_id, &grok_session_id)
                && !opts.workspace.is_empty()
            {
                hooked_request_connect_grok(&opts.workspace, &pane_id);
            }
        }

        Ok(agent)
    }
}

/// Skill activation + optional user prompt: the text every CLI takes as its
/// positional `[prompt]` arg (also on resume/fork) and auto-submits at
/// startup, bypassing TUI keystroke injection entirely. Shared by every
/// spawn path.
pub fn compose_initial_prompt(cli: &str, skill: &str, prompt: &str, team_name: &str) -> String {
    let mut initial_prompt = String::new();
    if !skill.is_empty() && skill != "none" {
        // claude addresses plugin skills fully qualified (/hive:hive);
        // codex and grok register them by bare skill name ($hive, /hive).
        let skill_ref = if cli == "claude" {
            skill
        } else {
            skill.rsplit(':').next().unwrap_or("")
        };
        initial_prompt = match crate::agent_cli::get_profile(cli) {
            Some(p) => p.skill_cmd_for(skill_ref),
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
    initial_prompt
}

/// The pane spawn's prompt: a claude member's goes into the bg spawn
/// itself, codex/grok pass it on the launch command line.
fn compose_spawn_prompt(cli: &str, opts: &SpawnOptions, team_name: &str) -> anyhow::Result<String> {
    let initial_prompt = compose_initial_prompt(cli, &opts.skill, &opts.prompt, team_name);
    // The launch goes through `hive <cli>`, whose parser strips any `--`
    // separator, so a prompt cannot be protected from being read as a
    // flag; refuse the one shape that would be.
    if initial_prompt.starts_with('-') {
        bail!("initial prompt must not start with '-'");
    }
    Ok(initial_prompt)
}

/// Mint a claude member's engine: a `claude --bg` job spawned with the
/// bootstrap prompt under *label* (`<team>.<member>`), proven deliverable
/// by waiting for its inbox entry. A job whose engine never registers is
/// stopped again. Returns the job id and the engine entry.
pub fn mint_claude_job(
    cwd: &str,
    label: &str,
    initial_prompt: &str,
    extra_args: &[String],
    extra_env: &HashMap<String, String>,
) -> anyhow::Result<(String, EngineSession)> {
    let job_id = match hooked_spawn_job(cwd, label, initial_prompt, extra_args, extra_env) {
        Some(jid) if !jid.is_empty() => jid,
        _ => bail!(
            "`claude --bg` returned no usable job id for '{label}' \
             (it failed, or announced one hive could not read); \
             cwd {cwd}. Refusing to spawn a claude member \
             without a job identity (needs a Claude Code with \
             background sessions, 2.1.240+)"
        ),
    };
    match hooked_wait_engine_entry(&job_id, AGENT_STARTUP_TIMEOUT) {
        Some(engine) => Ok((job_id, engine)),
        None => {
            hooked_stop_job(&job_id);
            bail!(
                "claude job '{job_id}' started but its engine \
                 never registered an inbox; claude delivery is \
                 inbox-only, refusing to keep an undeliverable member"
            );
        }
    }
}

/// Codex runtime state is daemon-native only (embedded codex is
/// unsupported): bring the shared app-server daemon up and trust *cwd*
/// before any thread is started or resumed on it.
pub fn ensure_codex_daemon(cwd: &str) -> anyhow::Result<()> {
    if !hooked_codex_spawn_daemon() {
        bail!(
            "codex shared app-server daemon failed to start; \
             codex runtime is daemon-only, refusing to spawn an \
             embedded codex team member"
        );
    }
    hooked_ensure_dir_trusted(cwd)
}

/// Mint a codex member's thread on the shared daemon (thread/start +
/// name/set flush) under *label* (`<team>.<member>`); returns the thread
/// id, which is the member's session id.
pub fn mint_codex_thread(cwd: &str, label: &str, model: &str) -> anyhow::Result<String> {
    ensure_codex_daemon(cwd)?;
    match hooked_start_member_thread(cwd, label, model) {
        Some(tid) if !tid.is_empty() => Ok(tid),
        _ => bail!(
            "codex app-server refused to mint a thread for \
             '{label}' (cwd {cwd}); refusing to spawn a codex \
             member without a thread identity"
        ),
    }
}

/// Split (or take over) the target pane and tag it as the member's.
fn open_member_pane(
    name: &str,
    team_name: &str,
    target_pane: &str,
    opts: &SpawnOptions,
) -> anyhow::Result<String> {
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
            let _ = crate::layout::ensure(&window_for_tile, false);
        }
        pane_id
    } else {
        target_pane.to_string()
    };
    hooked_set_pane_title(&pane_id, &format!("[{name}]"));
    hooked_tag_pane(&pane_id, "agent", name, team_name, &opts.cli);
    Ok(pane_id)
}

/// Everything a per-CLI engine mint reads; each mint returns the launch
/// arguments to append after `hive <cli>` (grok also hands back the
/// member's session id — minted on the leader, or the branch id a fork's
/// TUI will create — for the readiness wait).
struct _MintContext<'a> {
    name: &'a str,
    team_name: &'a str,
    pane_id: &'a str,
    cwd: &'a str,
    opts: &'a SpawnOptions,
    initial_prompt: &'a str,
}

impl _MintContext<'_> {
    /// Give the pane back after a daemon failure: a split pane is ours
    /// to kill, an in-place one only loses the tags/title just written.
    fn undo_pane_side_effects(&self) {
        if self.opts.split_window {
            hooked_kill_pane(self.pane_id);
        } else {
            hooked_clear_pane_tags(self.pane_id);
            hooked_set_pane_title(self.pane_id, "");
        }
    }

    /// A claude member is a `claude --bg` job: the engine runs on claude's
    /// own supervisor, the pane only watches it through the managed
    /// launcher's attach loop. The job is minted (or woken) up front — like
    /// codex's thread — so the member has a durable identity and a
    /// deliverable inbox before the pane even draws.
    fn mint_claude(&self) -> anyhow::Result<Vec<String>> {
        let opts = self.opts;
        let (name, team_name, cwd) = (self.name, self.team_name, self.cwd);
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
                    self.undo_pane_side_effects();
                    bail!(
                        "claude job '{claude_job_id}' did not come back up \
                         (removed from the job ledger, or the wake failed); \
                         cannot resume this member"
                    );
                }
            }
            if !self.initial_prompt.is_empty() {
                // Resume carries no launch prompt; hand it over on the
                // daemon reply lane, inbox as fallback (best-effort).
                if hooked_daemon_reply(&engine.session_id, self.initial_prompt).is_none() {
                    // The frame's `from` is the human's message-card
                    // label: hive is speaking here, not the member
                    // being resumed, so the team is the origin.
                    hooked_claude_sessions_send(
                        &engine.socket_path,
                        self.initial_prompt,
                        team_name,
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
            // Identity is never handed to the engine in env: it mints
            // its own session id, and the roster row keyed by it says
            // who the engine is.
            let env_map: HashMap<String, String> = opts
                .extra_env
                .iter()
                .flatten()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            match mint_claude_job(
                cwd,
                &format!("{team_name}.{name}"),
                self.initial_prompt,
                &extra_args,
                &env_map,
            ) {
                Ok((jid, e)) => {
                    claude_job_id = jid;
                    engine = e;
                }
                Err(err) => {
                    self.undo_pane_side_effects();
                    return Err(err);
                }
            }
        }
        hooked_write_pane_job(self.pane_id, &claude_job_id, &engine.session_id, cwd)?;
        // The managed launcher recognizes a jobId and runs the attach
        // watch loop (auto-reattach across engine respawns/upgrades).
        Ok(vec!["--resume".to_string(), shell_escape(&claude_job_id)])
    }

    /// Every codex member runs on the shared app-server daemon and owns
    /// exactly one thread. A new member's thread is minted by hive up front
    /// (thread/start + name/set flush), a resumed member's thread is its
    /// recorded sessionId (== threadId), and the TUI attaches with `resume
    /// <threadId>` through the managed launcher (which injects
    /// --remote/--cd).
    fn mint_codex(&self) -> anyhow::Result<Vec<String>> {
        let opts = self.opts;
        let (name, team_name, cwd) = (self.name, self.team_name, self.cwd);
        let mut parts = vec![
            "-c".to_string(),
            "check_for_update_on_startup=false".to_string(),
        ];
        if opts.session_id.is_some() && opts.session_mode == "fork" {
            // The managed launcher forks server-side (`hive codex fork
            // <sid>` → thread/fork → resume of the fork) and records the
            // pane's thread itself; nothing to mint here.
            parts.push("fork".to_string());
            parts.push(shell_escape(opts.session_id.as_deref().unwrap_or("")));
            return Ok(parts);
        }
        // A daemon failure gives the pane back rather than leaving a
        // tagged inert member behind.
        let minted = match &opts.session_id {
            // session_mode == "resume": the thread is the recorded sessionId.
            Some(sid) => ensure_codex_daemon(cwd).map(|_| sid.clone()),
            None => mint_codex_thread(cwd, &format!("{team_name}.{name}"), &opts.model),
        };
        let codex_thread_id = match minted {
            Ok(tid) => tid,
            Err(err) => {
                self.undo_pane_side_effects();
                return Err(err);
            }
        };
        hooked_write_pane_thread(self.pane_id, &codex_thread_id, cwd)?;
        parts.push("resume".to_string());
        parts.push(shell_escape(&codex_thread_id));
        // Bring the hived's client online now so it holds the
        // broadcast stream before the member's first turn.
        // Best-effort: a down/slow hived just falls back to the
        // lazy connect on the next runtime tick.
        if !opts.workspace.is_empty() {
            hooked_request_connect_codex(&opts.workspace);
        }
        Ok(parts)
    }

    /// A grok member's engine is its leader daemon on `m-<team>.<member>`,
    /// minted by identity before the pane runs anything: a fresh member
    /// gets `session/new` with hive's minted id (the leader cannot say which
    /// of the cwd's sessions is the member's, so hive names it and records
    /// it beside the socket), and the pane then attaches to that session as
    /// one more leader client — `hive grok --resume <sid>`, the same attach
    /// form claude (`--resume <jobId>`) and codex (`resume <threadId>`)
    /// take. A resume keeps the resumed session's own id; a fork has no
    /// leader-side primitive, so the TUI branches it (`--session-id <new>
    /// --resume <old> --fork-session`) on the identity-keyed leader. Unlike
    /// claude and codex, grok takes the model on the launch line. Returns
    /// the launch arguments and the member's session id.
    fn mint_grok(&self) -> anyhow::Result<(Vec<String>, String)> {
        let opts = self.opts;
        let (name, team_name, cwd) = (self.name, self.team_name, self.cwd);
        let key = crate::adapters::grok_leader::member_key(team_name, name);
        let mut parts: Vec<String> = Vec::new();
        let grok_session_id = match (&opts.session_id, opts.session_mode.as_str()) {
            (None, _) => {
                let sid = uuid4();
                if !hooked_grok_create_member_session(team_name, name, &sid, cwd) {
                    // Grok runtime state lives on the member's leader;
                    // without a materialized session the TUI would run
                    // detached from hive. Same deal as codex: give the pane
                    // back rather than tag an unreachable member.
                    self.undo_pane_side_effects();
                    bail!(
                        "grok leader for '{team_name}.{name}' did not materialize \
                         the session (cwd {cwd}); grok runtime is leader-only, \
                         refusing to spawn an unattached grok team member"
                    );
                }
                parts.push("--resume".to_string());
                parts.push(sid.clone());
                if !opts.model.is_empty() {
                    parts.push("-m".to_string());
                    parts.push(shell_escape(&opts.model));
                }
                sid
            }
            (Some(old), mode) => {
                if !hooked_grok_spawn_member_daemon(team_name, name) {
                    self.undo_pane_side_effects();
                    bail!(
                        "grok leader daemon failed to start for '{team_name}.{name}'; \
                         grok runtime is leader-only, refusing to spawn an \
                         unattached grok team member"
                    );
                }
                let sid = if mode == "resume" {
                    old.clone()
                } else {
                    uuid4()
                };
                hooked_grok_write_session_key(&key, &sid, cwd)?;
                if mode != "resume" {
                    // `--session-id` names the branch the TUI creates.
                    parts.push("--session-id".to_string());
                    parts.push(sid.clone());
                }
                // Resume/fork uses the original session's model.
                parts.push("--resume".to_string());
                parts.push(shell_escape(old));
                if mode != "resume" {
                    parts.push("--fork-session".to_string());
                }
                sid
            }
        };
        Ok((parts, grok_session_id))
    }
}
