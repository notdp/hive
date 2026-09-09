//! The managed launchers — `hive claude` / `hive codex` / `hive grok` —
//! that replace this process with the engine bound to the pane (a bg job,
//! the shared app-server daemon, the pane leader). Outside tmux, Claude
//! starts a background job with a local viewer; create/join later hands it
//! to the team window. Plus `ccd ls` and the wrappers' `resume-hint`.

use std::os::unix::process::CommandExt;

use serde_json::{json, Map, Value};

use super::util::{execvp, is_printable, json_pretty, stdin_isatty, stdout_isatty};
use crate::agent::uuid4;
use crate::identity;
use crate::identity::env_string;
use crate::paths::getcwd;
use crate::shell::shlex_quote;
use crate::team::live_member_pids;
use crate::tmux;

// ---------------------------------------------------------------------------
// codex managed launch
// ---------------------------------------------------------------------------

// codex subcommands that are not an interactive TUI launch: hive leaves these
// completely untouched (raw codex). Kept in sync with `codex --help`.
const CODEX_PASSTHROUGH_SUBCOMMANDS: &[&str] = &[
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
const CODEX_PASSTHROUGH_FLAGS: &[&str] = &["-h", "--help", "-V", "--version"];

// Global codex options that consume the following token as their value, so the
// subcommand scan does not mistake that value for the subcommand. `--opt=value`
// and `-Cvalue` are self-contained and handled separately.
const CODEX_VALUE_OPTS: &[&str] = &[
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
fn codex_subcommand_index(args: &[String]) -> Option<usize> {
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
            i += if CODEX_VALUE_OPTS.contains(&a.as_str()) && !a.contains('=') {
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
fn codex_positional_after(args: &[String], sub_index: usize) -> Option<String> {
    let mut i = sub_index + 1;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            return args.get(i + 1).cloned();
        }
        if a.starts_with('-') {
            i += if CODEX_VALUE_OPTS.contains(&a.as_str()) && !a.contains('=') {
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
fn codex_opt_value(args: &[String], names: &[&str]) -> Option<String> {
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
fn pane_member_label_via(get: impl Fn(&str, &str) -> Option<String>, pane: &str) -> Option<String> {
    let team = get(pane, "hive-team").unwrap_or_default();
    let agent = get(pane, "hive-agent").unwrap_or_default();
    if !team.is_empty() && !agent.is_empty() {
        Some(format!("{team}.{agent}"))
    } else {
        None
    }
}

fn pane_member_label(pane: &str) -> Option<String> {
    pane_member_label_via(tmux::get_pane_option, pane)
}

/// Launcher-minted job/thread name: member identity, or a pane placeholder.
fn mint_name(label: Option<String>, pane: &str) -> String {
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

fn codex_pane_thread_name(pane: &str) -> String {
    mint_name(pane_member_label(pane), pane)
}

/// True when the user already passed codex's cwd flag (-C / --cd, any form).
fn codex_args_set_cwd(args: &[String]) -> bool {
    args.iter()
        .any(|a| a == "--cd" || a.starts_with("--cd=") || a.starts_with("-C"))
}

fn codex_raw(args: &[String]) -> ! {
    execvp("codex", args)
}

/// Launch shapes hive leaves untouched wherever they run: a management
/// subcommand (not an interactive TUI launch), --help/--version, and a
/// caller who already chose a `--remote` endpoint.
fn codex_raw_shape(args: &[String]) -> bool {
    let sub = codex_subcommand_index(args).map(|i| args[i].as_str());
    sub.is_some_and(|s| CODEX_PASSTHROUGH_SUBCOMMANDS.contains(&s))
        || args.iter().any(|a| {
            CODEX_PASSTHROUGH_FLAGS.contains(&a.as_str())
                || a == "--remote"
                || a.starts_with("--remote=")
        })
}

/// Replace this process with codex on the shared app-server daemon.
///
/// Degrades to raw `codex` (embedded, status quo) whenever the managed path
/// cannot apply — the caller never ends up worse than plain codex.
fn exec_codex_managed(args: &[String]) -> ! {
    use crate::adapters::codex_app_server;

    if codex_raw_shape(args) {
        codex_raw(args);
    }
    let pane = {
        let env_pane = env_string("TMUX_PANE");
        if !env_pane.is_empty() {
            env_pane
        } else {
            identity::current_pane_id().unwrap_or_default()
        }
    };
    if pane.is_empty() || !identity::is_inside_tmux() {
        exec_codex_outside(args);
    }
    let sub_index = codex_subcommand_index(args);
    let sub = sub_index.map(|i| args[i].as_str());
    if !codex_app_server::spawn_daemon() {
        codex_raw(args); // daemon would not bind — fall back to embedded codex
    }
    let cwd = codex_opt_value(args, &["--cd", "-C"])
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
    if !codex_args_set_cwd(args) {
        argv.push("--cd".to_string());
        argv.push(cwd.clone());
    }

    if sub == Some("resume") {
        let sid = codex_positional_after(args, sub_index.expect("sub implies index"));
        match sid {
            Some(sid) => {
                let _ = codex_app_server::write_pane_thread(
                    &pane,
                    &sid,
                    &cwd,
                    tmux::own_socket_path().as_deref(),
                );
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
        let source = codex_positional_after(args, sub_index);
        let forked = source.as_deref().and_then(|source| {
            codex_app_server::fork_member_thread(source, &codex_pane_thread_name(&pane))
        });
        if let (Some(source), Some(forked)) = (source, forked) {
            let _ = codex_app_server::write_pane_thread(
                &pane,
                &forked,
                &cwd,
                tmux::own_socket_path().as_deref(),
            );
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
        &codex_pane_thread_name(&pane),
        &codex_opt_value(args, &["--model", "-m"]).unwrap_or_default(),
    );
    if let Some(minted) = minted {
        let _ = codex_app_server::write_pane_thread(
            &pane,
            &minted,
            &cwd,
            tmux::own_socket_path().as_deref(),
        );
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

/// Only explicit native IDs can be bound before the resume picker runs.
fn codex_thread_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn normalize_codex_cwd(args: &mut [String], cwd: &str) {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            break;
        }
        if args[i] == "--cd" || args[i] == "-C" {
            if let Some(value) = args.get_mut(i + 1) {
                *value = cwd.into();
            }
            i += 2;
        } else if CODEX_VALUE_OPTS.contains(&args[i].as_str()) {
            i += 2;
        } else {
            if args[i].starts_with("--cd=") {
                args[i] = format!("--cd={cwd}");
            } else if args[i].starts_with("-C") {
                args[i] = format!("-C{cwd}");
            }
            i += 1;
        }
    }
}

fn codex_launch_cwd(args: &[String], source: Option<&str>) -> String {
    use crate::adapters::base::SessionAdapter;
    codex_opt_value(args, &["--cd", "-C"])
        .or_else(|| {
            let adapter = crate::adapters::codex::CodexAdapter;
            let path = adapter.find_session_file(source?, None)?;
            adapter.read_meta(&path).and_then(|meta| meta.cwd)
        })
        .filter(|cwd| !cwd.is_empty())
        .unwrap_or_else(getcwd)
}

fn exec_codex_outside(args: &[String]) -> ! {
    use crate::adapters::codex_app_server;
    if !env_string("TMUX").is_empty() || !stdin_isatty() || !stdout_isatty() {
        codex_raw(args);
    }
    let sub_index = codex_subcommand_index(args);
    let sub = sub_index.map(|i| args[i].as_str());
    let source = if matches!(sub, Some("resume" | "fork")) {
        let source = codex_positional_after(args, sub_index.unwrap());
        match source.filter(|s| codex_thread_id(s)) {
            Some(source) => Some(source),
            None => codex_raw(args),
        }
    } else {
        None
    };
    if sub == Some("resume") {
        if let Some((team, _)) =
            crate::registry::member_for_session(source.as_deref().unwrap(), Some("codex"))
        {
            super::attach::attach_cmd(&team);
            std::process::exit(0);
        }
    }
    let cwd = codex_launch_cwd(args, source.as_deref());
    let cwd = std::path::Path::new(&cwd)
        .canonicalize()
        .unwrap_or_else(|error| {
            eprintln!("hive: invalid Codex working directory: {error}");
            std::process::exit(1);
        })
        .to_string_lossy()
        .into_owned();
    if !codex_app_server::spawn_daemon() {
        codex_raw(args);
    }
    let _ = codex_app_server::ensure_dir_trusted(&cwd);
    let name = format!("hive-{}", &uuid4()[..8]);
    let thread = match sub {
        Some("resume") => source.clone(),
        Some("fork") => codex_app_server::fork_member_thread(source.as_deref().unwrap(), &name),
        _ => codex_app_server::start_member_thread(
            &cwd,
            &name,
            &codex_opt_value(args, &["--model", "-m"]).unwrap_or_default(),
        ),
    }
    .unwrap_or_else(|| {
        eprintln!("hive: could not create Codex thread");
        std::process::exit(1);
    });
    let session = crate::terminal_handoff::Session {
        cli: "codex",
        id: thread.clone(),
        cwd: cwd.clone(),
        data: Value::Null,
    };
    let mut launch_args = args.to_vec();
    normalize_codex_cwd(&mut launch_args, &cwd);
    let mut argv = vec![
        "-c".into(),
        "check_for_update_on_startup=false".into(),
        "--remote".into(),
        format!(
            "unix://{}",
            codex_app_server::shared_socket_path().display()
        ),
    ];
    if !codex_args_set_cwd(args) {
        argv.extend(["--cd".into(), cwd]);
    }
    if let Some(source) = source {
        let mut rewritten = launch_args;
        let index = sub_index.unwrap();
        rewritten[index] = "resume".into();
        let offset = rewritten[index + 1..]
            .iter()
            .position(|s| s == &source)
            .unwrap();
        rewritten[index + 1 + offset] = thread;
        argv.extend(rewritten);
    } else {
        argv.extend(["resume".into(), thread]);
        argv.extend(launch_args);
    }
    match crate::terminal_handoff::run(&session, &argv) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!(
                "hive: {error}; resume with `hive codex resume {}`",
                session.id
            );
            std::process::exit(1);
        }
    }
}

pub(crate) fn codex_cmd(args: &[String]) {
    crate::plugin_manager::ensure_codex_plugin_current();
    exec_codex_managed(args);
}

// ---------------------------------------------------------------------------
// claude managed launch
// ---------------------------------------------------------------------------

// claude subcommands that are not an interactive TUI launch: raw passthrough.
// Hidden subcommands are only recognized at argv[1], so args[0] is the one
// place a subcommand can sit.
const CLAUDE_PASSTHROUGH_SUBCOMMANDS: &[&str] = &[
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
const CLAUDE_PASSTHROUGH_FLAGS: &[&str] = &["-h", "--help", "-v", "--version"];

// Launch shapes the bg mapping cannot represent: headless print mode
// (rejected by --bg upstream), an explicit --bg the caller manages itself,
// and -c/--continue (which session it continues is unknowable up front).
const CLAUDE_RAW_MODE_FLAGS: &[&str] = &["-p", "--print", "--bg", "-c", "--continue"];

/// (resume flag present, its value). `-r`/`--resume` take an optional value;
/// a bare flag opens claude's picker.
fn claude_resume_arg(args: &[String]) -> (bool, Option<String>) {
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

fn claude_pane_job_name(pane: &str) -> String {
    mint_name(pane_member_label(pane), pane)
}

/// Replace this process with a watch loop keeping the pane attached to its
/// bg job's engine. Never returns.
fn claude_attach_loop(job_id: &str) -> ! {
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

fn claude_raw(args: &[String]) -> ! {
    execvp("claude", args)
}

/// Launch shapes hive leaves untouched wherever they run: a management
/// subcommand (not an interactive TUI launch), --help/--version, the raw
/// modes the bg mapping cannot represent, and a bare -r/--resume (the
/// picker's choice is unknowable up front).
fn claude_raw_shape(args: &[String]) -> bool {
    args.first()
        .is_some_and(|f| CLAUDE_PASSTHROUGH_SUBCOMMANDS.contains(&f.as_str()))
        || args.iter().any(|a| {
            CLAUDE_PASSTHROUGH_FLAGS.contains(&a.as_str())
                || CLAUDE_RAW_MODE_FLAGS.contains(&a.as_str())
        })
        || matches!(claude_resume_arg(args), (true, None))
}

/// Run claude as a hive-managed background job with this pane attached.
///
/// Degrades to raw `claude` whenever the managed path cannot apply — the
/// caller never ends up worse than plain claude.
fn exec_claude_managed(args: &[String]) -> ! {
    use crate::adapters::claude_bg;

    if claude_raw_shape(args) {
        claude_raw(args);
    }
    let pane = env_string("TMUX_PANE");
    let outside = pane.is_empty() || env_string("TMUX").is_empty();
    if outside && (!env_string("TMUX").is_empty() || !stdin_isatty() || !stdout_isatty()) {
        claude_raw(args);
    }
    let (_, resume_val) = claude_resume_arg(args);
    if outside {
        if let Some(job) = resume_val.as_deref() {
            if let Some((team, _)) = crate::registry::member_for_session(job, Some("claude")) {
                super::attach::attach_cmd(&team);
                std::process::exit(0);
            }
        }
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
            if outside {
                run_outside_claude(resume_val);
            }
            let session_id = engine.map(|e| e.session_id).unwrap_or_default();
            let _ = claude_bg::write_pane_job(&pane, resume_val, &session_id, &cwd);
            claude_attach_loop(resume_val);
        }
        // Not a known job: fall through and treat the value as a session id.
    }

    let user_named = args
        .iter()
        .any(|a| a == "--name" || a.starts_with("--name="));
    let name = if user_named {
        String::new()
    } else if outside {
        format!("hive-{}", &uuid4()[..8])
    } else {
        claude_pane_job_name(&pane)
    };
    let job_id = claude_bg::spawn_job(&cwd, &name, "", args, None, "claude");
    let job_id = match job_id {
        Some(job_id) if !job_id.is_empty() => job_id,
        _ => {
            eprintln!("hive: `claude --bg` failed; launching plain claude");
            claude_raw(args);
        }
    };
    if outside {
        run_outside_claude(&job_id);
    }
    let engine = claude_bg::wait_engine_entry(&job_id, 10.0);
    let _ = claude_bg::write_pane_job(
        &pane,
        &job_id,
        &engine.map(|e| e.session_id).unwrap_or_default(),
        &cwd,
    );
    claude_attach_loop(&job_id);
}

fn run_outside_claude(job: &str) -> ! {
    let engine = crate::adapters::claude_bg::wait_engine_entry(job, 10.0).unwrap_or_else(|| {
        eprintln!("hive: Claude job has no live engine; resume {job}");
        std::process::exit(1)
    });
    let session = crate::terminal_handoff::Session::claude(engine);
    match crate::terminal_handoff::run(&session, &session.resume_args()) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("hive: {error}; resume with `hive claude --resume {job}`");
            std::process::exit(1);
        }
    }
}

pub(crate) fn claude_cmd(args: &[String]) {
    exec_claude_managed(args);
}

// ---------------------------------------------------------------------------
// grok managed launch
// ---------------------------------------------------------------------------

// grok subcommands that are not an interactive TUI launch: hive leaves these
// completely untouched (raw grok). A subcommand is always the first token; a
// prompt is the only other thing that can sit there.
const GROK_PASSTHROUGH_SUBCOMMANDS: &[&str] = &[
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
const GROK_PASSTHROUGH_FLAGS: &[&str] = &["-h", "--help", "-V", "--version"];

/// Value of the first `--opt value` / `--opt=value` occurrence in `args`.
///
/// A following token starting with `-` is the next flag, not this option's
/// value: `--resume -m grok-4` resumes grok's own picker instead of recording
/// `-m` as the pane's session id.
fn grok_opt_value(args: &[String], names: &[&str]) -> Option<String> {
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

fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// What an `hgrok` outside tmux can hold: an interactive launch whose
/// session id is knowable up front — a fresh session hive names, or one
/// resumed by id. A continue, a resume by title or from the picker, a
/// non-interactive run (`-p`, a prompt file) and an explicit leader
/// socket all belong to plain grok. Returns (session id, whether hive must
/// pass `--session-id`, the launch cwd); None is the raw shape.
fn grok_outside_launch(args: &[String]) -> Option<(String, bool, String)> {
    const RAW: &[&str] = &[
        "-c",
        "--continue",
        "-p",
        "--single",
        "--prompt-file",
        "--prompt-json",
        "--leader-socket",
    ];
    if args.iter().any(|a| {
        RAW.contains(&a.as_str()) || RAW.iter().any(|flag| a.starts_with(&format!("{flag}=")))
    }) {
        return None;
    }
    let cwd = match grok_opt_value(args, &["--cwd"]) {
        Some(dir) => dir,
        None if args.iter().any(|a| a == "--cwd" || a.starts_with("--cwd=")) => return None,
        None => getcwd(),
    };
    let cwd = std::fs::canonicalize(&cwd)
        .ok()?
        .to_string_lossy()
        .into_owned();
    let resume_present = args
        .iter()
        .any(|a| a == "-r" || a == "--resume" || a.starts_with("--resume="));
    if resume_present {
        let value = args
            .iter()
            .find_map(|a| a.strip_prefix("--resume=").map(str::to_string))
            .or_else(|| grok_opt_value(args, &["--resume", "-r"]))
            .filter(|v| !v.is_empty())?;
        return is_uuid(&value).then_some((value, false, cwd));
    }
    if let Some(explicit) = grok_opt_value(args, &["--session-id", "-s"]) {
        return is_uuid(&explicit).then_some((explicit, false, cwd));
    }
    Some((uuid4(), true, cwd))
}

/// (session id this launch will run, whether hive must pass --session-id).
fn grok_launch_session(args: &[String]) -> (Option<String>, bool) {
    let explicit = grok_opt_value(args, &["--session-id", "-s"]);
    if explicit.as_deref().is_some_and(|value| !value.is_empty()) {
        return (explicit, false);
    }
    if args
        .iter()
        .any(|a| a == "--resume" || a.starts_with("--resume="))
        && !args.iter().any(|a| a == "--fork-session")
    {
        return (grok_opt_value(args, &["--resume"]), false);
    }
    (Some(uuid4()), true)
}

fn grok_raw(args: &[String]) -> ! {
    execvp("grok", args)
}

/// Launch shapes hive leaves untouched wherever they run: a management
/// subcommand (not an interactive TUI launch) and --help/--version.
fn grok_raw_shape(args: &[String]) -> bool {
    args.first()
        .is_some_and(|f| GROK_PASSTHROUGH_SUBCOMMANDS.contains(&f.as_str()))
        || args
            .iter()
            .any(|a| GROK_PASSTHROUGH_FLAGS.contains(&a.as_str()))
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
fn exec_grok_managed(args: &[String]) -> ! {
    use crate::adapters::grok_leader;

    if grok_raw_shape(args) {
        grok_raw(args);
    }
    let pane = {
        let env_pane = env_string("TMUX_PANE");
        if !env_pane.is_empty() {
            env_pane
        } else {
            identity::current_pane_id().unwrap_or_default()
        }
    };
    let outside = pane.is_empty() || !identity::is_inside_tmux();
    if outside && (!env_string("TMUX").is_empty() || !stdin_isatty() || !stdout_isatty()) {
        grok_raw(args); // no terminal to hold a viewer, or a nested client
    }
    if outside {
        run_outside_grok(args);
    }
    if !grok_leader::spawn_daemon(&pane) {
        // A raw grok drives whatever session it likes; leaving an earlier
        // record in place would have hive resolve that stale id as this pane's.
        let _ = std::fs::remove_file(grok_leader::pane_session_path(&pane));
        eprintln!("hive: grok leader did not start; launching plain grok");
        grok_raw(args);
    }
    let (session_id, pass_flag) = grok_launch_session(args);
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

/// `hgrok` at a terminal outside tmux: a leader on a launch key serving
/// the session hive minted, the TUI held by the terminal handoff, and the
/// leader's own stop once the terminal is done with it — unless a create or
/// join bound it to a member meanwhile (`grok_leader::stop_launch`).
fn run_outside_grok(args: &[String]) -> ! {
    use crate::adapters::grok_leader;

    let Some((session_id, pass_flag, cwd)) = grok_outside_launch(args) else {
        grok_raw(args); // a shape whose session hive cannot name, or plain grok's own
    };
    if let Some((team, _)) = crate::registry::member_for_session(&session_id, Some("grok")) {
        super::attach::attach_cmd(&team);
        std::process::exit(0);
    }
    let key = grok_leader::mint_launch_key();
    if !grok_leader::spawn_launch_daemon(&key) {
        eprintln!("hive: grok leader did not start; launching plain grok");
        grok_raw(args);
    }
    if let Err(error) = grok_leader::write_session_key(&key, &session_id, &cwd) {
        grok_leader::stop_launch(&key, "");
        eprintln!("hive: {error}; launching plain grok");
        grok_raw(args);
    }
    let session = crate::terminal_handoff::Session::grok(&key, &session_id, &cwd);
    let mut initial: Vec<String> = vec![
        "--leader".to_string(),
        "--leader-socket".to_string(),
        grok_leader::socket_path_for_key(&key)
            .to_string_lossy()
            .into_owned(),
    ];
    if pass_flag {
        initial.push("--session-id".to_string());
        initial.push(session_id.clone());
    }
    initial.extend(args.iter().cloned());
    let result = crate::terminal_handoff::run(&session, &initial);
    grok_leader::stop_launch(&key, &session_id);
    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("hive: {error}; resume with `hive grok --resume {session_id}`");
            std::process::exit(1);
        }
    }
}

pub(crate) fn grok_cmd(args: &[String]) {
    exec_grok_managed(args);
}

// ---------------------------------------------------------------------------
// ccd
// ---------------------------------------------------------------------------

pub(crate) fn ccd_ls_cmd() {
    let members = live_member_pids();
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

pub(crate) fn resume_hint_cmd(cli_name: &str) {
    // Prints nothing and exits 0 on any failure: a hint must never break the
    // wrapper.
    if let Some(hint) = resume_hint(cli_name, &getcwd()) {
        println!("{hint}");
    }
}

fn resume_hint(cli_name: &str, cwd: &str) -> Option<String> {
    let (pane, _team, _agent) = pane_team_identity()?;
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
    if !is_printable(cwd) || !is_printable(&session_id) || session_id.starts_with('-') {
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
fn pane_team_identity() -> Option<(String, String, String)> {
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

#[cfg(test)]
mod tests {
    #[test]
    fn test_codex_resume_uses_recorded_cwd_unless_overridden() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = crate::testenv::EnvGuard::new();
        env.set("CODEX_HOME", tmp.path());
        let sessions = tmp.path().join("sessions/2026/04/02");
        std::fs::create_dir_all(&sessions).unwrap();
        let id = "11111111-2222-4333-8444-555555555555";
        std::fs::write(
            sessions.join(format!("rollout-2026-04-02T00-00-00-{id}.jsonl")),
            serde_json::json!({"type":"session_meta","payload":{"id":id,"cwd":"/original"}})
                .to_string(),
        )
        .unwrap();
        assert_eq!(super::codex_launch_cwd(&[], Some(id)), "/original");
        assert_eq!(
            super::codex_launch_cwd(&["--cd".into(), "/override".into()], Some(id)),
            "/override"
        );
    }

    #[test]
    fn test_codex_cwd_rewrite_keeps_option_values_and_prompt_literal() {
        let mut args: Vec<String> = ["-c", "-Cvalue", "-C", "relative", "--", "--cd=prompt"]
            .into_iter()
            .map(String::from)
            .collect();
        super::normalize_codex_cwd(&mut args, "/absolute");
        assert_eq!(
            args,
            ["-c", "-Cvalue", "-C", "/absolute", "--", "--cd=prompt"]
        );
    }

    use super::*;
    use crate::testkit::{args, display_env, fake_tmux_tagged};

    #[test]
    fn test_raw_shapes_cover_every_passthrough_constant() {
        for sub in CLAUDE_PASSTHROUGH_SUBCOMMANDS {
            assert!(claude_raw_shape(&args(&[sub, "x"])), "{sub}");
        }
        for flag in CLAUDE_PASSTHROUGH_FLAGS.iter().chain(CLAUDE_RAW_MODE_FLAGS) {
            assert!(claude_raw_shape(&args(&["--model", "m", flag])), "{flag}");
        }
        assert!(claude_raw_shape(&args(&["--resume"])));
        assert!(claude_raw_shape(&args(&["-r", "--model", "m"])));
        assert!(claude_raw_shape(&args(&["--resume="])));
        assert!(!claude_raw_shape(&args(&["--resume", "job-1"])));
        assert!(!claude_raw_shape(&args(&["--model", "m"])));
        assert!(!claude_raw_shape(&args(&["a prompt mentioning help"])));
        assert!(!claude_raw_shape(&[]));

        for sub in CODEX_PASSTHROUGH_SUBCOMMANDS {
            assert!(codex_raw_shape(&args(&["-m", "gpt", sub])), "{sub}");
        }
        for flag in CODEX_PASSTHROUGH_FLAGS {
            assert!(codex_raw_shape(&args(&[flag])), "{flag}");
        }
        assert!(codex_raw_shape(&args(&["--remote", "unix:///s"])));
        assert!(codex_raw_shape(&args(&["--remote=unix:///s"])));
        assert!(!codex_raw_shape(&args(&["resume", "t-1"])));
        assert!(!codex_raw_shape(&args(&["fork", "t-1"])));
        // a value that happens to name a subcommand is a value
        assert!(!codex_raw_shape(&args(&["-m", "exec"])));
        assert!(!codex_raw_shape(&[]));

        for sub in GROK_PASSTHROUGH_SUBCOMMANDS {
            assert!(grok_raw_shape(&args(&[sub])), "{sub}");
        }
        for flag in GROK_PASSTHROUGH_FLAGS {
            assert!(grok_raw_shape(&args(&["-m", "grok-4", flag])), "{flag}");
        }
        assert!(!grok_raw_shape(&args(&["--resume"])));
        assert!(!grok_raw_shape(&args(&["-m", "grok-4"])));
        assert!(!grok_raw_shape(&[]));
    }

    #[test]
    fn test_grok_outside_launch_names_the_session_or_hands_the_shape_to_plain_grok() {
        let _env = crate::testenv::EnvGuard::new();
        let cwd = std::fs::canonicalize(std::env::current_dir().unwrap())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let sid = "0f0f0f0f-1111-4222-8333-444444444444";
        // fresh: hive names the session and passes it
        let (id, pass, dir) = grok_outside_launch(&args(&["-m", "grok-4"])).unwrap();
        assert!(is_uuid(&id) && pass && dir == cwd);
        // resumed by id, either spelling
        for form in [
            vec!["-r", sid],
            vec!["--resume", sid],
            vec![&format!("--resume={sid}")],
        ] {
            let (id, pass, _) = grok_outside_launch(&args(&form)).unwrap();
            assert_eq!((id.as_str(), pass), (sid, false), "{form:?}");
        }
        let (id, pass, _) = grok_outside_launch(&args(&["--session-id", sid])).unwrap();
        assert_eq!((id.as_str(), pass), (sid, false));
        // plain grok's: picker, title, continue, non-interactive, own socket
        for form in [
            vec!["--resume"],
            vec!["-r"],
            vec!["-r", "-m", "grok-4"],
            vec!["--resume", "my chat"],
            vec!["-c"],
            vec!["--continue"],
            vec!["-p", "hi"],
            vec!["--single"],
            vec!["--prompt-file", "x"],
            vec!["--prompt-json=x"],
            vec!["--leader-socket", "/tmp/s"],
            vec!["--session-id", "not-a-uuid"],
            vec!["--cwd"],
            vec!["--cwd", "/definitely/not/a/dir"],
        ] {
            assert!(grok_outside_launch(&args(&form)).is_none(), "{form:?}");
        }
        // --cwd is the launch's directory, canonical
        let tmp = tempfile::tempdir().unwrap();
        let (_, _, dir) =
            grok_outside_launch(&args(&["--cwd", tmp.path().to_str().unwrap()])).unwrap();
        assert_eq!(
            dir,
            std::fs::canonicalize(tmp.path()).unwrap().to_string_lossy()
        );
    }

    #[test]
    fn test_a_following_flag_is_not_the_value() {
        let a = args(&["--resume", "-m", "grok-4"]);
        assert_eq!(grok_opt_value(&a, &["--resume"]), None);
        assert_eq!(codex_opt_value(&a, &["--resume"]), None);
    }

    #[test]
    fn test_a_trailing_bare_option_has_no_value() {
        let a = args(&["--resume"]);
        assert_eq!(grok_opt_value(&a, &["--resume"]), None);
        assert_eq!(codex_opt_value(&a, &["--resume"]), None);
    }

    #[test]
    fn test_a_real_value_still_reads() {
        let a = args(&["--resume", "old-sid", "-m", "grok-4"]);
        assert_eq!(
            grok_opt_value(&a, &["--resume"]),
            Some("old-sid".to_string())
        );
        assert_eq!(
            codex_opt_value(&a, &["--resume"]),
            Some("old-sid".to_string())
        );
    }

    #[test]
    fn test_the_equals_form_still_reads() {
        let a = args(&["--resume=old-sid"]);
        assert_eq!(
            grok_opt_value(&a, &["--resume"]),
            Some("old-sid".to_string())
        );
        assert_eq!(
            codex_opt_value(&a, &["--resume"]),
            Some("old-sid".to_string())
        );
    }

    #[test]
    fn test_codex_cwd_does_not_swallow_the_next_flag() {
        assert_eq!(
            codex_opt_value(&args(&["--cd", "--model", "x"]), &["--cd", "-C"]),
            None
        );
        assert_eq!(
            codex_opt_value(&args(&["--cd", "/tmp/w", "--model", "x"]), &["--cd", "-C"]),
            Some("/tmp/w".to_string())
        );
    }

    #[test]
    fn test_grok_resume_before_a_flag_leaves_the_pane_unrecorded() {
        // a bare --resume opens grok's picker: hive cannot know the session id,
        // so it records nothing rather than recording the next flag
        assert_eq!(
            grok_launch_session(&args(&["--resume", "-m", "grok-4"])),
            (None, false)
        );
    }

    #[test]
    fn test_grok_resume_with_an_id_records_that_session() {
        assert_eq!(
            grok_launch_session(&args(&["--resume", "old-sid"])),
            (Some("old-sid".to_string()), false)
        );
    }

    #[test]
    fn test_grok_bare_launch_mints_a_session_and_passes_the_flag() {
        let (sid, pass_flag) = grok_launch_session(&args(&["-m", "grok-4"]));
        assert!(pass_flag);
        assert_eq!(sid.expect("minted session id").len(), 36);
    }

    fn tags_lookup<'a>(
        mapping: &'a [((&'a str, &'a str), &'a str)],
    ) -> impl Fn(&str, &str) -> Option<String> + 'a {
        move |target: &str, key: &str| {
            mapping
                .iter()
                .find(|((t, k), _)| *t == target && *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn test_a_member_pane_mints_the_member_name_for_claude() {
        let mapping = [
            (("%179", "hive-team"), "honey"),
            (("%179", "hive-agent"), "worker"),
        ];
        let label = pane_member_label_via(tags_lookup(&mapping), "%179");
        assert_eq!(mint_name(label, "%179"), "honey.worker");
    }

    #[test]
    fn test_a_member_pane_mints_the_member_name_for_codex() {
        let mapping = [
            (("%9", "hive-team"), "comb"),
            (("%9", "hive-agent"), "validator"),
        ];
        let label = pane_member_label_via(tags_lookup(&mapping), "%9");
        assert_eq!(mint_name(label, "%9"), "comb.validator");
    }

    #[test]
    fn test_an_untagged_pane_falls_back_to_the_pane_placeholder() {
        let mapping: [((&str, &str), &str); 0] = [];
        let label = pane_member_label_via(tags_lookup(&mapping), "%42");
        assert_eq!(mint_name(label, "%42"), "hive-42");
    }

    #[test]
    fn test_a_half_tagged_pane_is_not_a_member() {
        let mapping = [(("%7", "hive-team"), "honey")];
        let label = pane_member_label_via(tags_lookup(&mapping), "%7");
        assert_eq!(mint_name(label, "%7"), "hive-7");
    }

    #[test]
    fn test_codex_subcommand_index_skips_global_options() {
        assert_eq!(
            codex_subcommand_index(&args(&["-c", "k=v", "exec"])),
            Some(2)
        );
        assert_eq!(codex_subcommand_index(&args(&["resume", "sid"])), Some(0));
        assert_eq!(codex_subcommand_index(&args(&["-m", "gpt"])), None);
    }

    #[test]
    fn test_codex_positional_after_skips_flags() {
        let a = args(&["resume", "--model", "x", "sid-1"]);
        assert_eq!(codex_positional_after(&a, 0), Some("sid-1".to_string()));
        assert_eq!(codex_positional_after(&args(&["resume"]), 0), None);
    }

    #[test]
    fn test_claude_resume_arg_shapes() {
        assert_eq!(claude_resume_arg(&args(&[])), (false, None));
        assert_eq!(claude_resume_arg(&args(&["--resume"])), (true, None));
        assert_eq!(
            claude_resume_arg(&args(&["-r", "abc"])),
            (true, Some("abc".to_string()))
        );
        assert_eq!(
            claude_resume_arg(&args(&["--resume=abc"])),
            (true, Some("abc".to_string()))
        );
        assert_eq!(claude_resume_arg(&args(&["--resume", "-m"])), (true, None));
        assert_eq!(claude_resume_arg(&args(&["--resume="])), (true, None));
    }

    #[test]
    fn test_resume_hint_needs_a_tagged_member_pane_and_its_job_record() {
        let mut env = display_env();
        env.env.set("TMUX_PANE", "%5");
        let _argv = fake_tmux_tagged(
            "",
            &[],
            &[("%5", "hive-team", "honey"), ("%5", "hive-agent", "bee")],
        );

        // A member pane with no job record: nothing to resume, no hint.
        assert_eq!(resume_hint("claude", "/tmp/w"), None);

        crate::adapters::claude_bg::write_pane_job("%5", "job-77", "sess-77", "/tmp/w").unwrap();
        let hint = resume_hint("claude", "/tmp/w").expect("a recorded job is resumable");
        assert!(hint.starts_with("Resume from anywhere:\n  "), "{hint}");
        assert!(
            hint.contains("cd /tmp/w && hive claude --resume job-77"),
            "{hint}"
        );

        // The record alone is not enough: an untagged pane is nobody's member.
        env.env.set("TMUX_PANE", "%6");
        crate::adapters::claude_bg::write_pane_job("%6", "job-78", "sess-78", "/tmp/w").unwrap();
        assert_eq!(resume_hint("claude", "/tmp/w"), None);
    }
}
