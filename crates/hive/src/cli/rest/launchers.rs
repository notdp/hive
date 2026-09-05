use std::os::unix::process::CommandExt;

use serde_json::{json, Map, Value};

use super::*;
use crate::tmux;

// ---------------------------------------------------------------------------
// codex managed launch
// ---------------------------------------------------------------------------

// codex subcommands that are not an interactive TUI launch: hive leaves these
// completely untouched (raw codex). Kept in sync with `codex --help`.
const _CODEX_PASSTHROUGH_SUBCOMMANDS: &[&str] = &[
    "exec",
    "e",
    "review",
    "login",
    "logout",
    "mcp",
    "plugin",
    "mcp-server",
    "app-server",
    "remote-control",
    "app",
    "completion",
    "update",
    "doctor",
    "sandbox",
    "debug",
    "apply",
    "a",
    "cloud",
    "exec-server",
    "features",
    "help",
];

// Non-interactive surfaces: --help/--version never start a session.
const _CODEX_PASSTHROUGH_FLAGS: &[&str] = &["-h", "--help", "-V", "--version"];

// Global codex options that consume the following token as their value, so the
// subcommand scan does not mistake that value for the subcommand. `--opt=value`
// and `-Cvalue` are self-contained and handled separately.
const _CODEX_VALUE_OPTS: &[&str] = &[
    "-c",
    "--config",
    "-m",
    "--model",
    "-C",
    "--cd",
    "--remote",
    "--remote-auth-token-env",
    "--enable",
    "--disable",
    "-p",
    "--profile",
    "-a",
    "--ask-for-approval",
    "-s",
    "--sandbox",
];

/// Index of the first non-option token in `args` — the subcommand, if any.
pub(crate) fn _codex_subcommand_index(args: &[String]) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            return if i + 1 < args.len() {
                Some(i + 1)
            } else {
                None
            };
        }
        if a.starts_with('-') {
            i += if _CODEX_VALUE_OPTS.contains(&a.as_str()) && !a.contains('=') {
                2
            } else {
                1
            };
            continue;
        }
        return Some(i);
    }
    None
}

/// First positional token after the subcommand (e.g. resume's SESSION_ID).
pub(crate) fn _codex_positional_after(args: &[String], sub_index: usize) -> Option<String> {
    let mut i = sub_index + 1;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            return args.get(i + 1).cloned();
        }
        if a.starts_with('-') {
            i += if _CODEX_VALUE_OPTS.contains(&a.as_str()) && !a.contains('=') {
                2
            } else {
                1
            };
            continue;
        }
        return Some(a.clone());
    }
    None
}

/// Value of the first `--opt value` / `--opt=value` occurrence in `args`.
///
/// A following token starting with `-` is the next flag, not this option's
/// value: the option is read as bare (None) rather than swallowing it.
pub(crate) fn _codex_opt_value(args: &[String], names: &[&str]) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        if names.contains(&a.as_str()) {
            let next = args.get(i + 1).map(String::as_str).unwrap_or("");
            return if !next.is_empty() && !next.starts_with('-') {
                Some(next.to_string())
            } else {
                None
            };
        }
        for name in names {
            let prefix = if name.starts_with("--") {
                format!("{name}=")
            } else {
                (*name).to_string()
            };
            if a.starts_with(&prefix) && a != name {
                return Some(a[prefix.len()..].to_string());
            }
        }
    }
    None
}

/// `<team>.<member>` when the pane carries hive member tags, else None.
pub(crate) fn _pane_member_label_via(
    get: impl Fn(&str, &str) -> Option<String>,
    pane: &str,
) -> Option<String> {
    let team = get(pane, "hive-team").unwrap_or_default();
    let agent = get(pane, "hive-agent").unwrap_or_default();
    if !team.is_empty() && !agent.is_empty() {
        Some(format!("{team}.{agent}"))
    } else {
        None
    }
}

fn _pane_member_label(pane: &str) -> Option<String> {
    _pane_member_label_via(tmux::get_pane_option, pane)
}

/// Launcher-minted job/thread name: member identity, or a pane placeholder.
pub(crate) fn _mint_name(label: Option<String>, pane: &str) -> String {
    label.unwrap_or_else(|| {
        let stripped = pane.replace('%', "");
        format!(
            "hive-{}",
            if stripped.is_empty() {
                "pane"
            } else {
                stripped.as_str()
            }
        )
    })
}

pub(crate) fn _codex_pane_thread_name(pane: &str) -> String {
    _mint_name(_pane_member_label(pane), pane)
}

/// True when the user already passed codex's cwd flag (-C / --cd, any form).
fn _codex_args_set_cwd(args: &[String]) -> bool {
    args.iter()
        .any(|a| a == "--cd" || a.starts_with("--cd=") || a.starts_with("-C"))
}

fn _codex_raw(args: &[String]) -> ! {
    execvp("codex", args)
}

/// Replace this process with codex on the shared app-server daemon.
///
/// Degrades to raw `codex` (embedded, status quo) whenever the managed path
/// cannot apply — the caller never ends up worse than plain codex.
fn _exec_codex_managed(args: &[String]) -> ! {
    use crate::adapters::codex_app_server;

    let pane = {
        let env_pane = env_string("TMUX_PANE");
        if !env_pane.is_empty() {
            env_pane
        } else {
            tmux::get_current_pane_id().unwrap_or_default()
        }
    };
    if pane.is_empty() || !tmux::is_inside_tmux() {
        _codex_raw(args); // hive needs a tmux pane to bind a thread to
    }
    let sub_index = _codex_subcommand_index(args);
    let sub = sub_index.map(|i| args[i].as_str());
    if let Some(sub) = sub {
        if _CODEX_PASSTHROUGH_SUBCOMMANDS.contains(&sub) {
            _codex_raw(args); // a management subcommand, not an interactive TUI launch
        }
    }
    if args
        .iter()
        .any(|a| _CODEX_PASSTHROUGH_FLAGS.contains(&a.as_str()))
    {
        _codex_raw(args); // --help/--version never start a session
    }
    if args
        .iter()
        .any(|a| a == "--remote" || a.starts_with("--remote="))
    {
        _codex_raw(args); // caller already chose an endpoint
    }
    if !codex_app_server::spawn_daemon() {
        _codex_raw(args); // daemon would not bind — fall back to embedded codex
    }
    let cwd = _codex_opt_value(args, &["--cd", "-C"])
        .filter(|value| !value.is_empty())
        .unwrap_or_else(getcwd);
    let _ = codex_app_server::ensure_dir_trusted(&cwd);
    let sock = codex_app_server::shared_socket_path();
    // -c check_for_update_on_startup=false mirrors the hive-spawned path so a
    // managed launch never drops the user into codex's npm self-update prompt.
    let mut argv: Vec<String> = vec![
        "-c".to_string(),
        "check_for_update_on_startup=false".to_string(),
        "--remote".to_string(),
        format!("unix://{}", sock.to_string_lossy()),
    ];
    if !_codex_args_set_cwd(args) {
        argv.push("--cd".to_string());
        argv.push(cwd.clone());
    }

    if sub == Some("resume") {
        let sid = _codex_positional_after(args, sub_index.expect("sub implies index"));
        match sid {
            Some(sid) => {
                let _ = codex_app_server::write_pane_thread(&pane, &sid, &cwd);
            }
            None => {
                // Picker / --last: the chosen thread is unknowable up front. A
                // stale record must not keep routing hive at the previous thread.
                let _ = codex_app_server::clear_pane_thread(&pane);
            }
        }
        argv.extend(args.iter().cloned());
        execvp("codex", &argv);
    }
    if sub == Some("fork") {
        let sub_index = sub_index.expect("sub implies index");
        let source = _codex_positional_after(args, sub_index);
        let forked = source.as_deref().and_then(|source| {
            codex_app_server::fork_member_thread(source, &_codex_pane_thread_name(&pane))
        });
        if let (Some(source), Some(forked)) = (source, forked) {
            let _ = codex_app_server::write_pane_thread(&pane, &forked, &cwd);
            let mut rewritten: Vec<String> = args.to_vec();
            rewritten[sub_index] = "resume".to_string();
            if let Some(offset) = rewritten
                .iter()
                .skip(sub_index + 1)
                .position(|a| *a == source)
            {
                rewritten[sub_index + 1 + offset] = forked;
            }
            argv.extend(rewritten);
            execvp("codex", &argv);
        }
        // No source id, or the fork RPC failed: let codex fork on its own —
        // remote-attached but unrecorded, so clear any stale pane record.
        let _ = codex_app_server::clear_pane_thread(&pane);
        argv.extend(args.iter().cloned());
        execvp("codex", &argv);
    }
    // Interactive launch — no subcommand, flags only, or a bare [PROMPT]:
    // mint the pane's thread so it is born with an identity hive can read,
    // deliver to, and resume. A trailing prompt rides `resume`'s own [PROMPT]
    // positional unchanged.
    let minted = codex_app_server::start_member_thread(
        &cwd,
        &_codex_pane_thread_name(&pane),
        &_codex_opt_value(args, &["--model", "-m"]).unwrap_or_default(),
    );
    if let Some(minted) = minted {
        let _ = codex_app_server::write_pane_thread(&pane, &minted, &cwd);
        argv.push("resume".to_string());
        argv.push(minted);
        argv.extend(args.iter().cloned());
        execvp("codex", &argv);
    }
    // Mint failed (daemon just died?): remote attach unrecorded — degraded,
    // and a stale record must not point hive at a thread this TUI won't run.
    let _ = codex_app_server::clear_pane_thread(&pane);
    argv.extend(args.iter().cloned());
    execvp("codex", &argv);
}

pub fn codex_cmd(args: &[String]) {
    crate::plugin_manager::ensure_codex_plugin_current();
    _exec_codex_managed(args);
}

// ---------------------------------------------------------------------------
// claude managed launch
// ---------------------------------------------------------------------------

// claude subcommands that are not an interactive TUI launch: raw passthrough.
// Hidden subcommands are only recognized at argv[1], so args[0] is the one
// place a subcommand can sit.
const _CLAUDE_PASSTHROUGH_SUBCOMMANDS: &[&str] = &[
    "agents",
    "attach",
    "logs",
    "stop",
    "respawn",
    "rm",
    "mcp",
    "plugin",
    "config",
    "doctor",
    "update",
    "install",
    "migrate-installer",
    "setup-token",
    "api",
    "bg-spare",
    "bg-pty-host",
    "daemon",
    "help",
];

// Non-interactive surfaces: --help/--version never start a session.
const _CLAUDE_PASSTHROUGH_FLAGS: &[&str] = &["-h", "--help", "-v", "--version"];

// Launch shapes the bg mapping cannot represent: headless print mode
// (rejected by --bg upstream), an explicit --bg the caller manages itself,
// and -c/--continue (which session it continues is unknowable up front).
const _CLAUDE_RAW_MODE_FLAGS: &[&str] = &["-p", "--print", "--bg", "-c", "--continue"];

/// (resume flag present, its value). `-r`/`--resume` take an optional value;
/// a bare flag opens claude's picker.
pub(crate) fn _claude_resume_arg(args: &[String]) -> (bool, Option<String>) {
    for (i, a) in args.iter().enumerate() {
        if a == "-r" || a == "--resume" {
            if let Some(next) = args.get(i + 1) {
                if !next.starts_with('-') {
                    return (true, Some(next.clone()));
                }
            }
            return (true, None);
        }
        if let Some(rest) = a.strip_prefix("--resume=") {
            return (
                true,
                if rest.is_empty() {
                    None
                } else {
                    Some(rest.to_string())
                },
            );
        }
    }
    (false, None)
}

pub(crate) fn _claude_pane_job_name(pane: &str) -> String {
    _mint_name(_pane_member_label(pane), pane)
}

/// Replace this process with a watch loop keeping the pane attached to its
/// bg job's engine. Never returns.
fn _claude_attach_loop(job_id: &str) -> ! {
    let quoted = shlex_quote(job_id);
    let script = format!(
        "set -m\n\
         while :; do\n  \
         t0=$(date +%s)\n  \
         claude attach {quoted}\n  \
         rc=$?\n  \
         if [ $rc -ge 1 ] && [ $rc -le 128 ] && [ $(( $(date +%s) - t0 )) -lt 5 ]; then\n    \
         exit $rc\n  \
         fi\n  \
         echo \"hive: viewer detached from job {quoted}; \"\\\n\
         \"reattaching in 1s (Ctrl-C to stay detached)\" >&2\n  \
         sleep 1 || exit 0\n\
         done\n"
    );
    let env = crate::adapters::claude_bg::bg_env(None);
    let err = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .env_clear()
        .envs(&env)
        .exec();
    eprintln!("Error: {err}");
    std::process::exit(1);
}

fn _claude_raw(args: &[String]) -> ! {
    execvp("claude", args)
}

/// Run claude as a hive-managed background job with this pane attached.
///
/// Degrades to raw `claude` whenever the managed path cannot apply — the
/// caller never ends up worse than plain claude.
fn _exec_claude_managed(args: &[String]) -> ! {
    use crate::adapters::claude_bg;

    if args.len() == 1 && args[0] == "channel-server" {
        // Tombstone for the retired hive-channel plugin's MCP entry: exec'ing
        // this into claude would feed it a garbage subcommand every session.
        eprintln!(
            "Error: the hive-channel plugin is retired (claude delivery now \
             uses the session's own cross-session inbox). Remove it with: \
             claude plugin uninstall hive-channel@hive"
        );
        std::process::exit(1);
    }
    let pane = env_string("TMUX_PANE");
    if pane.is_empty() || env_string("TMUX").is_empty() {
        _claude_raw(args); // hive needs a real tmux pane to bind a job to
    }
    if let Some(first) = args.first() {
        if _CLAUDE_PASSTHROUGH_SUBCOMMANDS.contains(&first.as_str()) {
            _claude_raw(args); // a management subcommand, not an interactive TUI launch
        }
    }
    if args
        .iter()
        .any(|a| _CLAUDE_PASSTHROUGH_FLAGS.contains(&a.as_str()))
    {
        _claude_raw(args);
    }
    if args
        .iter()
        .any(|a| _CLAUDE_RAW_MODE_FLAGS.contains(&a.as_str()))
    {
        _claude_raw(args);
    }

    let (resume_present, resume_val) = _claude_resume_arg(args);
    if resume_present && resume_val.is_none() {
        _claude_raw(args); // picker: the chosen session is unknowable up front
    }
    let cwd = getcwd();

    if let Some(resume_val) = resume_val
        .as_deref()
        .filter(|value| claude_bg::looks_like_job_id(value))
    {
        let mut engine = claude_bg::engine_session_for_job(resume_val);
        if engine.is_none() && claude_bg::job_exists(resume_val, "claude") {
            engine = claude_bg::ensure_engine(resume_val, None, "claude");
        }
        if engine.is_some() || claude_bg::job_exists(resume_val, "claude") {
            let session_id = engine.map(|e| e.session_id).unwrap_or_default();
            let _ = claude_bg::write_pane_job(&pane, resume_val, &session_id, &cwd);
            _claude_attach_loop(resume_val);
        }
        // Not a known job: fall through and treat the value as a session id.
    }

    let user_named = args
        .iter()
        .any(|a| a == "--name" || a.starts_with("--name="));
    let name = if user_named {
        String::new()
    } else {
        _claude_pane_job_name(&pane)
    };
    let job_id = claude_bg::spawn_job(&cwd, &name, "", args, None, "claude");
    let job_id = match job_id {
        Some(job_id) if !job_id.is_empty() => job_id,
        _ => {
            eprintln!("hive: `claude --bg` failed; launching plain claude");
            _claude_raw(args);
        }
    };
    let engine = claude_bg::wait_engine_entry(&job_id, 10.0);
    let _ = claude_bg::write_pane_job(
        &pane,
        &job_id,
        &engine.map(|e| e.session_id).unwrap_or_default(),
        &cwd,
    );
    _claude_attach_loop(&job_id);
}

pub fn claude_cmd(args: &[String]) {
    _exec_claude_managed(args);
}

// ---------------------------------------------------------------------------
// grok managed launch
// ---------------------------------------------------------------------------

// grok subcommands that are not an interactive TUI launch: hive leaves these
// completely untouched (raw grok). A subcommand is always the first token; a
// prompt is the only other thing that can sit there.
const _GROK_PASSTHROUGH_SUBCOMMANDS: &[&str] = &[
    "agent",
    "completions",
    "dashboard",
    "doctor",
    "du",
    "export",
    "help",
    "inspect",
    "leader",
    "login",
    "logout",
    "mcp",
    "memory",
    "models",
    "plugin",
    "sessions",
    "setup",
    "trace",
    "update",
    "version",
    "worktree",
    "wrap",
];

// Non-interactive surfaces: --help/--version never start a session.
const _GROK_PASSTHROUGH_FLAGS: &[&str] = &["-h", "--help", "-V", "--version"];

/// Value of the first `--opt value` / `--opt=value` occurrence in `args`.
///
/// A following token starting with `-` is the next flag, not this option's
/// value: `--resume -m grok-4` resumes grok's own picker instead of recording
/// `-m` as the pane's session id.
pub(crate) fn _grok_opt_value(args: &[String], names: &[&str]) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        if names.contains(&a.as_str()) {
            let next = args.get(i + 1).map(String::as_str).unwrap_or("");
            return if !next.is_empty() && !next.starts_with('-') {
                Some(next.to_string())
            } else {
                None
            };
        }
        for name in names {
            if let Some(rest) = a.strip_prefix(&format!("{name}=")) {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// (session id this launch will run, whether hive must pass --session-id).
pub(crate) fn _grok_launch_session(args: &[String]) -> (Option<String>, bool) {
    let explicit = _grok_opt_value(args, &["--session-id", "-s"]);
    if explicit.as_deref().is_some_and(|value| !value.is_empty()) {
        return (explicit, false);
    }
    if args
        .iter()
        .any(|a| a == "--resume" || a.starts_with("--resume="))
        && !args.iter().any(|a| a == "--fork-session")
    {
        return (_grok_opt_value(args, &["--resume"]), false);
    }
    (Some(uuid4()), true)
}

fn _grok_raw(args: &[String]) -> ! {
    execvp("grok", args)
}

/// Replace this process with grok, attached to the pane's leader daemon.
///
/// A pane tagged as a team member resolves to the member's identity-keyed
/// engine, which spawn minted before this pane existed: `spawn_daemon`
/// finds it listening and the TUI attaches (`--resume <sid>`). An untagged
/// pane — a raw `hive grok` outside any team — is the one place a leader is
/// born from a pane, keyed `p<slug>` with the pane's lifecycle.
///
/// Degrades to raw `grok` whenever the managed path cannot apply — the
/// caller never ends up worse than plain grok.
fn _exec_grok_managed(args: &[String]) -> ! {
    use crate::adapters::grok_leader;

    let pane = {
        let env_pane = env_string("TMUX_PANE");
        if !env_pane.is_empty() {
            env_pane
        } else {
            tmux::get_current_pane_id().unwrap_or_default()
        }
    };
    if pane.is_empty() || !tmux::is_inside_tmux() {
        _grok_raw(args); // hive needs a tmux pane to bind a daemon to
    }
    if let Some(first) = args.first() {
        if _GROK_PASSTHROUGH_SUBCOMMANDS.contains(&first.as_str()) {
            _grok_raw(args); // a management subcommand, not an interactive TUI launch
        }
    }
    if args
        .iter()
        .any(|a| _GROK_PASSTHROUGH_FLAGS.contains(&a.as_str()))
    {
        _grok_raw(args); // --help/--version never start a session
    }
    if !grok_leader::spawn_daemon(&pane) {
        // A raw grok drives whatever session it likes; leaving an earlier
        // record in place would have hive resolve that stale id as this pane's.
        let _ = std::fs::remove_file(grok_leader::pane_session_path(&pane));
        eprintln!("hive: grok leader did not start; launching plain grok");
        _grok_raw(args);
    }
    let (session_id, pass_flag) = _grok_launch_session(args);
    let mut argv: Vec<String> = vec![
        "--leader".to_string(),
        "--leader-socket".to_string(),
        grok_leader::pane_socket_path(&pane)
            .to_string_lossy()
            .into_owned(),
    ];
    if pass_flag {
        argv.push("--session-id".to_string());
        argv.push(session_id.clone().unwrap_or_default());
    }
    if let Some(session_id) = session_id.as_deref().filter(|value| !value.is_empty()) {
        let _ = grok_leader::write_pane_session(&pane, session_id, &getcwd());
    }
    argv.extend(args.iter().cloned());
    execvp("grok", &argv);
}

pub fn grok_cmd(args: &[String]) {
    _exec_grok_managed(args);
}

// ---------------------------------------------------------------------------
// ccd
// ---------------------------------------------------------------------------

pub fn ccd_ls_cmd() {
    let members = _live_member_pids();
    let mut rows: Vec<Value> = Vec::new();
    for s in crate::adapters::claude_sessions::list_sessions() {
        let mut row = Map::new();
        row.insert("name".to_string(), Value::String(s.name.clone()));
        row.insert("title".to_string(), Value::String(s.title.clone()));
        row.insert("pid".to_string(), Value::Number(s.pid.into()));
        row.insert("kind".to_string(), Value::String(s.kind.clone()));
        row.insert("cwd".to_string(), Value::String(s.cwd.clone()));
        if let Some((team, agent)) = members.get(&s.pid) {
            row.insert(
                "member".to_string(),
                Value::String(format!("{team}.{agent}")),
            );
        }
        rows.push(Value::Object(row));
    }
    println!("{}", json_pretty(&json!({ "sessions": rows })));
}

// ---------------------------------------------------------------------------
// resume-hint
// ---------------------------------------------------------------------------

pub fn resume_hint_cmd(cli_name: &str) {
    // Prints nothing and exits 0 on any failure: a hint must never break the
    // wrapper.
    if let Some(hint) = _resume_hint(cli_name, &getcwd()) {
        println!("{hint}");
    }
}

pub(super) fn _resume_hint(cli_name: &str, cwd: &str) -> Option<String> {
    let (pane, _team, _agent) = _pane_team_identity()?;
    let (session_id, resume_cmd) = match cli_name {
        "codex" => (
            crate::adapters::codex_app_server::session_id_for_pane(&pane),
            "hive codex resume",
        ),
        "grok" => (
            crate::adapters::grok_leader::read_pane_session(&pane).map(|record| record.session_id),
            "hive grok --resume",
        ),
        _ => (
            crate::adapters::claude_bg::job_id_for_pane(&pane),
            "hive claude --resume",
        ),
    };
    let session_id = session_id.filter(|value| !value.is_empty())?;
    // Both fields are untrusted content headed for automatic terminal output:
    // control/non-printable bytes (ESC/OSC/BEL/newline) silence the hint. So
    // does a leading "-", which would parse as a CLI option instead of a
    // session id when pasted.
    if !py_isprintable(cwd) || !py_isprintable(&session_id) || session_id.starts_with('-') {
        return None;
    }
    let command = format!(
        "cd {} && {resume_cmd} {}",
        shlex_quote(cwd),
        shlex_quote(&session_id)
    );
    // cyan matches the CLI's own resume line; stripped whenever stdout is not
    // a real terminal (pipes, tests, logs) — click's behavior.
    let styled = if stdout_isatty() {
        format!("\x1b[36m{command}\x1b[0m")
    } else {
        command
    };
    Some(format!("Resume from anywhere:\n  {styled}"))
}

/// (pane, team, agent) when this pane is a tagged team member, else None.
fn _pane_team_identity() -> Option<(String, String, String)> {
    let pane = env_string("TMUX_PANE").trim().to_string();
    if pane.is_empty() {
        return None;
    }
    let team = tmux::get_pane_option(&pane, "hive-team").unwrap_or_default();
    let agent = tmux::get_pane_option(&pane, "hive-agent").unwrap_or_default();
    if team.is_empty() || agent.is_empty() {
        return None;
    }
    Some((pane, team, agent))
}
