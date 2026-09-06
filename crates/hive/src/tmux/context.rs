use super::run::{exec_capture, run};

// --- Context detection ---

fn env_string(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

/// True inside a tmux client — or inside a member engine's tool subprocess.
///
/// A claude bg engine runs on the supervisor's pty, not in any tmux client,
/// so its tools see no reliable $TMUX; but the member's pane identity is
/// resolvable from the engine's own env markers, and the tmux server on the
/// default socket answers targeted commands without $TMUX. Gating on $TMUX
/// alone would lock every member out of hive.
pub fn is_inside_tmux() -> bool {
    if !env_string("TMUX").is_empty() {
        return true;
    }
    member_env_pane().is_some()
}

/// Pane resolved from a member engine's per-tool env markers, or None.
///
/// - codex injects the thread's `CODEX_THREAD_ID` into tool subprocesses;
///   hive records which pane each thread is bound to.
/// - a claude bg engine's tools carry `CLAUDE_CODE_MESSAGING_SOCKET`
///   (`/tmp/cc-socks/<enginePid>.sock`); the engine's registry entry names
///   its jobId, and hive records which pane each job is bound to. An
///   interactive claude session's tools carry the socket too, but have no
///   bg registry entry (and no job record), so they fall through.
/// - a grok member's leader exports `GROK_SESSION_ID` into its tools; that
///   id keys the member's grok roster row, and the member's pane is the one
///   tagged with that team and name on the default server. The leader
///   carries no `TMUX_PANE` (it is minted by identity before any pane
///   exists), so display is resolved from identity here, as for the other
///   two.
///
/// A member whose pane is gone (window closed, server restarted) resolves
/// nothing here; its identity is the registry row keyed by its sessionId —
/// the ladder's session rung (`cli::util::session_member_binding`).
fn member_env_pane() -> Option<String> {
    let thread_id = env_string("CODEX_THREAD_ID").trim().to_string();
    if !thread_id.is_empty() {
        if let Some(pane) = crate::adapters::codex_app_server::pane_for_thread(&thread_id) {
            if !pane.is_empty() {
                return Some(pane);
            }
        }
    }
    let sock = env_string("CLAUDE_CODE_MESSAGING_SOCKET")
        .trim()
        .to_string();
    if !sock.is_empty() {
        let base = sock.rsplit('/').next().unwrap_or("");
        let stem = match base.rfind('.') {
            Some(i) => &base[..i],
            None => base,
        };
        if !stem.is_empty() && stem.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(pid) = stem.parse::<u32>() {
                if let Some(engine) = crate::adapters::claude_bg::engine_session_for_pid(pid) {
                    if let Some(pane) = crate::adapters::claude_bg::pane_for_job(&engine.job_id) {
                        if !pane.is_empty() {
                            return Some(pane);
                        }
                    }
                }
            }
        }
    }
    let grok_session = env_string("GROK_SESSION_ID").trim().to_string();
    if !grok_session.is_empty() {
        if let Some((team, member)) =
            crate::registry::member_for_session(&grok_session, Some("grok"))
        {
            if let Some(pane) = super::listing::list_panes_all()
                .into_iter()
                .find(|p| p.team == team && p.agent == member)
            {
                return Some(pane.pane_id);
            }
        }
    }
    // A pane-keyed grok leader (a raw `hive grok` outside any team) pins
    // its pane's TMUX_PANE into the env it spawns tools with, but carries
    // no $TMUX; a member's identity-keyed leader pins nothing (the rung
    // above is its display). Trust a pinned pane only when it is real on
    // the default server.
    let pinned = env_string("TMUX_PANE").trim().to_string();
    if !pinned.is_empty() && env_string("TMUX").is_empty() {
        if let Ok(r) = run(
            &["display-message", "-t", &pinned, "-p", "#{pane_id}"],
            false,
            5,
        ) {
            if r.stdout.trim() == pinned {
                return Some(pinned);
            }
        }
    }
    None
}

/// Get the pane id of the calling process.
///
/// Inside a member engine's tool subprocess the env's TMUX_PANE is
/// unreliable — the codex shared daemon's env is frozen at spawn time (and
/// hive strips TMUX_PANE from it), and a claude bg engine has none at all —
/// so the per-CLI identity markers win over the env var (see
/// `member_env_pane`); everywhere else the per-pane TMUX_PANE env var
/// is the answer.
pub fn get_current_pane_id() -> Option<String> {
    if let Some(pane) = member_env_pane() {
        if !pane.is_empty() {
            return Some(pane);
        }
    }
    std::env::var("TMUX_PANE").ok()
}

fn current_pane_display(fmt: &str) -> Option<String> {
    let pane_id = get_current_pane_id()?;
    if pane_id.is_empty() {
        return None;
    }
    let r = run(&["display-message", "-t", &pane_id, "-p", fmt], false, 5).ok()?;
    let out = r.stdout.trim().to_string();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Get the window target that contains the calling pane.
pub fn get_current_window_target() -> Option<String> {
    current_pane_display("#{session_name}:#{window_index}")
}

/// Get the tmux session name for the calling pane.
pub fn get_current_session_name() -> Option<String> {
    current_pane_display("#{session_name}")
}

/// Get the stable tmux window id for the calling pane.
pub fn get_current_window_id() -> Option<String> {
    let pane_id = get_current_pane_id()?;
    if pane_id.is_empty() {
        return None;
    }
    display_value(&pane_id, "#{window_id}")
}

pub fn display_value(target: &str, fmt: &str) -> Option<String> {
    let r = run(&["display-message", "-t", target, "-p", fmt], false, 5).ok()?;
    let val = r.stdout.trim().to_string();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

/// True only when tmux resolves `window_id` to itself.
///
/// Never errors: a missing tmux binary, timeout, nonzero exit, or mismatched
/// id all mean "not alive" to callers making reap decisions.
pub fn window_exists(window_id: &str) -> bool {
    if window_id.is_empty() {
        return false;
    }
    match run(
        &["display-message", "-t", window_id, "-p", "#{window_id}"],
        false,
        5,
    ) {
        Ok(r) => r.returncode == 0 && r.stdout.trim() == window_id,
        Err(_) => false,
    }
}

/// `run-shell -b <command>`: the shell string is passed byte-for-byte.
pub fn run_shell_detached(command: &str) {
    let _ = run(&["run-shell", "-b", command], false, 5);
}

pub fn get_most_recent_client_tty(session_name: Option<&str>) -> Option<String> {
    let rows = list_terminal_clients(session_name);
    rows.into_iter().next().map(|row| row.1)
}

/// Terminal (non-control-mode) clients as `(activity, tty)`, newest first.
fn list_terminal_clients(session_name: Option<&str>) -> Vec<(i64, String)> {
    let mut args: Vec<&str> = vec!["list-clients"];
    if let Some(session) = session_name {
        if !session.is_empty() {
            args.push("-t");
            args.push(session);
        }
    }
    args.extend([
        "-F",
        "#{client_activity}\t#{client_control_mode}\t#{pane_id}\t#{client_tty}",
    ]);
    let r = match run(&args, false, 5) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut rows: Vec<(i64, String)> = Vec::new();
    for line in r.stdout.trim().split('\n') {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() != 4 || parts[1] != "0" || parts[2].is_empty() || parts[3].is_empty() {
            continue;
        }
        let raw = if parts[0].is_empty() { "0" } else { parts[0] };
        let activity: i64 = raw.parse().unwrap_or(0);
        rows.push((activity, parts[3].to_string()));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows
}

pub fn get_client_window_target(client_tty: &str) -> Option<String> {
    if client_tty.is_empty() {
        return None;
    }
    let r = run(
        &[
            "display-message",
            "-c",
            client_tty,
            "-p",
            "#{session_name}:#{window_index}",
        ],
        false,
        5,
    )
    .ok()?;
    let out = r.stdout.trim().to_string();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

pub fn get_most_recent_client_window(session_name: Option<&str>) -> Option<String> {
    let client_tty = get_most_recent_client_tty(session_name)?;
    if client_tty.is_empty() {
        return None;
    }
    get_client_window_target(&client_tty)
}

pub fn get_client_mode(target: Option<&str>) -> String {
    let resolved_target = match target {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => match get_current_pane_id() {
            Some(p) if !p.is_empty() => p,
            _ => return "unknown".to_string(),
        },
    };
    match display_value(&resolved_target, "#{client_control_mode}").as_deref() {
        Some("1") => "control".to_string(),
        Some("0") => "terminal".to_string(),
        _ => "unknown".to_string(),
    }
}

pub fn get_pane_window_name(pane_id: &str) -> Option<String> {
    display_value(pane_id, "#{window_name}")
}

pub fn rename_window(window_target: &str, name: &str) {
    let _ = run(&["rename-window", "-t", window_target, name], false, 5);
}

pub fn get_pane_tty(pane_id: &str) -> Option<String> {
    display_value(pane_id, "#{pane_tty}")
}

pub fn get_pane_title(pane_id: &str) -> Option<String> {
    display_value(pane_id, "#{pane_title}")
}

pub fn get_pane_current_command(pane_id: &str) -> Option<String> {
    display_value(pane_id, "#{pane_current_command}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TTYProcessInfo {
    pub pid: String,
    pub command: String,
    pub argv: String,
}

/// Split on whitespace runs, at most 3 parts.
fn split_whitespace_max3(row: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = Vec::new();
    let mut rest = row.trim_start();
    for _ in 0..2 {
        match rest.find(|c: char| c.is_whitespace()) {
            Some(idx) => {
                parts.push(&rest[..idx]);
                rest = rest[idx..].trim_start();
            }
            None => break,
        }
    }
    if !rest.is_empty() {
        parts.push(rest);
    }
    parts
}

pub fn list_tty_processes(tty: &str) -> Vec<TTYProcessInfo> {
    let mut tty_name = tty.trim().to_string();
    if tty_name.is_empty() {
        return Vec::new();
    }
    if let Some(stripped) = tty_name.strip_prefix("/dev/") {
        tty_name = stripped.to_string();
    }
    let argv: Vec<String> = ["ps", "-t", &tty_name, "-o", "pid=,comm=,command="]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let result = match exec_capture(&argv, 5, None) {
        Ok(r) => r,
        // TimeoutExpired -> []; a missing ps binary degrades the same way.
        Err(_) => return Vec::new(),
    };
    let mut processes: Vec<TTYProcessInfo> = Vec::new();
    for line in result.stdout.lines() {
        let row = line.trim();
        if row.is_empty() {
            continue;
        }
        let parts = split_whitespace_max3(row);
        if parts.len() < 2 {
            continue;
        }
        processes.push(TTYProcessInfo {
            pid: parts[0].to_string(),
            command: parts[1].to_string(),
            argv: if parts.len() > 2 { parts[2] } else { parts[1] }.to_string(),
        });
    }
    processes
}

pub fn get_pane_window_target(pane_id: &str) -> Option<String> {
    display_value(pane_id, "#{session_name}:#{window_index}")
}

pub fn get_window_id(target: &str) -> Option<String> {
    display_value(target, "#{window_id}")
}

pub fn get_pane_session_name(pane_id: &str) -> Option<String> {
    display_value(pane_id, "#{session_name}")
}
