//! Agent: an agent CLI instance running in a tmux pane.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::bail;

use crate::adapters::claude_bg::{EngineSession, KeyResult};
use crate::adapters::claude_sessions;

pub const AGENT_STARTUP_TIMEOUT: f64 = 90.0;
const _TMUX_REQUIRED_MESSAGE: &str = "Hive requires tmux. Start or attach to a tmux session first.";

pub const SUPPORTED_CLIS: [&str; 3] = ["claude", "codex", "grok"];

/// Escape a string for safe shell use.
fn _shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn _resolve_session_id_from_runtime(pane_id: &str) -> Option<String> {
    let resolved_pane = if !pane_id.is_empty() {
        pane_id.to_string()
    } else {
        crate::tmux::get_current_pane_id().unwrap_or_default()
    };
    if resolved_pane.is_empty() {
        return None;
    }
    hooked_resolve_session_id_for_pane(&resolved_pane)
}

/// Best-effort lookup for the current pane's agent session ID.
pub fn detect_current_session_id(_cwd: &str, _model: &str, pane_id: &str) -> Option<String> {
    _resolve_session_id_from_runtime(pane_id)
}

/// A native transport (codex daemon / grok leader / claude inbox) did not accept the
/// message. Normal hive delivery never falls back to keystrokes; callers
/// surface this as an explicit submit failure (injectStatus=failed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryError(pub String);

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for DeliveryError {}

/// Submit text to an interactive agent TUI, preserving any pending draft.
pub fn _submit_interactive_text(pane_id: &str, text: &str, cli: &str) -> anyhow::Result<()> {
    let profile_name = _resolve_profile_name(pane_id, cli);
    if profile_name == "claude" {
        let job_id = hooked_job_id_for_pane(pane_id).unwrap_or_default();
        if !job_id.is_empty() {
            // A claude member's keyboard is the job, not the pane: hive pipes
            // the keystrokes into `claude attach <jobId>` itself. Nothing here
            // touches tmux — the pane's viewer is a screen, and what the human
            // has it showing (another session, the panel list, nothing at all)
            // cannot misroute or block a delivery.
            let result = hooked_type_into_job(&job_id, text);
            if !result.ok {
                bail!("claude job {job_id} did not take the text: {}", result.why);
            }
            return Ok(());
        }
        // No job record: an interactive claude TUI on the pane tty, typed at
        // through tmux like any other CLI. Refuse rather than type into the
        // pane shell when that TUI is not running — or into an attach viewer,
        // whose composer belongs to whatever session it is showing.
        if hooked_interactive_claude_pid(pane_id).is_none() {
            bail!("no interactive claude process on pane {pane_id} to receive keystrokes");
        }
    }

    if hooked_is_pane_in_mode(pane_id) {
        hooked_cancel_pane_mode(pane_id);
        hooked_sleep(0.05);
    }

    let buffer_name = _save_and_clear_draft(pane_id, &profile_name);

    hooked_send_keys(pane_id, text, false)?;
    hooked_sleep(0.05);
    hooked_send_key(pane_id, "Enter")?;

    if !buffer_name.is_empty() {
        _restore_draft(pane_id, &profile_name, &buffer_name);
    }
    Ok(())
}

/// Best-effort: if a draft exists, save it to a tmux buffer and clear input.
///
/// Returns the buffer name to restore later, or '' when there is no draft.
/// The composer is only cleared once tmux confirms the draft is in the
/// buffer — a save that did not happen must not cost the user the draft —
/// and a clear that failed halfway still reports its buffer so the restore
/// pastes it back.
fn _save_and_clear_draft(pane_id: &str, profile_name: &str) -> String {
    if !hooked_supported_profile(profile_name) {
        return String::new();
    }
    let buffer_name = format!("hive_draft_{}", pane_id.replace('%', ""));
    let draft_text = match hooked_parse_draft(pane_id, profile_name) {
        Ok(text) => text,
        Err(_) => return String::new(),
    };
    if draft_text.is_empty() {
        return String::new();
    }
    if hooked_load_buffer(&buffer_name, &draft_text).is_err() {
        return String::new();
    }
    if hooked_clear_input(pane_id, profile_name).is_ok() {
        let _ = hooked_wait_input_empty(pane_id, profile_name, 1.0);
    }
    buffer_name
}

fn _restore_draft(pane_id: &str, profile_name: &str, buffer_name: &str) {
    if hooked_wait_input_empty(pane_id, profile_name, 2.0).is_ok() {
        hooked_paste_buffer(buffer_name, pane_id, true);
    }
    hooked_delete_buffer(buffer_name);
}

/// Prefer runtime detection; fall back to the declared cli.
fn _resolve_profile_name(pane_id: &str, cli: &str) -> String {
    #[cfg(test)]
    if let Some(Some(name)) = testhook::with(|h| h.resolve_profile_name.clone()) {
        return name;
    }
    let mut profile = crate::agent_cli::detect_profile_for_pane(pane_id);
    if profile.is_none() && !cli.is_empty() {
        profile = crate::agent_cli::get_profile(cli);
    }
    match profile {
        Some(p) => p.name.to_string(),
        None => cli.to_string(),
    }
}

/// Wait for the codex TUI process to appear on the pane's TTY.
///
/// The pane's thread identity is minted (and recorded) before the launch
/// command runs, so readiness is just the TUI being up — process evidence,
/// no screen scraping. Best-effort like the banner wait it replaces: a
/// timeout is not fatal.
fn _wait_codex_attached(pane_id: &str, timeout: f64, interval: f64) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
    loop {
        if let Some(profile) = hooked_detect_cli_process_for_pane(pane_id) {
            if profile.name == "codex" {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        hooked_sleep(interval);
    }
}

/// Wait for the grok TUI to materialize the session hive minted for it.
///
/// `--session-id` is honoured at startup: grok creates
/// `$GROK_HOME/sessions/<quoted cwd>/<sid>/` before the first prompt, so that
/// directory appearing is the readiness signal — no screen scraping. The cwd
/// segment is grok's own encoding of the pane cwd, so the pane's session is
/// matched by id under any of them. On resume the directory already exists, so
/// the pane's live grok process is required too. Best-effort like the codex
/// thread wait: a timeout is not fatal.
fn _wait_grok_session_ready(pane_id: &str, session_id: &str, timeout: f64, interval: f64) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
    loop {
        let sessions_dir = crate::adapters::grok_leader::grok_home().join("sessions");
        let mut found = false;
        if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
            for entry in entries.flatten() {
                if entry.path().join(session_id).exists() {
                    found = true;
                    break;
                }
            }
        }
        if found {
            if let Some(profile) = hooked_detect_cli_process_for_pane(pane_id) {
                if profile.name == "grok" {
                    return true;
                }
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        hooked_sleep(interval);
    }
}

fn _uuid4() -> String {
    let mut bytes = [0u8; 16];
    let read = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes));
    if read.is_err() {
        // ponytail: sha256(time, pid) fallback — /dev/urandom exists everywhere hive runs
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(format!(
            "{:?}-{}",
            std::time::SystemTime::now(),
            std::process::id()
        ));
        bytes.copy_from_slice(&hasher.finalize()[..16]);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

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

    // --- Control ---

    /// Send a prompt to the agent; return the accepted-transport class.
    ///
    /// Delivery is native-transport-only: codex goes through the shared
    /// daemon's `turn/start` RPC on the member's recorded thread, grok
    /// through its per-pane leader's `session/prompt`, claude through its
    /// session's own inbox socket. None of them touches the composer, and
    /// there is no keystroke fallback on any failure — a transport that did
    /// not accept the message raises `DeliveryError` (callers surface it as
    /// an explicit submit failure). The returned classification names which
    /// transport boundary was crossed (`turnStartAccepted` /
    /// `sessionPromptQueued` / `udsWriteAccepted`); none of them proves the
    /// agent processed the message — that final confirmation only ever comes
    /// from the target's transcript.
    pub fn send(&self, text: &str) -> Result<String, DeliveryError> {
        // A claude member's engine is not on the pane TTY at all: the pane's
        // job record is its address, and a parked engine (supervisor idles
        // jobs after ~1h) is woken in-line — so a probe that sees nothing is
        // still a deliverable claude member. That record is an address only
        // for a member hive spawned as claude, and only while the pane shows
        // no *other* live CLI: a recycled pane id whose member is codex must
        // never route into a stale `hive-pane-<n>.job`, whichever way the
        // probe happens to read that pane.
        if self.pane_id.is_empty() {
            return self._send_headless(text);
        }
        let probe = hooked_detect_cli_process_for_pane(&self.pane_id);
        let profile_name = probe.as_ref().map(|p| p.name.clone()).unwrap_or_default();
        let claude_member =
            self.cli == "claude" && (profile_name.is_empty() || profile_name == "claude");
        if probe.is_none() && !claude_member {
            return Err(DeliveryError(format!(
                "no live CLI process on pane {} (cli_exited): \
                 refusing native transport to a retained shell",
                self.pane_id
            )));
        }
        if claude_member {
            if let Some(job_id) = hooked_job_id_for_pane(&self.pane_id).filter(|j| !j.is_empty()) {
                return self._deliver_claude_job(&job_id, text);
            }
        }
        if profile_name == "codex" {
            return match hooked_codex_send_to_pane(&self.pane_id, text) {
                Some(accepted) => Ok(accepted.to_string()),
                None => Err(DeliveryError(format!(
                    "codex pane {} did not accept the turn \
                     (no recorded thread, daemon down, RPC error, or \
                     connection failure)",
                    self.pane_id
                ))),
            };
        }
        if profile_name == "grok" {
            return match hooked_grok_send_to_pane(&self.pane_id, text) {
                Some(accepted) => Ok(accepted.to_string()),
                None => Err(DeliveryError(format!(
                    "grok pane {} did not accept the prompt \
                     (no leader/session, RPC error, or connection failure)",
                    self.pane_id
                ))),
            };
        }
        if claude_member {
            return Err(DeliveryError(format!(
                "claude pane {} has no bg job record; a hive \
                 claude member runs as a background job (relaunch it with \
                 `hive claude`) — hive does not deliver to a bare claude TUI",
                self.pane_id
            )));
        }
        if profile_name == "claude" {
            return Err(DeliveryError(format!(
                "pane {} shows claude but its member '{}' \
                 is a {} member (recycled pane id, or a stale job \
                 record); hive does not deliver across CLIs",
                self.pane_id, self.name, self.cli
            )));
        }
        Err(DeliveryError(format!(
            "pane {} runs no supported agent CLI \
             (profile={}); hive delivers over \
             native transports only",
            self.pane_id,
            if profile_name.is_empty() {
                "unknown"
            } else {
                &profile_name
            }
        )))
    }

    fn _deliver_claude_job(&self, job_id: &str, text: &str) -> Result<String, DeliveryError> {
        let where_ = if !self.pane_id.is_empty() {
            format!("pane {}", self.pane_id)
        } else {
            "headless".to_string()
        };
        let mut engine = hooked_engine_session_for_job(job_id);
        if engine.is_none() && hooked_job_row(job_id).is_some() {
            // Asleep, not dead: the job ledger still lists it, and a
            // tty-less attach revives the engine (same jobId and
            // sessionId, fresh pid) — then re-read its new entry.
            engine = hooked_ensure_engine(job_id, None);
        }
        let Some(engine) = engine else {
            return Err(DeliveryError(format!(
                "claude job '{job_id}' ({where_}) is gone (removed from the \
                 job ledger, or the wake failed); the message stays on the bus"
            )));
        };
        // Primary lane: the supervisor daemon's reply channel — the
        // typed-keystroke lane, no peer wrapper in any state. Any
        // failure falls back to the inbox socket, which still
        // delivers (wrapped) with today's error semantics.
        if let Some(accepted) = hooked_daemon_reply(&engine.session_id, text) {
            return Ok(accepted.to_string());
        }
        let accepted = hooked_claude_sessions_send(
            &engine.socket_path,
            text,
            &format!("{}.{}", self.team_name, self.name),
            &engine.session_id,
        );
        match accepted {
            Some(a) if a == claude_sessions::WRITE_TIMED_OUT => Err(DeliveryError(format!(
                "claude job '{job_id}' ({where_}) accepted the connection \
                 but did not drain the message in time"
            ))),
            Some(a) => Ok(a.to_string()),
            None => Err(DeliveryError(format!(
                "claude job '{job_id}' ({where_}) is not listening on its \
                 inbox; the message stays on the bus"
            ))),
        }
    }

    /// Deliver to a joined interactive Claude session (no bg job).
    ///
    /// Same two lanes as a job engine: the supervisor reply channel first,
    /// the session's own inbox socket as fallback.
    fn _deliver_claude_session(
        &self,
        session_id: &str,
        text: &str,
    ) -> Result<String, DeliveryError> {
        if let Some(accepted) = hooked_daemon_reply(session_id, text) {
            return Ok(accepted.to_string());
        }
        let sid8: String = session_id.chars().take(8).collect();
        let live = hooked_list_sessions()
            .into_iter()
            .find(|s| s.session_id == session_id);
        let Some(live) = live else {
            return Err(DeliveryError(format!(
                "claude member '{}' (session {sid8}) has no \
                 live session; the message stays on the bus",
                self.name
            )));
        };
        let accepted = hooked_claude_sessions_send(
            &live.socket_path,
            text,
            &format!("{}.{}", self.team_name, self.name),
            session_id,
        );
        match accepted {
            Some(a) if a != claude_sessions::WRITE_TIMED_OUT => Ok(a.to_string()),
            _ => Err(DeliveryError(format!(
                "claude member '{}' (session {sid8}) did not \
                 accept the frame; the message stays on the bus",
                self.name
            ))),
        }
    }

    /// Deliver to a member with no pane: the engine is the only address.
    ///
    /// Identity comes from the registry row (claude jobId / codex threadId /
    /// grok member key) — there is no pane to probe, and nothing to guard
    /// against pane-id recycling.
    fn _send_headless(&self, text: &str) -> Result<String, DeliveryError> {
        if self.cli == "claude" {
            let sid = self.session_id.clone().unwrap_or_default();
            if sid.is_empty() {
                return Err(DeliveryError(format!(
                    "claude member '{}' has no recorded engine identity; \
                     the message stays on the bus",
                    self.name
                )));
            }
            if hooked_job_row(&sid).is_some() {
                return self._deliver_claude_job(&sid, text);
            }
            return self._deliver_claude_session(&sid, text);
        }
        if self.cli == "codex" {
            let thread_id = self.session_id.clone().unwrap_or_default();
            let accepted = if !thread_id.is_empty() {
                hooked_codex_send_to_thread(&thread_id, text)
            } else {
                None
            };
            return match accepted {
                Some(a) => Ok(a.to_string()),
                None => Err(DeliveryError(format!(
                    "codex member '{}' did not accept the turn \
                     (no recorded thread, daemon down, RPC error, or \
                     connection failure)",
                    self.name
                ))),
            };
        }
        if self.cli == "grok" {
            let key = crate::adapters::grok_leader::member_key(&self.team_name, &self.name);
            return match hooked_grok_send_to_key(&key, text) {
                Some(a) => Ok(a.to_string()),
                None => Err(DeliveryError(format!(
                    "grok member '{}' did not accept the prompt \
                     (no leader/session, RPC error, or connection failure)",
                    self.name
                ))),
            };
        }
        Err(DeliveryError(format!(
            "member '{}' runs '{}', which hive has no \
             headless transport for",
            self.name, self.cli
        )))
    }

    /// Abort the member's running turn over its CLI's native transport.
    ///
    /// Every branch is addressed to the engine, never to the pane: claude's
    /// Escape rides the same pipe as its text, codex takes `turn/interrupt`
    /// on its recorded thread and grok the ACP `session/cancel` on its
    /// recorded session. So the abort lands on *that* turn whatever the
    /// pane's viewer happens to be showing, and a member whose transport is
    /// gone is a refusal — never an Escape into a pager, a copy-mode scroll
    /// or somebody else's session.
    pub fn interrupt(&self) -> anyhow::Result<()> {
        if self.cli == "claude" {
            let mut job_id = if !self.pane_id.is_empty() {
                hooked_job_id_for_pane(&self.pane_id).unwrap_or_default()
            } else {
                String::new()
            };
            if job_id.is_empty() {
                job_id = self.session_id.clone().unwrap_or_default();
            }
            if job_id.is_empty() {
                bail!(
                    "claude member '{}' has no bg job record \
                     to interrupt; hive never send-keys a member pane",
                    self.name
                );
            }
            let result = hooked_interrupt_job(&job_id);
            if !result.ok {
                bail!("claude job {job_id} was not interrupted: {}", result.why);
            }
            return Ok(());
        }
        if self.cli == "codex" {
            let accepted = if !self.pane_id.is_empty() {
                hooked_codex_interrupt_pane(&self.pane_id)
            } else if let Some(sid) = self.session_id.as_deref().filter(|s| !s.is_empty()) {
                hooked_codex_interrupt_thread(sid)
            } else {
                None
            };
            if accepted.is_none() {
                bail!(
                    "codex pane {} did not accept turn/interrupt \
                     (no recorded thread, daemon down, RPC error, or \
                     connection failure)",
                    self.pane_id
                );
            }
            return Ok(());
        }
        if self.cli == "grok" {
            let accepted = if !self.pane_id.is_empty() {
                hooked_grok_interrupt_pane(&self.pane_id)
            } else {
                hooked_grok_interrupt_key(&crate::adapters::grok_leader::member_key(
                    &self.team_name,
                    &self.name,
                ))
            };
            if accepted.is_none() {
                bail!(
                    "grok pane {} did not accept session/cancel \
                     (no leader/session, or connection failure)",
                    self.pane_id
                );
            }
            return Ok(());
        }
        bail!(
            "member '{}' runs '{}', which hive has no native \
             interrupt for; hive never send-keys a member pane",
            self.name,
            self.cli
        );
    }

    /// Capture pane output.
    pub fn capture(&self, lines: u32) -> anyhow::Result<String> {
        hooked_capture_pane(&self.pane_id, lines)
    }

    pub fn is_alive(&self) -> bool {
        if !self.pane_id.is_empty() {
            return crate::tmux::is_pane_alive(&self.pane_id);
        }
        self._engine_alive()
    }

    /// A pane-less member is alive iff its engine answers for it.
    fn _engine_alive(&self) -> bool {
        if self.cli == "claude" {
            let job_id = self.session_id.clone().unwrap_or_default();
            if job_id.is_empty() {
                return false;
            }
            if hooked_engine_session_for_job(&job_id).is_some() || hooked_job_row(&job_id).is_some()
            // asleep is not dead
            {
                return true;
            }
            // A joined interactive session: alive while its channel is live.
            return hooked_list_sessions()
                .iter()
                .any(|s| s.session_id == job_id);
        }
        if self.cli == "codex" {
            return self
                .session_id
                .as_deref()
                .map(|s| !s.is_empty())
                .unwrap_or(false)
                && hooked_codex_daemon_alive();
        }
        if self.cli == "grok" {
            let key = crate::adapters::grok_leader::member_key(&self.team_name, &self.name);
            return hooked_grok_probe_socket(&crate::adapters::grok_leader::socket_path_for_key(
                &key,
            ));
        }
        false
    }

    /// Force kill the pane — and, for a claude member, park its engine.
    ///
    /// The engine lives on claude's supervisor, not in the pane, so killing
    /// the pane alone would leave an orphan job running headless. `claude
    /// stop` parks it: the job stays in the ledger and a managed
    /// `hive claude --resume <jobId>` launch can still wake it.
    pub fn kill(&self) {
        if self.cli == "claude" {
            let mut job_id = if !self.pane_id.is_empty() {
                hooked_job_id_for_pane(&self.pane_id).unwrap_or_default()
            } else {
                String::new()
            };
            if job_id.is_empty() {
                job_id = self.session_id.clone().unwrap_or_default();
            }
            // A joined interactive session is not hive's engine to stop:
            // kill only removes it from the roster.
            if !job_id.is_empty() && hooked_job_row(&job_id).is_some() {
                hooked_stop_job(&job_id);
            }
            if !self.pane_id.is_empty() {
                crate::adapters::claude_bg::clear_pane_job(&self.pane_id);
            }
        } else if self.cli == "grok" {
            // The member's leader daemon is the engine; a kill removes the
            // member, so the engine goes with it — deterministically, not on
            // the hived's next orphan sweep. Resolve while the pane tags
            // still exist; a pane-less member is addressed by its member key.
            let key = if !self.pane_id.is_empty() {
                crate::adapters::grok_leader::resolve_pane_key(&self.pane_id)
            } else {
                crate::adapters::grok_leader::member_key(&self.team_name, &self.name)
            };
            crate::adapters::grok_leader::pool().drop_key(&key);
            crate::adapters::grok_leader::kill_daemon_key(&key);
        }
        if !self.pane_id.is_empty() {
            hooked_kill_pane(&self.pane_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Seams: every cross-module effect goes through one wrapper so the unit tests
// can intercept it the way the Python suite monkeypatches the module globals.
// Without an installed test hook each wrapper is a plain passthrough.
// ---------------------------------------------------------------------------

fn hooked_is_inside_tmux() -> bool {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| h.is_inside_tmux) {
        return v;
    }
    crate::tmux::is_inside_tmux()
}

fn hooked_split_window(
    target: &str,
    horizontal: bool,
    size: Option<&str>,
) -> anyhow::Result<String> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.split_window_result
            .clone()
            .unwrap_or_else(|| target.to_string())
    }) {
        return Ok(v);
    }
    crate::tmux::split_window(target, horizontal, size, true, None)
}

fn hooked_get_pane_window_target(pane_id: &str) -> String {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| h.pane_window_target.clone()) {
        return v;
    }
    crate::tmux::get_pane_window_target(pane_id).unwrap_or_default()
}

fn hooked_set_pane_title(pane_id: &str, title: &str) {
    #[cfg(test)]
    if testhook::with(|h| h.titles.push((pane_id.to_string(), title.to_string()))).is_some() {
        return;
    }
    crate::tmux::set_pane_title(pane_id, title);
}

fn hooked_tag_pane(pane_id: &str, role: &str, agent: &str, team: &str, cli: &str) {
    #[cfg(test)]
    if testhook::with(|h| {
        h.tags.push((
            pane_id.to_string(),
            role.to_string(),
            agent.to_string(),
            team.to_string(),
        ))
    })
    .is_some()
    {
        return;
    }
    crate::tmux::tag_pane(pane_id, role, agent, team, cli, "");
}

fn hooked_kill_pane(pane_id: &str) {
    #[cfg(test)]
    if testhook::with(|h| h.killed.push(pane_id.to_string())).is_some() {
        return;
    }
    crate::tmux::kill_pane(pane_id);
}

fn hooked_clear_pane_tags(pane_id: &str) {
    #[cfg(test)]
    if testhook::with(|h| h.cleared_tags.push(pane_id.to_string())).is_some() {
        return;
    }
    crate::tmux::clear_pane_tags(pane_id);
}

fn hooked_send_keys(pane_id: &str, text: &str, enter: bool) -> anyhow::Result<()> {
    #[cfg(test)]
    if testhook::with(|h| {
        h.calls.push(text.to_string());
        if enter {
            h.calls.push("<Enter>".to_string());
        }
    })
    .is_some()
    {
        return Ok(());
    }
    crate::tmux::send_keys(pane_id, text, enter)
}

fn hooked_send_key(pane_id: &str, key: &str) -> anyhow::Result<()> {
    #[cfg(test)]
    if testhook::with(|h| h.calls.push(format!("<{key}>"))).is_some() {
        return Ok(());
    }
    crate::tmux::send_key(pane_id, key)
}

fn hooked_is_pane_in_mode(pane_id: &str) -> bool {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| h.is_pane_in_mode) {
        return v;
    }
    crate::tmux::is_pane_in_mode(pane_id)
}

fn hooked_cancel_pane_mode(pane_id: &str) {
    #[cfg(test)]
    if testhook::with(|h| h.cancelled_modes.push(pane_id.to_string())).is_some() {
        return;
    }
    crate::tmux::cancel_pane_mode(pane_id);
}

fn hooked_load_buffer(name: &str, data: &str) -> anyhow::Result<()> {
    #[cfg(test)]
    if let Some(fails) = testhook::with(|h| {
        if !h.load_buffer_fails {
            h.buffers_loaded.push((name.to_string(), data.to_string()));
        }
        h.load_buffer_fails
    }) {
        if fails {
            bail!("tmux load-buffer timed out");
        }
        return Ok(());
    }
    crate::tmux::load_buffer(name, data)
}

fn hooked_paste_buffer(name: &str, target: &str, bracketed: bool) {
    #[cfg(test)]
    if testhook::with(|h| h.pasted.push((name.to_string(), target.to_string()))).is_some() {
        return;
    }
    crate::tmux::paste_buffer(name, target, bracketed);
}

fn hooked_delete_buffer(name: &str) {
    #[cfg(test)]
    if testhook::with(|h| h.deleted_buffers.push(name.to_string())).is_some() {
        return;
    }
    crate::tmux::delete_buffer(name);
}

fn hooked_capture_pane(pane_id: &str, lines: u32) -> anyhow::Result<String> {
    #[cfg(test)]
    if testhook::with(|h| h.captured.push((pane_id.to_string(), lines))).is_some() {
        return Ok(String::new());
    }
    crate::tmux::capture_pane(pane_id, lines, false)
}

fn hooked_sleep(seconds: f64) {
    #[cfg(test)]
    if testhook::with(|h| h.sleeps.push(seconds)).is_some() {
        return;
    }
    std::thread::sleep(Duration::from_secs_f64(seconds.max(0.0)));
}

fn hooked_supported_profile(profile_name: &str) -> bool {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| h.supported_profile) {
        return v;
    }
    crate::draft_guard::supported_profile(profile_name)
}

fn hooked_parse_draft(pane_id: &str, profile_name: &str) -> anyhow::Result<String> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| h.parse_draft.clone()) {
        return Ok(v.unwrap_or_default());
    }
    crate::draft_guard::parse_draft(pane_id, profile_name)
}

fn hooked_clear_input(pane_id: &str, profile_name: &str) -> anyhow::Result<()> {
    #[cfg(test)]
    if let Some(fails) = testhook::with(|h| {
        if !h.clear_input_fails {
            h.draft_cleared.push(pane_id.to_string());
        }
        h.clear_input_fails
    }) {
        if fails {
            bail!("tmux clear-input timed out");
        }
        return Ok(());
    }
    crate::draft_guard::clear_input(pane_id, profile_name)
}

fn hooked_wait_input_empty(
    pane_id: &str,
    profile_name: &str,
    timeout: f64,
) -> anyhow::Result<bool> {
    #[cfg(test)]
    if testhook::with(|_h| ()).is_some() {
        return Ok(true);
    }
    crate::draft_guard::wait_input_empty(pane_id, profile_name, Duration::from_secs_f64(timeout))
}

fn hooked_resolve_session_id_for_pane(pane_id: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.resolved_session_panes.push(pane_id.to_string());
        h.session_ids_by_pane.get(pane_id).cloned()
    }) {
        return v;
    }
    crate::agent_cli::resolve_session_id_for_pane(pane_id, None)
}

fn hooked_detect_cli_process_for_pane(pane_id: &str) -> Option<crate::agent_cli::CLIProfile> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        if !h.cli_probe_seq.is_empty() {
            let name = h.cli_probe_seq.remove(0);
            return name.and_then(|n| crate::agent_cli::get_profile(&n));
        }
        match &h.cli_probe {
            Some(name) if !name.is_empty() => crate::agent_cli::get_profile(name),
            _ => None,
        }
    }) {
        return v.cloned();
    }
    crate::agent_cli::detect_cli_process_for_pane(pane_id).cloned()
}

fn hooked_interactive_claude_pid(pane_id: &str) -> Option<i32> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| h.interactive_claude_pid) {
        return v;
    }
    crate::adapters::claude_view::interactive_claude_pid(pane_id)
}

fn hooked_wait_codex_attached(pane_id: &str) -> bool {
    #[cfg(test)]
    if let Some(Some(v)) = testhook::with(|h| {
        h.wait_codex_attached.inspect(|_| {
            h.waited_codex.push(pane_id.to_string());
        })
    }) {
        return v;
    }
    _wait_codex_attached(pane_id, AGENT_STARTUP_TIMEOUT, 0.5)
}

fn hooked_wait_grok_session_ready(pane_id: &str, session_id: &str) -> bool {
    #[cfg(test)]
    if let Some(Some(v)) = testhook::with(|h| {
        h.wait_grok_ready.inspect(|_| {
            h.waited_grok
                .push((pane_id.to_string(), session_id.to_string()));
            h.event_order.push(format!("ready:{pane_id}"));
        })
    }) {
        return v;
    }
    _wait_grok_session_ready(pane_id, session_id, AGENT_STARTUP_TIMEOUT, 0.5)
}

// --- claude_bg seams -------------------------------------------------------

fn hooked_job_id_for_pane(pane_id: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.pane_job_lookups.push(pane_id.to_string());
        h.job_id_for_pane.clone()
    }) {
        return v;
    }
    crate::adapters::claude_bg::job_id_for_pane(pane_id)
}

fn hooked_engine_session_for_job(job_id: &str) -> Option<EngineSession> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.seen_jobs.push(job_id.to_string());
        h.engines_by_job.get(job_id).cloned()
    }) {
        return v;
    }
    crate::adapters::claude_bg::engine_session_for_job(job_id)
}

fn hooked_job_row(job_id: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        if h.job_row_ids.iter().any(|j| j == job_id) {
            let mut row = serde_json::Map::new();
            row.insert(
                "id".to_string(),
                serde_json::Value::String(job_id.to_string()),
            );
            Some(row)
        } else {
            None
        }
    }) {
        return v;
    }
    crate::adapters::claude_bg::job_row(job_id, "claude")
}

fn hooked_ensure_engine(job_id: &str, timeout: Option<f64>) -> Option<EngineSession> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.wakes.push(job_id.to_string());
        match &h.ensure_engine {
            None => Some(testhook::fake_engine(4321, job_id, "sess-registry")),
            Some(v) => v.clone(),
        }
    }) {
        return v;
    }
    crate::adapters::claude_bg::ensure_engine(job_id, timeout, "claude")
}

fn hooked_wait_engine_entry(job_id: &str, timeout: f64) -> Option<EngineSession> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| h.wait_engine_entry.clone()) {
        return v;
    }
    crate::adapters::claude_bg::wait_engine_entry(job_id, timeout)
}

fn hooked_spawn_job(
    cwd: &str,
    name: &str,
    prompt: &str,
    extra_args: &[String],
    extra_env: &HashMap<String, String>,
) -> Option<String> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.spawns.push(testhook::SpawnRecord {
            cwd: cwd.to_string(),
            name: name.to_string(),
            prompt: prompt.to_string(),
            extra_args: extra_args.to_vec(),
            extra_env: extra_env.clone(),
        });
        h.spawn_job_result.clone()
    }) {
        return v;
    }
    crate::adapters::claude_bg::spawn_job(cwd, name, prompt, extra_args, Some(extra_env), "claude")
}

fn hooked_write_pane_job(
    pane_id: &str,
    job_id: &str,
    session_id: &str,
    cwd: &str,
) -> anyhow::Result<()> {
    #[cfg(test)]
    if testhook::with(|h| {
        h.records.push((
            pane_id.to_string(),
            job_id.to_string(),
            session_id.to_string(),
            cwd.to_string(),
        ))
    })
    .is_some()
    {
        return Ok(());
    }
    Ok(crate::adapters::claude_bg::write_pane_job(
        pane_id, job_id, session_id, cwd,
    )?)
}

fn hooked_stop_job(job_id: &str) {
    #[cfg(test)]
    if testhook::with(|h| h.stopped.push(job_id.to_string())).is_some() {
        return;
    }
    crate::adapters::claude_bg::stop_job(job_id, "claude");
}

fn hooked_type_into_job(job_id: &str, text: &str) -> KeyResult {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.type_into_job_result.clone().inspect(|_| {
            h.typed.push((job_id.to_string(), text.to_string()));
        })
    })
    .flatten()
    {
        return v;
    }
    crate::adapters::claude_bg::type_into_job(job_id, text, "claude")
}

fn hooked_interrupt_job(job_id: &str) -> KeyResult {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.interrupt_job_result.clone().inspect(|_| {
            h.interrupted_jobs.push(job_id.to_string());
        })
    })
    .flatten()
    {
        return v;
    }
    crate::adapters::claude_bg::interrupt_job(job_id, "claude")
}

// --- claude_sessions seams -------------------------------------------------

fn hooked_daemon_reply(session_id: &str, text: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.daemon_replies
            .push((session_id.to_string(), text.to_string()));
        h.daemon_reply
    }) {
        return v;
    }
    claude_sessions::daemon_reply(session_id, text)
}

fn hooked_claude_sessions_send(
    sock_path: &str,
    text: &str,
    sender: &str,
    session_id: &str,
) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.inbox_writes.push((
            sock_path.to_string(),
            text.to_string(),
            sender.to_string(),
            session_id.to_string(),
        ));
        h.sessions_send
    }) {
        return v;
    }
    claude_sessions::send(sock_path, text, sender, session_id)
}

fn hooked_list_sessions() -> Vec<claude_sessions::ClaudeSession> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| h.list_sessions.clone()) {
        return v;
    }
    claude_sessions::list_sessions()
}

// --- codex_app_server seams ------------------------------------------------

fn hooked_codex_spawn_daemon() -> bool {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.codex_started.push(());
        h.codex_spawn_daemon
    }) {
        return v;
    }
    crate::adapters::codex_app_server::spawn_daemon()
}

fn hooked_ensure_dir_trusted(cwd: &str) -> anyhow::Result<()> {
    #[cfg(test)]
    if testhook::with(|h| h.codex_trusted.push(cwd.to_string())).is_some() {
        return Ok(());
    }
    crate::adapters::codex_app_server::ensure_dir_trusted(cwd)
}

fn hooked_start_member_thread(cwd: &str, name: &str, model: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.codex_minted
            .push((cwd.to_string(), name.to_string(), model.to_string()));
        h.start_member_thread.clone()
    }) {
        return v;
    }
    crate::adapters::codex_app_server::start_member_thread(cwd, name, model)
}

fn hooked_write_pane_thread(pane_id: &str, thread_id: &str, cwd: &str) -> anyhow::Result<()> {
    #[cfg(test)]
    if testhook::with(|h| {
        h.codex_records
            .push((pane_id.to_string(), thread_id.to_string(), cwd.to_string()))
    })
    .is_some()
    {
        return Ok(());
    }
    crate::adapters::codex_app_server::write_pane_thread(pane_id, thread_id, cwd)
}

fn hooked_codex_send_to_pane(pane_id: &str, text: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.codex_sent.push((pane_id.to_string(), text.to_string()));
        h.codex_send_to_pane
    }) {
        return v;
    }
    crate::adapters::codex_app_server::send_to_pane(pane_id, text)
}

fn hooked_codex_send_to_thread(thread_id: &str, text: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.codex_sent_thread
            .push((thread_id.to_string(), text.to_string()));
        h.codex_send_to_thread
    }) {
        return v;
    }
    crate::adapters::codex_app_server::send_to_thread(thread_id, text)
}

fn hooked_codex_interrupt_pane(pane_id: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.codex_interrupted_panes.push(pane_id.to_string());
        h.codex_interrupt_pane
    }) {
        return v;
    }
    crate::adapters::codex_app_server::interrupt_pane(pane_id)
}

fn hooked_codex_interrupt_thread(thread_id: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.codex_interrupted_threads.push(thread_id.to_string());
        h.codex_interrupt_thread
    }) {
        return v;
    }
    crate::adapters::codex_app_server::interrupt_thread(thread_id)
}

fn hooked_codex_daemon_alive() -> bool {
    #[cfg(test)]
    if let Some(Some(v)) = testhook::with(|h| h.codex_daemon_alive) {
        return v;
    }
    crate::adapters::codex_app_server::daemon_alive()
}

// --- grok_leader seams -----------------------------------------------------

fn hooked_grok_spawn_daemon(pane_id: &str) -> bool {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.grok_started.push(pane_id.to_string());
        h.grok_spawn_daemon
    }) {
        return v;
    }
    crate::adapters::grok_leader::spawn_daemon(pane_id)
}

fn hooked_write_pane_session(pane_id: &str, session_id: &str, cwd: &str) -> anyhow::Result<()> {
    #[cfg(test)]
    if testhook::with(|h| {
        h.grok_sessions
            .push((pane_id.to_string(), session_id.to_string(), cwd.to_string()))
    })
    .is_some()
    {
        return Ok(());
    }
    crate::adapters::grok_leader::write_pane_session(pane_id, session_id, cwd)
}

fn hooked_grok_send_to_pane(pane_id: &str, text: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.grok_sent.push((pane_id.to_string(), text.to_string()));
        h.grok_send_to_pane
    }) {
        return v;
    }
    crate::adapters::grok_leader::send_to_pane(pane_id, text)
}

fn hooked_grok_send_to_key(key: &str, text: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.grok_sent_key.push((key.to_string(), text.to_string()));
        h.grok_send_to_key
    }) {
        return v;
    }
    crate::adapters::grok_leader::send_to_key(key, text)
}

fn hooked_grok_interrupt_pane(pane_id: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.grok_interrupted_panes.push(pane_id.to_string());
        h.grok_interrupt_pane
    }) {
        return v;
    }
    crate::adapters::grok_leader::interrupt_pane(pane_id)
}

fn hooked_grok_interrupt_key(key: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.grok_interrupted_keys.push(key.to_string());
        h.grok_interrupt_key
    }) {
        return v;
    }
    crate::adapters::grok_leader::interrupt_key(key)
}

fn hooked_grok_probe_socket(socket_path: &std::path::Path) -> bool {
    #[cfg(test)]
    if let Some(Some(v)) = testhook::with(|h| h.grok_probe_socket) {
        return v;
    }
    crate::adapters::grok_leader::probe_socket(socket_path)
}

// --- hived seams -----------------------------------------------------------

fn hooked_request_connect_codex(workspace: &str) {
    #[cfg(test)]
    if testhook::with(|h| h.connects_codex.push(workspace.to_string())).is_some() {
        return;
    }
    let _ = crate::hived::request_connect_codex(workspace);
}

fn hooked_request_connect_grok(workspace: &str, pane_id: &str) {
    #[cfg(test)]
    if testhook::with(|h| {
        h.connects_grok
            .push((workspace.to_string(), pane_id.to_string()));
        h.event_order.push(format!("connect:{workspace}:{pane_id}"));
    })
    .is_some()
    {
        return;
    }
    let _ = crate::hived::request_connect_grok(workspace, pane_id);
}

// ---------------------------------------------------------------------------
// Test hook: one thread-local environment double, mirroring what the Python
// suite pins with monkeypatch (`_setup_tmux_mocks` defaults in `Hook::new`).
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod testhook {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use crate::adapters::claude_bg::{EngineSession, KeyResult};
    use crate::adapters::claude_sessions::ClaudeSession;

    #[derive(Debug, Clone, PartialEq)]
    pub struct SpawnRecord {
        pub cwd: String,
        pub name: String,
        pub prompt: String,
        pub extra_args: Vec<String>,
        pub extra_env: HashMap<String, String>,
    }

    /// A bg engine registry entry as engine_session_for_job would return it.
    pub fn fake_engine(pid: i32, job_id: &str, session_id: &str) -> EngineSession {
        EngineSession {
            pid,
            job_id: job_id.to_string(),
            session_id: session_id.to_string(),
            socket_path: format!("/tmp/hive-test-inbox-{pid}.sock"),
            cwd: "/tmp".to_string(),
            status: "idle".to_string(),
            waiting_for: String::new(),
            status_updated_at: 0.0,
            name: String::new(),
        }
    }

    #[allow(dead_code)]
    #[derive(Default)]
    pub struct Hook {
        // records
        pub calls: Vec<String>,
        pub tags: Vec<(String, String, String, String)>,
        pub titles: Vec<(String, String)>,
        pub killed: Vec<String>,
        pub cleared_tags: Vec<String>,
        pub captured: Vec<(String, u32)>,
        pub sleeps: Vec<f64>,
        pub cancelled_modes: Vec<String>,
        pub buffers_loaded: Vec<(String, String)>,
        pub pasted: Vec<(String, String)>,
        pub deleted_buffers: Vec<String>,
        pub draft_cleared: Vec<String>,
        pub resolved_session_panes: Vec<String>,
        pub event_order: Vec<String>,

        pub spawns: Vec<SpawnRecord>,
        pub wakes: Vec<String>,
        pub records: Vec<(String, String, String, String)>,
        pub stopped: Vec<String>,
        pub pane_job_lookups: Vec<String>,
        pub seen_jobs: Vec<String>,
        pub typed: Vec<(String, String)>,
        pub interrupted_jobs: Vec<String>,

        pub codex_started: Vec<()>,
        pub codex_minted: Vec<(String, String, String)>,
        pub codex_trusted: Vec<String>,
        pub codex_records: Vec<(String, String, String)>,
        pub codex_sent: Vec<(String, String)>,
        pub codex_sent_thread: Vec<(String, String)>,
        pub codex_interrupted_panes: Vec<String>,
        pub codex_interrupted_threads: Vec<String>,

        pub grok_started: Vec<String>,
        pub grok_sessions: Vec<(String, String, String)>,
        pub grok_sent: Vec<(String, String)>,
        pub grok_sent_key: Vec<(String, String)>,
        pub grok_interrupted_panes: Vec<String>,
        pub grok_interrupted_keys: Vec<String>,

        pub inbox_writes: Vec<(String, String, String, String)>,
        pub daemon_replies: Vec<(String, String)>,

        pub connects_codex: Vec<String>,
        pub connects_grok: Vec<(String, String)>,
        pub waited_codex: Vec<String>,
        pub waited_grok: Vec<(String, String)>,

        // behaviors (Hook::new sets the `_setup_tmux_mocks` defaults)
        pub is_inside_tmux: bool,
        pub split_window_result: Option<String>, // None → echo the target pane
        pub pane_window_target: String,
        pub is_pane_in_mode: bool,
        pub supported_profile: bool,
        pub parse_draft: Option<String>,
        pub load_buffer_fails: bool,
        pub clear_input_fails: bool,
        pub resolve_profile_name: Option<String>,
        pub interactive_claude_pid: Option<i32>,
        pub cli_probe: Option<String>, // "" or unset → no live CLI on the pane
        pub cli_probe_seq: Vec<Option<String>>, // consumed first when non-empty
        pub session_ids_by_pane: HashMap<String, String>,
        pub wait_codex_attached: Option<bool>, // None → run the real wait
        pub wait_grok_ready: Option<bool>,     // None → run the real wait

        pub spawn_job_result: Option<String>,
        pub wait_engine_entry: Option<EngineSession>,
        pub ensure_engine: Option<Option<EngineSession>>, // None → echo-jid engine
        pub job_id_for_pane: Option<String>,
        pub job_row_ids: Vec<String>, // job_row answers Some({"id": jid}) for these
        pub engines_by_job: HashMap<String, EngineSession>,
        pub type_into_job_result: Option<KeyResult>,
        pub interrupt_job_result: Option<KeyResult>,

        pub daemon_reply: Option<&'static str>,
        pub sessions_send: Option<&'static str>,
        pub list_sessions: Vec<ClaudeSession>,

        pub codex_spawn_daemon: bool,
        pub start_member_thread: Option<String>,
        pub codex_send_to_pane: Option<&'static str>,
        pub codex_send_to_thread: Option<&'static str>,
        pub codex_interrupt_pane: Option<&'static str>,
        pub codex_interrupt_thread: Option<&'static str>,
        pub codex_daemon_alive: Option<bool>,

        pub grok_spawn_daemon: bool,
        pub grok_send_to_pane: Option<&'static str>,
        pub grok_send_to_key: Option<&'static str>,
        pub grok_interrupt_pane: Option<&'static str>,
        pub grok_interrupt_key: Option<&'static str>,
        pub grok_probe_socket: Option<bool>,
    }

    impl Hook {
        /// Python `_setup_tmux_mocks` equivalents: inside tmux, split echoes
        /// the target, no daemons, readiness waits answer immediately, the
        /// claude bg spawn path succeeds without touching a real binary.
        pub fn new() -> Hook {
            Hook {
                is_inside_tmux: true,
                wait_codex_attached: Some(true),
                wait_grok_ready: Some(true),
                spawn_job_result: Some("abcd1234".to_string()),
                wait_engine_entry: Some(fake_engine(4321, "abcd1234", "sess-registry")),
                start_member_thread: Some("tid-minted".to_string()),
                ..Default::default()
            }
        }
    }

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    pub fn with<T>(f: impl FnOnce(&mut Hook) -> T) -> Option<T> {
        HOOK.with(|cell| cell.borrow_mut().as_mut().map(f))
    }

    pub struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            HOOK.with(|cell| *cell.borrow_mut() = None);
        }
    }

    pub fn install(hook: Hook) -> Guard {
        HOOK.with(|cell| *cell.borrow_mut() = Some(hook));
        Guard
    }
}

#[cfg(test)]
mod tests {
    use super::testhook::{self, fake_engine, Hook};
    use super::*;

    fn setup() -> testhook::Guard {
        testhook::install(Hook::new())
    }

    fn hook<T>(f: impl FnOnce(&mut Hook) -> T) -> T {
        testhook::with(f).expect("test hook installed")
    }

    fn spawn_opts(f: impl FnOnce(&mut SpawnOptions)) -> SpawnOptions {
        let mut opts = SpawnOptions {
            cwd: "/tmp".to_string(),
            ..SpawnOptions::default()
        };
        f(&mut opts);
        opts
    }

    fn member(name: &str, team: &str, pane: &str, cli: &str) -> Agent {
        Agent {
            name: name.to_string(),
            team_name: team.to_string(),
            pane_id: pane.to_string(),
            model: String::new(),
            prompt: String::new(),
            cwd: "/tmp".to_string(),
            session_id: None,
            spawned_at: 0.0,
            cli: cli.to_string(),
        }
    }

    fn headless(cli: &str, session_id: Option<&str>) -> Agent {
        let mut agent = member("rex", "honey", "", cli);
        agent.cwd = "/repo".to_string();
        agent.session_id = session_id.map(|s| s.to_string());
        agent
    }

    /// Python `_mock_claude_bg_up`.
    fn mock_claude_bg_up(job_id: &str, session_id: &str) {
        let engine = fake_engine(4321, job_id, session_id);
        hook(|h| {
            h.spawn_job_result = Some(job_id.to_string());
            h.wait_engine_entry = Some(engine.clone());
            h.ensure_engine = Some(Some(engine.clone()));
        });
    }

    /// Python `_mock_daemon_up`.
    fn mock_daemon_up() {
        hook(|h| h.codex_spawn_daemon = true);
    }

    /// Python `_mock_grok_leader_up`.
    fn mock_grok_leader_up() {
        hook(|h| {
            h.grok_spawn_daemon = true;
            h.wait_grok_ready = Some(true);
        });
    }

    /// Python `_pin_cli_probe`: "" pins "no live CLI process".
    fn pin_cli_probe(name: &str) {
        hook(|h| h.cli_probe = Some(name.to_string()));
    }

    /// Python `_pin_job`: pane record -> engine entry.
    fn pin_job(job_id: &str, engine: EngineSession) {
        hook(|h| {
            h.job_id_for_pane = Some(job_id.to_string());
            h.engines_by_job.insert(job_id.to_string(), engine);
        });
    }

    /// Python `_stale_claude_record`.
    fn stale_claude_record() {
        pin_job("beef4321", fake_engine(4321, "beef4321", "sess-registry"));
        hook(|h| h.sessions_send = Some(claude_sessions::ACCEPTED_UDS_WRITE));
    }

    fn calls() -> Vec<String> {
        hook(|h| h.calls.clone())
    }

    fn launch_of(cmd: &str) -> String {
        cmd.split(" && ")
            .last()
            .unwrap()
            .split("; hive resume-hint")
            .next()
            .unwrap()
            .to_string()
    }

    fn err_of<T: std::fmt::Debug>(result: anyhow::Result<T>) -> String {
        format!("{:#}", result.expect_err("expected an error"))
    }

    // --- spawn -------------------------------------------------------------

    #[test]
    fn test_spawn_rejects_outside_tmux() {
        let _guard = setup();
        hook(|h| h.is_inside_tmux = false);
        let err = err_of(Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| o.skill = "none".into()),
        ));
        assert!(err.contains("requires tmux"), "{err}");
    }

    #[test]
    fn test_spawn_loads_specified_skill() {
        let _guard = setup();
        mock_claude_bg_up("abcd1234", "sess-registry");
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.is_first = true;
                o.skill = "demo-review".into();
            }),
        )
        .unwrap();
        // The skill activation rides the bg spawn's prompt, not the pane command.
        assert_eq!(hook(|h| h.spawns[0].prompt.clone()), "/demo-review t");
        assert!(!calls().iter().any(|c| c.contains("hive teammate")));
    }

    #[test]
    fn test_spawn_skips_skill_when_none() {
        let _guard = setup();
        mock_claude_bg_up("abcd1234", "sess-registry");
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.is_first = true;
                o.skill = "none".into();
            }),
        )
        .unwrap();
        assert_eq!(hook(|h| h.spawns[0].prompt.clone()), "");
        assert!(!calls()
            .iter()
            .any(|c| c.starts_with('/') && !c.starts_with("/tmp")));
    }

    #[test]
    fn test_spawn_passes_extra_env() {
        let _guard = setup();
        mock_claude_bg_up("abcd1234", "sess-registry");
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.is_first = true;
                o.skill = "none".into();
                o.extra_env = Some(vec![("CR_WORKSPACE".into(), "/tmp/cr-test".into())]);
            }),
        )
        .unwrap();
        let startup_cmd = calls()[0].clone();
        assert!(startup_cmd.contains("CR_WORKSPACE="));
        assert!(startup_cmd.contains("/tmp/cr-test"));
        assert!(!startup_cmd.contains("HIVE_TEAM_NAME="));
        assert!(!startup_cmd.contains("HIVE_AGENT_NAME="));
        // The engine runs outside the pane, so the env must reach the bg spawn,
        // alongside the member identity its tools resolve without a pane.
        let expected: HashMap<String, String> = [
            ("HIVE_TEAM".to_string(), "t".to_string()),
            ("HIVE_MEMBER".to_string(), "w1".to_string()),
            ("CR_WORKSPACE".to_string(), "/tmp/cr-test".to_string()),
        ]
        .into_iter()
        .collect();
        assert_eq!(hook(|h| h.spawns[0].extra_env.clone()), expected);
    }

    #[test]
    fn test_spawn_without_extra_env_does_not_export_default_hive_vars() {
        let _guard = setup();
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.is_first = true;
                o.skill = "none".into();
            }),
        )
        .unwrap();
        let startup_cmd = calls()[0].clone();
        assert!(!startup_cmd.contains("HIVE_TEAM_NAME="));
        assert!(!startup_cmd.contains("HIVE_AGENT_NAME="));
        assert!(!startup_cmd.contains("export "));
    }

    #[test]
    fn test_spawn_hive_loads_skill_and_sends_prompt() {
        let _guard = setup();
        mock_claude_bg_up("abcd1234", "sess-registry");
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.is_first = true;
                o.skill = "hive".into();
                o.prompt = "Please check your inbox.".into();
            }),
        )
        .unwrap();
        // Skill activation + user prompt ride the bg spawn's positional prompt.
        assert_eq!(
            hook(|h| h.spawns[0].prompt.clone()),
            "/hive t\n\nPlease check your inbox."
        );
        assert_eq!(hook(|h| h.spawns[0].name.clone()), "t.w1");
    }

    #[test]
    fn test_spawn_codex_hive_loads_skill_and_sends_prompt() {
        let _guard = setup();
        mock_daemon_up();
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.is_first = true;
                o.skill = "hive".into();
                o.prompt = "Please check your inbox.".into();
                o.cli = "codex".into();
            }),
        )
        .unwrap();
        // Skill activation + user prompt are passed as the [PROMPT] positional
        // arg (avoids TUI keystroke race against the codex skill picker).
        let calls = calls();
        let startup_cmd = calls[0].clone();
        assert!(startup_cmd.contains("$hive"));
        assert!(startup_cmd.contains("Please check your inbox."));
        // Only the initial `cd ... && codex` Enter — no follow-up TUI inject.
        assert_eq!(calls.iter().filter(|c| *c == "<Enter>").count(), 1);
    }

    #[test]
    fn test_spawn_claude_mints_job_records_pane_and_attaches() {
        // the job (and its engine entry) exist BEFORE the pane command is typed:
        // readiness is the engine registering, never screen text
        let _guard = setup();
        mock_claude_bg_up("abcd1234", "sess-registry");
        let agent = Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.is_first = true;
                o.cli = "claude".into();
            }),
        )
        .unwrap();
        assert_eq!(agent.pane_id, "%0");
        assert!(hook(|h| h.captured.clone()).is_empty()); // no screen scraping anywhere in the spawn
        assert_eq!(hook(|h| h.spawns[0].name.clone()), "t.w1");
        assert_eq!(
            hook(|h| h.records.clone()),
            vec![(
                "%0".to_string(),
                "abcd1234".to_string(),
                "sess-registry".to_string(),
                "/tmp".to_string()
            )]
        );
        let launch = launch_of(&calls()[0]);
        assert_eq!(
            launch.split_whitespace().collect::<Vec<_>>(),
            vec!["hive", "claude", "--resume", "'abcd1234'"]
        );
    }

    #[test]
    fn test_spawn_claude_mint_failure_kills_pane_and_fails() {
        let _guard = setup();
        hook(|h| h.spawn_job_result = None);
        let err = err_of(Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.is_first = true;
                o.cli = "claude".into();
            }),
        ));
        assert!(err.contains("job identity"), "{err}");
        assert_eq!(hook(|h| h.killed.clone()), vec!["%0"]);
        assert!(calls().is_empty()); // no startup command was ever sent
    }

    #[test]
    fn test_spawn_claude_engine_never_registers_stops_job_and_fails() {
        let _guard = setup();
        mock_claude_bg_up("abcd1234", "sess-registry");
        hook(|h| h.wait_engine_entry = None);
        let err = err_of(Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.is_first = true;
                o.cli = "claude".into();
            }),
        ));
        assert!(err.contains("inbox-only"), "{err}");
        assert_eq!(hook(|h| h.stopped.clone()), vec!["abcd1234"]); // the half-born job is parked
        assert_eq!(hook(|h| h.killed.clone()), vec!["%0"]);
        assert!(calls().is_empty());
    }

    #[test]
    fn test_spawn_rejects_prompt_starting_with_dash() {
        // the launch goes through `hive <cli>`, whose parser strips any `--`
        // separator, so a dashed prompt would be read as a flag: refuse it
        for cli_name in ["claude", "codex", "grok"] {
            let _guard = setup();
            mock_daemon_up();
            mock_grok_leader_up();
            let err = err_of(Agent::spawn(
                "w1",
                "t",
                "%0",
                spawn_opts(|o| {
                    o.is_first = true;
                    o.cli = cli_name.into();
                    o.skill = "none".into();
                    o.prompt = "--edge prompt".into();
                }),
            ));
            assert!(err.contains("must not start with '-'"), "{err}");
        }
    }

    #[test]
    fn test_spawn_pane_command_runs_hive_launcher_then_resume_hint() {
        // the pane runs hive's managed launcher as the binary (never the rc's
        // hclaude/hcodex/hgrok function) and prints the cd-ready hint once the
        // CLI exits
        for cli_name in ["claude", "codex", "grok"] {
            let _guard = setup();
            mock_daemon_up();
            mock_grok_leader_up();
            Agent::spawn(
                "w1",
                "t",
                "%0",
                spawn_opts(|o| {
                    o.cwd = "/work/dir".into();
                    o.is_first = true;
                    o.cli = cli_name.into();
                    o.skill = "none".into();
                }),
            )
            .unwrap();
            let launch = calls()[0].split(" && ").last().unwrap().to_string();
            let tail = format!("; hive resume-hint {cli_name} 2>/dev/null || true");
            assert!(launch.ends_with(&tail), "{launch}");
            // token check, not a prefix: a bare claude launch now carries no flags
            let head = &launch[..launch.len() - tail.len()];
            assert_eq!(
                head.split_whitespace().take(2).collect::<Vec<_>>(),
                vec!["hive", cli_name]
            );
        }
    }

    #[test]
    fn test_spawn_claude_resume_wakes_the_job_and_rebinds_the_pane() {
        // resume of a claude member is just waking its durable job: nothing is
        // minted, the pane record points at the same jobId
        let _guard = setup();
        mock_claude_bg_up("cafe0123", "sess-registry");
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.is_first = true;
                o.cli = "claude".into();
                o.session_id = Some("cafe0123".into());
                o.session_mode = "resume".into();
            }),
        )
        .unwrap();
        assert!(hook(|h| h.spawns.clone()).is_empty()); // nothing minted on resume
        assert_eq!(hook(|h| h.wakes.clone()), vec!["cafe0123"]);
        assert_eq!(
            hook(|h| h.records.clone()),
            vec![(
                "%0".to_string(),
                "cafe0123".to_string(),
                "sess-registry".to_string(),
                "/tmp".to_string()
            )]
        );
        assert!(calls()[0].contains("--resume 'cafe0123'"));
    }

    #[test]
    fn test_spawn_claude_resume_of_a_gone_job_fails_and_gives_the_pane_back() {
        let _guard = setup();
        hook(|h| h.ensure_engine = Some(None));
        let err = err_of(Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.is_first = true;
                o.cli = "claude".into();
                o.session_id = Some("cafe0123".into());
                o.session_mode = "resume".into();
            }),
        ));
        assert!(err.contains("did not come back"), "{err}");
        assert_eq!(hook(|h| h.killed.clone()), vec!["%0"]);
        assert!(calls().is_empty());
    }

    #[test]
    fn test_spawn_tags_pane_before_waiting_for_ready() {
        let _guard = setup();
        Agent::spawn(
            "w1",
            "t",
            "%9",
            spawn_opts(|o| {
                o.is_first = true;
                o.skill = "none".into();
                o.cli = "claude".into();
            }),
        )
        .unwrap();
        assert!(
            !calls().is_empty(),
            "spawn should still start the CLI process"
        );
        assert_eq!(
            hook(|h| h.tags.clone()),
            vec![(
                "%9".to_string(),
                "agent".to_string(),
                "w1".to_string(),
                "t".to_string()
            )]
        );
    }

    #[test]
    fn test_spawn_claude_pins_model_at_bg_spawn_not_pane_flag() {
        let _guard = setup();
        mock_claude_bg_up("abcd1234", "sess-registry");
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.model = "opus".into();
                o.is_first = true;
                o.skill = "none".into();
                o.cli = "claude".into();
            }),
        )
        .unwrap();
        // model is a bg-spawn flag (durable in respawnFlags), not a viewer flag
        assert_eq!(
            hook(|h| h.spawns[0].extra_args.clone()),
            vec!["--model", "opus"]
        );
        assert!(!calls()[0].contains("--model"));
    }

    #[test]
    fn test_spawn_codex_pins_model_at_mint_not_flag() {
        let _guard = setup();
        mock_daemon_up();
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.model = "gpt-5.2".into();
                o.is_first = true;
                o.skill = "none".into();
                o.cli = "codex".into();
            }),
        )
        .unwrap();
        let startup_cmd = calls()[0].clone();
        // model is a thread/start property, not a resume flag
        assert!(!startup_cmd.contains("-m 'gpt-5.2'"));
        assert_eq!(
            hook(|h| h.codex_minted.clone()),
            vec![(
                "/tmp".to_string(),
                "t.w1".to_string(),
                "gpt-5.2".to_string()
            )]
        );
    }

    #[test]
    fn test_spawn_rejects_unknown_cli() {
        let _guard = setup();
        let err = err_of(Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.skill = "none".into();
                o.cli = "vim".into();
            }),
        ));
        assert!(err.contains("unsupported cli"), "{err}");
    }

    #[test]
    fn test_spawn_claude_fork_mints_a_new_job_from_the_session() {
        let _guard = setup();
        mock_claude_bg_up("abcd1234", "sess-registry");
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.is_first = true;
                o.skill = "none".into();
                o.cli = "claude".into();
                o.session_id = Some("sess-abc".into());
            }),
        )
        .unwrap();
        // fork mode: a NEW bg job branches the source session server-side
        assert_eq!(
            hook(|h| h.spawns[0].extra_args.clone()),
            vec!["-r", "sess-abc", "--fork-session"]
        );
        assert!(calls()[0].contains("--resume 'abcd1234'")); // the pane attaches to the fork
    }

    #[test]
    fn test_spawn_codex_resume_uses_fork_subcommand() {
        let _guard = setup();
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.is_first = true;
                o.skill = "none".into();
                o.cli = "codex".into();
                o.session_id = Some("sess-abc".into());
            }),
        )
        .unwrap();
        let startup_cmd = calls()[0].clone();
        assert!(startup_cmd
            .split(" && ")
            .last()
            .unwrap()
            .starts_with("hive codex -c check_for_update_on_startup=false fork 'sess-abc'"));
        // codex fork does not take --model; model flag should not appear
        assert!(!startup_cmd.contains("-m"));
    }

    #[test]
    fn test_spawn_codex_new_session_resumes_minted_thread() {
        let _guard = setup();
        mock_daemon_up();
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cwd = "/work/dir".into();
                o.is_first = true;
                o.skill = "none".into();
                o.cli = "codex".into();
            }),
        )
        .unwrap();
        let startup_cmd = calls()[0].clone();
        // hive minted the thread, recorded the pane binding, trusted the cwd, and
        // the pane attaches with `resume <tid>` — the managed launcher injects
        // --remote/--cd itself, so the spawn command carries neither.
        assert!(startup_cmd.contains("resume 'tid-minted'"));
        assert!(!startup_cmd.contains("--remote"));
        assert!(!startup_cmd.contains("--cd"));
        assert_eq!(
            hook(|h| h.codex_minted.clone()),
            vec![("/work/dir".to_string(), "t.w1".to_string(), "".to_string())]
        );
        assert_eq!(hook(|h| h.codex_trusted.clone()), vec!["/work/dir"]);
        assert_eq!(
            hook(|h| h.codex_records.clone()),
            vec![(
                "%0".to_string(),
                "tid-minted".to_string(),
                "/work/dir".to_string()
            )]
        );
    }

    #[test]
    fn test_spawn_codex_mint_failure_kills_pane_and_fails() {
        let _guard = setup();
        mock_daemon_up();
        hook(|h| h.start_member_thread = None);
        let err = err_of(Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cwd = "/work/dir".into();
                o.is_first = true;
                o.skill = "none".into();
                o.cli = "codex".into();
            }),
        ));
        assert!(err.contains("thread identity"), "{err}");
        assert_eq!(hook(|h| h.killed.clone()), vec!["%0"]);
        assert!(calls().is_empty()); // no startup command was ever sent
    }

    #[test]
    fn test_spawn_codex_preconnects_2nd_client_with_workspace() {
        // With a workspace, spawn asks the hived to bring its client online
        // before the member's first turn.
        let _guard = setup();
        mock_daemon_up();
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cwd = "/work/dir".into();
                o.is_first = true;
                o.skill = "none".into();
                o.cli = "codex".into();
                o.workspace = "/tmp/ws".into();
            }),
        )
        .unwrap();
        assert_eq!(hook(|h| h.connects_codex.clone()), vec!["/tmp/ws"]);
    }

    #[test]
    fn test_spawn_codex_skips_preconnect_without_workspace() {
        let _guard = setup();
        mock_daemon_up();
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cwd = "/work/dir".into();
                o.is_first = true;
                o.skill = "none".into();
                o.cli = "codex".into();
            }),
        )
        .unwrap(); // no workspace → no eager preconnect, lazy tick covers it
        assert!(hook(|h| h.connects_codex.clone()).is_empty());
    }

    #[test]
    fn test_spawn_codex_new_session_refuses_when_daemon_fails() {
        // Embedded codex is unsupported: if the shared daemon cannot bind, spawn
        // must not launch a raw codex as a team member — it kills the pane it
        // just split and raises instead of leaving a stateless tagged member.
        let _guard = setup(); // spawn_daemon defaults to false
        let err = err_of(Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cwd = "/work/dir".into();
                o.is_first = true;
                o.skill = "none".into();
                o.cli = "codex".into();
            }),
        ));
        assert!(err.contains("daemon-only"), "{err}");
        assert_eq!(hook(|h| h.killed.clone()), vec!["%0"]); // the split pane is cleaned up
        assert!(calls().is_empty()); // no startup command was ever sent
    }

    #[test]
    fn test_spawn_codex_daemon_fail_in_place_clears_tags_instead_of_killing() {
        // split_window=false spawns into the caller's own shell pane: on daemon
        // failure that pane must survive, but the hive tags just written are
        // undone.
        let _guard = setup();
        let err = err_of(Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cwd = "/work/dir".into();
                o.is_first = true;
                o.skill = "none".into();
                o.cli = "codex".into();
                o.split_window = false;
            }),
        ));
        assert!(err.contains("daemon-only"), "{err}");
        assert!(hook(|h| h.killed.clone()).is_empty());
        assert_eq!(hook(|h| h.cleared_tags.clone()), vec!["%0"]);
        assert!(calls().is_empty());
    }

    #[test]
    fn test_spawn_codex_fork_does_not_start_daemon() {
        // The pane's `hive codex fork <sid>` binds the daemon, forks server-side
        // and records the pane's thread itself; spawn stays out of it.
        let _guard = setup();
        hook(|h| h.codex_spawn_daemon = true);
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cwd = "/work/dir".into();
                o.is_first = true;
                o.skill = "none".into();
                o.cli = "codex".into();
                o.session_id = Some("sess-abc".into());
            }),
        )
        .unwrap();
        let startup_cmd = calls()[0].clone();
        assert!(startup_cmd.contains("fork") && startup_cmd.contains("sess-abc"));
        assert!(!startup_cmd.contains("--remote")); // the launcher injects it
        assert!(hook(|h| h.codex_started.clone()).is_empty()); // daemon not started by spawn for a fork
    }

    #[test]
    fn test_spawn_grok_launches_with_minted_session_id_and_model_flag() {
        let _guard = setup();
        mock_grok_leader_up();
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.model = "grok-4.6".into();
                o.cwd = "/work/dir".into();
                o.is_first = true;
                o.skill = "none".into();
                o.cli = "grok".into();
            }),
        )
        .unwrap();
        let launch = launch_of(&calls()[0]);
        let (pane, session_id, cwd) = hook(|h| h.grok_sessions[0].clone());
        assert_eq!((pane.as_str(), cwd.as_str()), ("%0", "/work/dir"));
        assert_eq!(
            launch.split_whitespace().collect::<Vec<_>>(),
            vec![
                "hive",
                "grok",
                "--session-id",
                session_id.as_str(),
                "-m",
                "'grok-4.6'"
            ]
        );
    }

    #[test]
    fn test_spawn_grok_resume_keeps_the_session_id_and_drops_fork_flag() {
        let _guard = setup();
        mock_grok_leader_up();
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.skill = "none".into();
                o.cli = "grok".into();
                o.session_id = Some("sess-abc".into());
                o.session_mode = "resume".into();
            }),
        )
        .unwrap();
        let launch = launch_of(&calls()[0]);
        assert_eq!(
            launch.split_whitespace().collect::<Vec<_>>(),
            vec!["hive", "grok", "--resume", "'sess-abc'"]
        );
        // the pane drives the resumed session itself — no new id is minted
        assert_eq!(
            hook(|h| h.grok_sessions.clone()),
            vec![("%0".to_string(), "sess-abc".to_string(), "/tmp".to_string())]
        );
    }

    #[test]
    fn test_spawn_grok_fork_mints_a_new_session_id_for_the_branch() {
        let _guard = setup();
        mock_grok_leader_up();
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.skill = "none".into();
                o.cli = "grok".into();
                o.session_id = Some("sess-abc".into());
            }),
        )
        .unwrap();
        let launch = launch_of(&calls()[0]);
        let forked_id = hook(|h| h.grok_sessions[0].1.clone());
        assert_ne!(forked_id, "sess-abc");
        assert_eq!(
            launch.split_whitespace().collect::<Vec<_>>(),
            vec![
                "hive",
                "grok",
                "--session-id",
                forked_id.as_str(),
                "--resume",
                "'sess-abc'",
                "--fork-session"
            ]
        );
    }

    #[test]
    fn test_spawn_grok_refuses_when_leader_daemon_fails() {
        // Grok runtime lives on the per-pane leader: without one the pane would
        // run a grok nobody can reach, so spawn gives the pane back and raises.
        let _guard = setup(); // grok spawn_daemon defaults to false
        let err = err_of(Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.skill = "none".into();
                o.cli = "grok".into();
            }),
        ));
        assert!(err.contains("leader-only"), "{err}");
        assert_eq!(hook(|h| h.killed.clone()), vec!["%0"]);
        assert!(calls().is_empty()); // no launch command was ever sent
        assert!(hook(|h| h.grok_sessions.clone()).is_empty()); // and no session record left behind
    }

    #[test]
    fn test_spawn_grok_leader_fail_in_place_clears_tags_instead_of_killing() {
        let _guard = setup();
        let err = err_of(Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.skill = "none".into();
                o.cli = "grok".into();
                o.split_window = false;
            }),
        ));
        assert!(err.contains("leader-only"), "{err}");
        assert!(hook(|h| h.killed.clone()).is_empty());
        assert_eq!(hook(|h| h.cleared_tags.clone()), vec!["%0"]);
        assert!(calls().is_empty());
    }

    #[test]
    fn test_spawn_grok_connects_the_2nd_client_once_the_session_is_ready() {
        // the client can only load a session the TUI has opened, so the connect
        // follows readiness instead of racing it
        let _guard = setup();
        mock_grok_leader_up();
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cwd = "/work/dir".into();
                o.skill = "none".into();
                o.cli = "grok".into();
                o.workspace = "/tmp/ws".into();
            }),
        )
        .unwrap();
        assert_eq!(
            hook(|h| h.event_order.clone()),
            vec!["ready:%0", "connect:/tmp/ws:%0"]
        );
    }

    #[test]
    fn test_spawn_grok_skips_the_connect_when_readiness_times_out() {
        let _guard = setup();
        mock_grok_leader_up();
        hook(|h| h.wait_grok_ready = Some(false));
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cwd = "/work/dir".into();
                o.skill = "none".into();
                o.cli = "grok".into();
                o.workspace = "/tmp/ws".into();
            }),
        )
        .unwrap();
        assert!(hook(|h| h.connects_grok.clone()).is_empty()); // nothing to load yet; the lazy connect retries
    }

    #[test]
    fn test_spawn_grok_skips_preconnect_without_workspace() {
        let _guard = setup();
        mock_grok_leader_up();
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cwd = "/work/dir".into();
                o.skill = "none".into();
                o.cli = "grok".into();
            }),
        )
        .unwrap(); // lazy connect on the next tick covers it
        assert!(hook(|h| h.connects_grok.clone()).is_empty());
    }

    // --- send --------------------------------------------------------------

    #[test]
    fn test_send_codex_uses_turn_start_when_daemon_accepts() {
        // pin the process probe: the real one inspects the live tmux pane "%3",
        // which detects whatever CLI happens to run there on this machine
        let _guard = setup();
        pin_cli_probe("codex");
        hook(|h| h.codex_send_to_pane = Some("turnStartAccepted"));
        member("w", "t", "%3", "codex").send("hi").unwrap();
        assert_eq!(
            hook(|h| h.codex_sent.clone()),
            vec![("%3".to_string(), "hi".to_string())]
        );
        assert!(calls().is_empty()); // no keystroke fallback when daemon accepts
    }

    #[test]
    fn test_send_uses_detected_codex_daemon_when_stored_cli_is_stale() {
        let _guard = setup();
        pin_cli_probe("codex");
        hook(|h| h.codex_send_to_pane = Some("turnStartAccepted"));
        member("w", "t", "%3", "claude").send("hi").unwrap();
        assert_eq!(
            hook(|h| h.codex_sent.clone()),
            vec![("%3".to_string(), "hi".to_string())]
        );
        assert!(calls().is_empty());
    }

    #[test]
    fn test_send_codex_accepted_returns_classification_without_keystrokes() {
        let _guard = setup();
        pin_cli_probe("codex");
        hook(|h| h.codex_send_to_pane = Some("turnStartAccepted"));
        let accepted = member("w", "t", "%3", "codex").send("hi").unwrap();
        assert_eq!(accepted, "turnStartAccepted");
        assert!(calls().is_empty()); // native transport only — the composer is never touched
    }

    #[test]
    fn test_send_codex_transport_failure_raises_without_keystrokes() {
        // VAL-5: any codex transport failure (no daemon, no thread, RPC error,
        // exception — the adapter folds them all to None) raises DeliveryError
        // and never falls back to keystroke injection.
        let _guard = setup();
        pin_cli_probe("codex");
        hook(|h| h.codex_send_to_pane = None);
        assert!(member("w", "t", "%3", "codex").send("hi").is_err());
        assert!(calls().is_empty());
    }

    #[test]
    fn test_send_grok_queues_the_prompt_on_the_leader() {
        let _guard = setup();
        pin_cli_probe("grok");
        hook(|h| h.grok_send_to_pane = Some("sessionPromptQueued"));
        let accepted = member("w", "t", "%3", "grok").send("hi").unwrap();
        assert_eq!(accepted, "sessionPromptQueued");
        assert_eq!(
            hook(|h| h.grok_sent.clone()),
            vec![("%3".to_string(), "hi".to_string())]
        );
        assert!(calls().is_empty()); // native transport only — the composer is never touched
    }

    #[test]
    fn test_send_grok_transport_failure_raises_without_keystrokes() {
        // Every grok transport failure (no leader, no session record, RPC error,
        // ack timeout — the adapter folds them all to None) raises DeliveryError
        // and never falls back to keystroke injection.
        let _guard = setup();
        pin_cli_probe("grok");
        hook(|h| h.grok_send_to_pane = None);
        assert!(member("w", "t", "%3", "grok").send("hi").is_err());
        assert!(calls().is_empty());
    }

    #[test]
    fn test_send_claude_writes_to_the_engine_inbox_as_the_member_address() {
        let _guard = setup();
        pin_cli_probe("claude");
        let engine = fake_engine(4321, "abcd1234", "sess-registry");
        pin_job(&engine.job_id.clone(), engine.clone());
        hook(|h| h.sessions_send = Some(claude_sessions::ACCEPTED_UDS_WRITE));
        let accepted = member("w", "t", "%3", "claude").send("hi").unwrap();
        assert_eq!(accepted, "udsWriteAccepted");
        // the engine's own session id rides the frame: claude drops a
        // mismatching one, so a recycled socket cannot take a dead session's
        // mail
        assert_eq!(
            hook(|h| h.inbox_writes.clone()),
            vec![(
                engine.socket_path.clone(),
                "hi".to_string(),
                "t.w".to_string(),
                engine.session_id.clone()
            )]
        );
        assert!(calls().is_empty()); // native transport only — the composer is never touched
    }

    #[test]
    fn test_send_claude_resolves_the_engine_from_the_pane_job_record() {
        // the delivery address is derived pane -> job record -> engine entry;
        // nothing on the pane tty (the attach viewer!) is ever what gets
        // messaged
        let _guard = setup();
        pin_cli_probe("claude");
        hook(|h| {
            h.job_id_for_pane = Some("beef4321".to_string());
            h.engines_by_job.insert(
                "beef4321".to_string(),
                fake_engine(4321, "beef4321", "sess-registry"),
            );
            h.sessions_send = Some(claude_sessions::ACCEPTED_UDS_WRITE);
        });
        member("w", "t", "%3", "claude").send("hi").unwrap();
        assert_eq!(hook(|h| h.pane_job_lookups.clone()), vec!["%3"]);
        assert_eq!(hook(|h| h.seen_jobs.clone()), vec!["beef4321"]); // the pane's own record keys the engine
    }

    #[test]
    fn test_send_claude_without_job_record_raises() {
        let _guard = setup();
        pin_cli_probe("claude");
        let err = member("w", "t", "%3", "claude").send("hi").unwrap_err();
        assert!(err.0.contains("no bg job record"), "{err}");
        assert!(hook(|h| h.inbox_writes.clone()).is_empty()); // no socket to write to; nothing was attempted
        assert!(calls().is_empty());
    }

    #[test]
    fn test_send_claude_asleep_engine_is_woken_then_delivered() {
        // a parked engine (supervisor idles jobs after ~1h) is not a dead
        // member: the ledger still lists the job, the wake revives it, delivery
        // proceeds
        let _guard = setup();
        pin_cli_probe(""); // no viewer on the pane either
        let engine = fake_engine(4321, "beef4321", "sess-registry");
        hook(|h| {
            h.job_id_for_pane = Some("beef4321".to_string());
            h.job_row_ids = vec!["beef4321".to_string()];
            h.ensure_engine = Some(Some(engine.clone()));
            h.sessions_send = Some(claude_sessions::ACCEPTED_UDS_WRITE);
        });
        let accepted = member("w", "t", "%3", "claude").send("hi").unwrap();
        assert_eq!(accepted, "udsWriteAccepted");
        assert_eq!(hook(|h| h.wakes.clone()), vec!["beef4321"]);
        assert_eq!(
            hook(|h| h.inbox_writes.clone()),
            vec![(
                engine.socket_path.clone(),
                "hi".to_string(),
                "t.w".to_string(),
                engine.session_id.clone()
            )]
        );
        assert!(calls().is_empty());
    }

    #[test]
    fn test_send_claude_gone_job_raises() {
        // the ledger no longer lists the job (removed): nothing to wake
        let _guard = setup();
        pin_cli_probe("");
        hook(|h| h.job_id_for_pane = Some("beef4321".to_string()));
        let err = member("w", "t", "%3", "claude").send("hi").unwrap_err();
        assert!(err.0.contains("gone"), "{err}");
        assert!(hook(|h| h.wakes.clone()).is_empty()); // nothing listed → no wake attempt
        assert!(calls().is_empty());
    }

    #[test]
    fn test_send_claude_not_listening_raises_without_keystrokes() {
        let _guard = setup();
        pin_cli_probe("claude");
        pin_job("abcd1234", fake_engine(4321, "abcd1234", "sess-registry"));
        hook(|h| h.sessions_send = None);
        let err = member("w", "t", "%3", "claude").send("hi").unwrap_err();
        assert!(err.0.contains("not listening"), "{err}");
        assert!(calls().is_empty());
    }

    #[test]
    fn test_send_claude_write_timeout_raises_and_is_not_an_accept() {
        // the listener took the connection but never read the frame: a stalled
        // session, reported as a failure rather than returned as a
        // classification
        let _guard = setup();
        pin_cli_probe("claude");
        pin_job("abcd1234", fake_engine(4321, "abcd1234", "sess-registry"));
        hook(|h| h.sessions_send = Some(claude_sessions::WRITE_TIMED_OUT));
        let err = member("w", "t", "%3", "claude").send("hi").unwrap_err();
        assert!(err.0.contains("did not drain the message in time"), "{err}");
        assert!(calls().is_empty());
    }

    #[test]
    fn test_send_unknown_profile_raises_without_keystrokes() {
        // no CLI process on the pane TTY: the send gate refuses before any
        // transport
        let _guard = setup();
        pin_cli_probe("");
        assert!(member("w", "t", "%3", "mystery").send("hi").is_err());
        assert!(calls().is_empty());
    }

    #[test]
    fn test_send_claude_never_uses_codex_daemon() {
        let _guard = setup();
        pin_cli_probe("claude");
        pin_job("abcd1234", fake_engine(4321, "abcd1234", "sess-registry"));
        hook(|h| {
            h.codex_send_to_pane = Some("turnStartAccepted");
            h.sessions_send = Some(claude_sessions::ACCEPTED_UDS_WRITE);
        });
        member("w", "t", "%3", "claude").send("hi").unwrap();
        assert!(hook(|h| h.codex_sent.clone()).is_empty()); // codex daemon path not taken for claude
        assert_eq!(hook(|h| h.inbox_writes.clone()).len(), 1); // claude delivers over its session inbox
    }

    #[test]
    fn test_send_codex_member_never_routes_into_a_stale_claude_record() {
        // A blind probe (tmux busy, nothing on the pane tty) must not hand a
        // codex member's message to whatever claude job the pane id used to
        // host.
        let _guard = setup();
        pin_cli_probe("");
        stale_claude_record();
        let err = member("w", "t", "%3", "codex").send("hi").unwrap_err();
        assert!(err.0.contains("no live CLI process"), "{err}");
        assert!(hook(|h| h.inbox_writes.clone()).is_empty()); // the other member's inbox was never opened
        assert!(calls().is_empty());
    }

    #[test]
    fn test_send_codex_member_refuses_a_pane_probed_as_claude() {
        // The probe itself reads the stale job record as evidence of a live
        // claude, so 'the probe said claude' is not enough — the member hive
        // spawned on this pane is codex, and its transport is the daemon.
        let _guard = setup();
        pin_cli_probe("claude");
        stale_claude_record();
        hook(|h| h.codex_send_to_pane = Some("turnStartAccepted"));
        let err = member("w", "t", "%3", "codex").send("hi").unwrap_err();
        assert!(err.0.contains("does not deliver across CLIs"), "{err}");
        assert!(hook(|h| h.inbox_writes.clone()).is_empty());
        assert!(hook(|h| h.codex_sent.clone()).is_empty()); // a claude-looking pane is not a codex thread either
        assert!(calls().is_empty());
    }

    // --- draft guard ---------------------------------------------------------

    #[test]
    fn test_save_and_clear_draft_keeps_the_draft_when_the_buffer_save_fails() {
        // tmux never took the buffer: clearing the composer now would destroy
        // the only copy of the user's draft.
        let _guard = setup();
        hook(|h| {
            h.supported_profile = true;
            h.parse_draft = Some("unsent thought".to_string());
            h.load_buffer_fails = true;
        });
        assert_eq!(_save_and_clear_draft("%3", "claude"), "");
        assert!(hook(|h| h.draft_cleared.clone()).is_empty());
    }

    #[test]
    fn test_save_and_clear_draft_still_restores_when_the_clear_fails() {
        // The buffer holds the draft, so a half-done clear must still hand the
        // restore its buffer name.
        let _guard = setup();
        hook(|h| {
            h.supported_profile = true;
            h.parse_draft = Some("unsent thought".to_string());
            h.clear_input_fails = true;
        });
        assert_eq!(_save_and_clear_draft("%3", "claude"), "hive_draft_3");
    }

    // --- session detection ---------------------------------------------------

    #[test]
    fn test_spawn_claude_skips_session_detection() {
        let _guard = setup();
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.is_first = true;
                o.skill = "none".into();
                o.cli = "claude".into();
            }),
        )
        .unwrap();
        assert!(
            hook(|h| h.resolved_session_panes.clone()).is_empty(),
            "should not resolve session for claude"
        );
    }

    #[test]
    fn test_detect_current_session_id_delegates_to_resolve() {
        let _guard = setup();
        hook(|h| {
            h.session_ids_by_pane
                .insert("%11".to_string(), "map-sess-1".to_string());
        });
        assert_eq!(
            detect_current_session_id("/tmp/test", "", "%11"),
            Some("map-sess-1".to_string())
        );
        assert_eq!(detect_current_session_id("/tmp/test", "", "%99"), None);
    }

    // --- session_mode: fork vs resume (VAL B5-B7) ----------------------------

    #[test]
    fn test_spawn_claude_fork_and_resume_session_semantics() {
        let _guard = setup();
        mock_claude_bg_up("abcd1234", "sess-registry");

        // fork: a new bg job branches the source session
        Agent::spawn(
            "w",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cli = "claude".into();
                o.session_id = Some("sess-1".into());
            }),
        )
        .unwrap();
        assert_eq!(
            hook(|h| h.spawns.last().unwrap().extra_args.clone()),
            vec!["-r", "sess-1", "--fork-session"]
        );
        assert!(hook(|h| h.wakes.clone()).is_empty());

        // resume: the id is the durable jobId — wake it, mint nothing
        hook(|h| h.calls.clear());
        Agent::spawn(
            "w",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cli = "claude".into();
                o.session_id = Some("cafe0123".into());
                o.session_mode = "resume".into();
            }),
        )
        .unwrap();
        assert_eq!(hook(|h| h.wakes.clone()), vec!["cafe0123"]);
        assert_eq!(hook(|h| h.spawns.len()), 1); // unchanged from the fork above
    }

    #[test]
    fn test_spawn_codex_fork_delegates_to_hive_codex() {
        let _guard = setup();
        // spawn itself never touches the daemon for a fork (the pane's `hive
        // codex` binds it); the default spawn_daemon mock returning false must
        // not matter.
        Agent::spawn(
            "w",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cli = "codex".into();
                o.session_id = Some("roll-1".into());
            }),
        )
        .unwrap();
        let launch = launch_of(&calls()[0]);
        assert!(launch.starts_with("hive codex "), "{launch}");
        assert!(launch.contains("fork 'roll-1'"));
        assert!(!launch.contains("--remote")); // the daemon binding is `hive codex`'s job
        assert!(!launch.contains("resume"));
    }

    #[test]
    fn test_spawn_codex_resume_records_thread_and_resumes_it() {
        let _guard = setup();
        mock_daemon_up();
        Agent::spawn(
            "w",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cwd = "/repo".into();
                o.cli = "codex".into();
                o.session_id = Some("roll-1".into());
                o.session_mode = "resume".into();
                o.skill = "none".into();
                o.workspace = "/ws".into();
            }),
        )
        .unwrap();
        let cmd = calls()[0].clone();
        // the resumed session's id IS its threadId: recorded, then resumed
        // through the managed launcher (which injects --remote/--cd itself)
        assert!(cmd.contains("resume 'roll-1'"));
        assert!(!cmd.contains("fork"));
        assert!(!cmd.contains("--remote"));
        assert!(hook(|h| h.codex_minted.clone()).is_empty()); // nothing minted on resume
        assert_eq!(
            hook(|h| h.codex_records.clone()),
            vec![("%0".to_string(), "roll-1".to_string(), "/repo".to_string())]
        );
        assert_eq!(hook(|h| h.connects_codex.clone()), vec!["/ws"]);
    }

    #[test]
    fn test_spawn_codex_resume_daemon_failure_never_falls_back_embedded() {
        let _guard = setup();

        // split path: new pane is killed
        let err = err_of(Agent::spawn(
            "w",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cli = "codex".into();
                o.session_id = Some("roll-1".into());
                o.session_mode = "resume".into();
            }),
        ));
        assert!(err.contains("daemon"), "{err}");
        assert_eq!(hook(|h| h.killed.clone()), vec!["%0"]);
        assert!(calls().is_empty()); // no command was ever typed — no embedded fallback

        // in-place path: tags/title cleared instead
        let err = err_of(Agent::spawn(
            "w",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cli = "codex".into();
                o.session_id = Some("roll-1".into());
                o.session_mode = "resume".into();
                o.split_window = false;
            }),
        ));
        assert!(err.contains("daemon"), "{err}");
        assert_eq!(hook(|h| h.cleared_tags.clone()), vec!["%0"]);
        assert!(hook(|h| h.titles.clone()).contains(&("%0".to_string(), "".to_string())));
        assert!(calls().is_empty());
    }

    #[test]
    fn test_spawn_rejects_unknown_session_mode() {
        let _guard = setup();
        let err = err_of(Agent::spawn(
            "w",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cli = "claude".into();
                o.session_id = Some("s".into());
                o.session_mode = "clone".into();
            }),
        ));
        assert!(err.contains("session_mode"), "{err}");
    }

    // --- readiness oracles: runtime signals, not screen text (VAL 1-7) ------

    #[test]
    fn test_spawn_claude_engine_readiness_skips_banner_and_settle() {
        let _guard = setup();

        // fresh and resume: the engine's registry entry is the oracle, the
        // banner (the pane only shows an attach viewer) is not consulted at all
        Agent::spawn("w", "t", "%0", spawn_opts(|o| o.cli = "claude".into())).unwrap();
        Agent::spawn(
            "w",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cli = "claude".into();
                o.session_id = Some("cafe0123".into());
                o.session_mode = "resume".into();
            }),
        )
        .unwrap();

        assert!(!hook(|h| h.sleeps.clone()).contains(&1.0)); // no fixed 1s settle either
    }

    #[test]
    fn test_spawn_codex_waits_on_process_not_banner() {
        let _guard = setup();
        mock_daemon_up();
        Agent::spawn(
            "v",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cli = "codex".into();
                o.skill = "none".into();
                o.session_id = Some("roll-1".into());
                o.session_mode = "resume".into();
            }),
        )
        .unwrap();
        assert_eq!(hook(|h| h.waited_codex.clone()), vec!["%0"]);
    }

    #[test]
    fn test_wait_codex_attached_polls_for_the_codex_process() {
        let _guard = setup();
        hook(|h| {
            h.cli_probe_seq = vec![None, Some("claude".to_string()), Some("codex".to_string())];
        });
        // None and a non-codex profile are both "not attached yet"
        assert!(_wait_codex_attached("%9", 60.0, 0.0));
    }

    #[test]
    fn test_wait_codex_attached_timeout_is_deterministic_and_nonfatal() {
        let _guard = setup();
        assert!(!_wait_codex_attached("%9", 0.0, 0.0));

        // spawn survives a readiness timeout and still completes
        mock_daemon_up();
        hook(|h| h.wait_codex_attached = Some(false));
        let agent = Agent::spawn(
            "v",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cli = "codex".into();
                o.skill = "hive".into();
            }),
        )
        .unwrap();
        assert_eq!(agent.pane_id, "%0");
    }

    #[test]
    fn test_spawn_grok_waits_on_the_minted_session_dir_not_the_banner() {
        let _guard = setup();
        hook(|h| h.grok_spawn_daemon = true);
        Agent::spawn(
            "w",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cli = "grok".into();
                o.skill = "none".into();
            }),
        )
        .unwrap();
        let waited = hook(|h| h.waited_grok.clone());
        let minted = hook(|h| h.grok_sessions[0].1.clone());
        assert_eq!(waited, vec![("%0".to_string(), minted)]); // the id hive minted, not the pane's cwd
    }

    #[test]
    fn test_wait_grok_session_ready_sees_the_session_dir_and_is_nonfatal() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("GROK_HOME", tmp.path());
        {
            let _guard = setup();
            hook(|h| h.wait_grok_ready = None); // run the real wait
            pin_cli_probe("grok");
            assert!(!_wait_grok_session_ready("%0", "sess-x", 0.0, 0.0));

            // grok creates $GROK_HOME/sessions/<quoted cwd>/<sid>/ at startup
            std::fs::create_dir_all(tmp.path().join("sessions").join("%2Ftmp").join("sess-x"))
                .unwrap();
            assert!(_wait_grok_session_ready("%0", "sess-x", 0.0, 0.0));

            // on resume the dir predates the launch, so the pane's own grok
            // must be up
            pin_cli_probe("");
            assert!(!_wait_grok_session_ready("%0", "sess-x", 0.0, 0.0));
        }

        // a readiness timeout is not fatal: spawn still completes
        let _guard = setup();
        mock_grok_leader_up();
        hook(|h| h.wait_grok_ready = Some(false));
        let agent = Agent::spawn(
            "v",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cli = "grok".into();
                o.skill = "hive".into();
            }),
        )
        .unwrap();
        assert_eq!(agent.pane_id, "%0");
    }

    #[test]
    fn test_spawn_codex_fork_waits_on_process_not_banner() {
        let _guard = setup();
        Agent::spawn(
            "f",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cli = "codex".into();
                o.session_id = Some("roll-1".into()); // fork mode
            }),
        )
        .unwrap();
        assert_eq!(hook(|h| h.waited_codex.clone()), vec!["%0"]);
    }

    // --- V1: the launch never execs — the pane shell must survive the CLI ---

    /// Single-quote-aware tokenizer (hive quotes with single quotes only), so
    /// quoted prompt text cannot green this on substrings.
    fn sq_tokens(segment: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut cur = String::new();
        let mut in_quote = false;
        for c in segment.chars() {
            match c {
                '\'' => in_quote = !in_quote,
                c if c.is_whitespace() && !in_quote => {
                    if !cur.is_empty() {
                        tokens.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        if !cur.is_empty() {
            tokens.push(cur);
        }
        tokens
    }

    /// The CLI must run as the shell's foreground child: no `exec` token may
    /// appear in the launch pipeline.
    fn assert_launch_keeps_shell(startup_cmd: &str) {
        for segment in startup_cmd.split("&&") {
            assert!(
                !sq_tokens(segment).iter().any(|t| t == "exec"),
                "{startup_cmd}"
            );
        }
    }

    #[test]
    fn test_launch_guard_catches_the_old_exec_form() {
        // negative control: the pre-change launch shape must trip the assertion
        let result = std::panic::catch_unwind(|| {
            assert_launch_keeps_shell("cd '/w' && exec /bin/codex --remote 'unix:///s'")
        });
        assert!(result.is_err());
    }

    #[test]
    fn test_spawn_claude_fresh_launch_keeps_shell() {
        let _guard = setup();
        Agent::spawn("w1", "t", "%0", spawn_opts(|o| o.skill = "none".into())).unwrap();
        let startup_cmd = calls()[0].clone();
        assert_launch_keeps_shell(&startup_cmd);
        assert!(startup_cmd.contains("claude"));
    }

    #[test]
    fn test_spawn_claude_resume_launch_keeps_shell() {
        let _guard = setup();
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.skill = "none".into();
                o.session_id = Some("cafe0123".into());
                o.session_mode = "resume".into();
            }),
        )
        .unwrap();
        let startup_cmd = calls()[0].clone();
        assert_launch_keeps_shell(&startup_cmd);
        assert!(startup_cmd.contains("--resume 'cafe0123'")); // the pane reattaches the job
    }

    #[test]
    fn test_spawn_codex_daemon_native_launch_keeps_shell() {
        let _guard = setup();
        mock_daemon_up();
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.cwd = "/work/dir".into();
                o.is_first = true;
                o.skill = "none".into();
                o.cli = "codex".into();
            }),
        )
        .unwrap();
        let startup_cmd = calls()[0].clone();
        assert_launch_keeps_shell(&startup_cmd);
        assert!(startup_cmd.contains("resume 'tid-minted'")); // minted-thread attach shape
    }

    #[test]
    fn test_spawn_codex_fork_shortcut_launch_keeps_shell() {
        let _guard = setup();
        Agent::spawn(
            "w1",
            "t",
            "%0",
            spawn_opts(|o| {
                o.skill = "none".into();
                o.cli = "codex".into();
                o.session_id = Some("sess-abc".into());
            }),
        )
        .unwrap();
        let startup_cmd = calls()[0].clone();
        assert_launch_keeps_shell(&startup_cmd);
        assert!(startup_cmd.contains("fork") && startup_cmd.contains("sess-abc"));
    }

    #[test]
    fn test_spawn_skill_ref_is_bare_for_grok_and_qualified_for_claude() {
        // grok/codex register plugin skills by bare name (/hive, $hive); claude
        // addresses them fully qualified (/hive:hive). /skills in grok only
        // opens the picker — never format the grok launch with it.
        let _guard = setup();
        mock_claude_bg_up("abcd1234", "sess-registry");
        hook(|h| {
            h.grok_spawn_daemon = true;
            h.wait_grok_ready = Some(true);
        });

        Agent::spawn(
            "g",
            "t",
            "%0",
            spawn_opts(|o| {
                o.is_first = true;
                o.cli = "grok".into();
                o.skill = "hive:hive".into();
            }),
        )
        .unwrap();
        let grok_all = calls().join(" ");
        assert!(grok_all.contains("/hive"));
        assert!(!grok_all.contains("/skills") && !grok_all.contains("/hive:hive"));

        hook(|h| h.calls.clear());
        Agent::spawn(
            "c",
            "t",
            "%0",
            spawn_opts(|o| {
                o.is_first = true;
                o.cli = "claude".into();
                o.skill = "hive:hive".into();
            }),
        )
        .unwrap();
        // claude's skill rides the bg spawn prompt, fully qualified
        assert!(hook(|h| h.spawns.last().unwrap().prompt.clone()).starts_with("/hive:hive"));
    }

    // --- headless members (tests/unit/test_agent_headless.py) ----------------

    #[test]
    fn test_headless_codex_send_routes_by_thread() {
        let _guard = setup();
        hook(|h| h.codex_send_to_thread = Some("turnStartAccepted"));
        assert_eq!(
            headless("codex", Some("sid-1")).send("hi").unwrap(),
            "turnStartAccepted"
        );
        assert_eq!(
            hook(|h| h.codex_sent_thread.clone()),
            vec![("sid-1".to_string(), "hi".to_string())]
        );
    }

    #[test]
    fn test_headless_codex_send_without_thread_refuses() {
        let _guard = setup();
        assert!(headless("codex", None).send("hi").is_err());
    }

    #[test]
    fn test_headless_grok_send_routes_by_member_key() {
        let _guard = setup();
        hook(|h| h.grok_send_to_key = Some("sessionPromptQueued"));
        assert_eq!(
            headless("grok", Some("sid-1")).send("hi").unwrap(),
            "sessionPromptQueued"
        );
        assert_eq!(
            hook(|h| h.grok_sent_key.clone()),
            vec![("m-honey.rex".to_string(), "hi".to_string())]
        );
    }

    #[test]
    fn test_headless_claude_send_delivers_to_job() {
        let _guard = setup();
        hook(|h| {
            h.job_row_ids = vec!["job-1".to_string()];
            h.engines_by_job
                .insert("job-1".to_string(), fake_engine(4321, "job-1", "sess-9"));
            h.daemon_reply = Some("udsWriteAccepted");
        });
        assert_eq!(
            headless("claude", Some("job-1")).send("hi").unwrap(),
            "udsWriteAccepted"
        );
        assert_eq!(
            hook(|h| h.daemon_replies.clone()),
            vec![("sess-9".to_string(), "hi".to_string())]
        );
    }

    #[test]
    fn test_headless_grok_interrupt_routes_by_member_key() {
        let _guard = setup();
        hook(|h| h.grok_interrupt_key = Some("sessionCancelSent"));
        headless("grok", Some("sid-1")).interrupt().unwrap();
        assert_eq!(
            hook(|h| h.grok_interrupted_keys.clone()),
            vec!["m-honey.rex"]
        );
    }

    #[test]
    fn test_headless_codex_interrupt_routes_by_thread() {
        let _guard = setup();
        hook(|h| h.codex_interrupt_thread = Some("turnInterruptAccepted"));
        headless("codex", Some("sid-1")).interrupt().unwrap();
        assert_eq!(hook(|h| h.codex_interrupted_threads.clone()), vec!["sid-1"]);
    }

    #[test]
    fn test_headless_is_alive_probes_the_engine() {
        let _guard = setup();
        hook(|h| h.codex_daemon_alive = Some(true));
        assert!(headless("codex", Some("sid-1")).is_alive());
        hook(|h| h.codex_daemon_alive = Some(false));
        assert!(!headless("codex", Some("sid-1")).is_alive());

        hook(|h| h.grok_probe_socket = Some(true));
        assert!(headless("grok", Some("sid-1")).is_alive());

        hook(|h| h.job_row_ids = vec!["job-1".to_string()]);
        assert!(headless("claude", Some("job-1")).is_alive()); // asleep is not dead
        hook(|h| h.job_row_ids.clear());
        assert!(!headless("claude", Some("job-1")).is_alive());
    }

    #[test]
    fn test_headless_claude_send_falls_back_to_interactive_session() {
        let _guard = setup();
        hook(|h| h.daemon_reply = Some("udsWriteAccepted"));
        assert_eq!(
            headless("claude", Some("ccd-sid-1")).send("hi").unwrap(),
            "udsWriteAccepted"
        );
        assert_eq!(
            hook(|h| h.daemon_replies.clone()),
            vec![("ccd-sid-1".to_string(), "hi".to_string())]
        );
    }

    #[test]
    fn test_headless_claude_session_send_uses_inbox_socket_fallback() {
        let _guard = setup();
        hook(|h| {
            h.list_sessions = vec![claude_sessions::ClaudeSession {
                name: String::new(),
                pid: 1,
                cwd: String::new(),
                kind: String::new(),
                socket_path: "/tmp/ccd.sock".to_string(),
                session_id: "ccd-sid-1".to_string(),
                title: String::new(),
            }];
            h.sessions_send = Some("accepted");
        });
        assert_eq!(
            headless("claude", Some("ccd-sid-1")).send("hi").unwrap(),
            "accepted"
        );
        let writes = hook(|h| h.inbox_writes.clone());
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, "/tmp/ccd.sock");
        assert_eq!(writes[0].3, "ccd-sid-1");
    }

    #[test]
    fn test_headless_claude_kill_never_stops_an_interactive_session() {
        let _guard = setup();
        headless("claude", Some("ccd-sid-1")).kill();
        assert!(hook(|h| h.stopped.clone()).is_empty());
    }

    // --- the hive paths that ride the key pipe (test_claude_key_pipe.py) -----

    /// Python `_member_pane`.
    fn member_pane(job_id: Option<&str>) {
        hook(|h| {
            h.resolve_profile_name = Some("claude".to_string());
            h.job_id_for_pane = job_id.map(|j| j.to_string());
        });
    }

    #[test]
    fn test_submit_on_a_member_pane_pipes_into_the_job() {
        let _guard = setup();
        member_pane(Some("cafe1234"));
        hook(|h| {
            h.type_into_job_result = Some(KeyResult {
                ok: true,
                confirmed: "transcript".to_string(),
                why: String::new(),
            })
        });
        _submit_interactive_text("%1", "hello", "claude").unwrap();
        assert_eq!(
            hook(|h| h.typed.clone()),
            vec![("cafe1234".to_string(), "hello".to_string())]
        );
        assert!(calls().is_empty()); // a claude member's keyboard must not touch tmux
    }

    #[test]
    fn test_submit_raises_when_the_job_did_not_take_the_text() {
        let _guard = setup();
        member_pane(Some("cafe1234"));
        hook(|h| {
            h.type_into_job_result = Some(KeyResult {
                ok: false,
                confirmed: String::new(),
                why: "never echoed".to_string(),
            })
        });
        let err = err_of(_submit_interactive_text("%1", "hello", "claude"));
        assert!(err.contains("never echoed"), "{err}");
    }

    #[test]
    fn test_a_non_member_claude_pane_still_goes_through_tmux() {
        // No job record: a plain interactive claude TUI, typed at like any
        // other CLI — and refused when that TUI is not running.
        let _guard = setup();
        member_pane(None);
        hook(|h| h.interactive_claude_pid = Some(456));
        _submit_interactive_text("%1", "hello", "claude").unwrap();
        assert_eq!(calls(), vec!["hello", "<Enter>"]);

        hook(|h| h.interactive_claude_pid = None);
        let err = err_of(_submit_interactive_text("%1", "hello", "claude"));
        assert!(err.contains("no interactive claude"), "{err}");
    }

    #[test]
    fn test_a_pane_whose_claude_is_an_attach_viewer_is_refused() {
        // A lost job record must not fall back onto the pane: the claude
        // process there is a viewer, and its composer belongs to whatever
        // session it shows — another member's, or a stranger's.
        let _guard = setup();
        member_pane(None);
        hook(|h| h.interactive_claude_pid = None); // the viewer is not an interactive claude
        let err = err_of(_submit_interactive_text("%1", "hello", "claude"));
        assert!(err.contains("no interactive claude"), "{err}");
        assert!(calls().is_empty());
    }

    #[test]
    fn test_member_interrupt_pipes_escape_into_the_job() {
        let _guard = setup();
        member_pane(Some("cafe1234"));
        hook(|h| {
            h.interrupt_job_result = Some(KeyResult {
                ok: true,
                confirmed: "transcript".to_string(),
                why: String::new(),
            })
        });
        member("red", "probe", "%1", "claude").interrupt().unwrap();
        assert_eq!(hook(|h| h.interrupted_jobs.clone()), vec!["cafe1234"]);
        assert!(calls().is_empty());
    }

    #[test]
    fn test_member_interrupt_without_a_job_record_is_refused() {
        // A lost job record leaves nothing addressable: Escape into the pane
        // would land in whatever session its viewer is showing, so hive
        // refuses instead.
        let _guard = setup();
        member_pane(None);
        let err = err_of(member("red", "probe", "%1", "claude").interrupt());
        assert!(err.contains("no bg job record"), "{err}");
        assert!(calls().is_empty());
    }

    #[test]
    fn test_codex_interrupt_goes_to_the_thread_not_the_pane() {
        let _guard = setup();
        hook(|h| h.codex_interrupt_pane = Some("turnInterruptAccepted"));
        member("blue", "probe", "%2", "codex").interrupt().unwrap();
        assert_eq!(hook(|h| h.codex_interrupted_panes.clone()), vec!["%2"]);
        assert!(calls().is_empty());
    }

    #[test]
    fn test_codex_interrupt_is_refused_when_the_rpc_is_not_accepted() {
        let _guard = setup();
        hook(|h| h.codex_interrupt_pane = None);
        let err = err_of(member("blue", "probe", "%2", "codex").interrupt());
        assert!(err.contains("turn/interrupt"), "{err}");
    }

    #[test]
    fn test_grok_interrupt_goes_to_the_session_not_the_pane() {
        let _guard = setup();
        hook(|h| h.grok_interrupt_pane = Some("sessionCancelSent"));
        member("grey", "probe", "%3", "grok").interrupt().unwrap();
        assert_eq!(hook(|h| h.grok_interrupted_panes.clone()), vec!["%3"]);
        assert!(calls().is_empty());
    }

    #[test]
    fn test_grok_interrupt_is_refused_when_the_cancel_is_not_accepted() {
        let _guard = setup();
        hook(|h| h.grok_interrupt_pane = None);
        let err = err_of(member("grey", "probe", "%3", "grok").interrupt());
        assert!(err.contains("session/cancel"), "{err}");
    }

    #[test]
    fn test_interrupt_of_an_unsupported_cli_is_refused() {
        let _guard = setup();
        let err = err_of(member("odd", "probe", "%4", "cursor").interrupt());
        assert!(err.contains("no native interrupt"), "{err}");
        assert!(calls().is_empty());
    }
}
