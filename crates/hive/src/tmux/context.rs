use std::env;
use std::path::{Path, PathBuf};

use super::run::{exec_capture, run};

/// The socket of the tmux server this process's tmux commands reach.
///
/// Inside tmux that is the first field of `TMUX`; outside, what the server
/// tmux resolves by default (`-L`/`TMUX_TMPDIR`) reports as its
/// `socket_path`. None when neither answers.
pub fn own_socket_path() -> Option<String> {
    if let Ok(tmux) = env::var("TMUX") {
        let path = tmux.split(',').next().unwrap_or("").trim();
        if !path.is_empty() {
            return Some(path.to_string());
        }
    }
    let r = run(&["display-message", "-p", "#{socket_path}"], false, 5).ok()?;
    if r.returncode != 0 {
        return None;
    }
    let path = r.stdout.trim();
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

/// tmux's default socket for this uid: `/tmp/tmux-<uid>/default`.
///
/// Deliberately ignores `TMUX_TMPDIR`: a server under a redirected tmpdir
/// is a private server from hive's point of view even when its label is
/// `default`.
pub fn default_socket_path() -> PathBuf {
    PathBuf::from(format!("/tmp/tmux-{}", unsafe { libc::getuid() })).join("default")
}

/// Whether two socket paths name the same socket.
///
/// tmux reports a resolved path (`/private/tmp/...` on macOS) while the
/// default location is spelled `/tmp/...`; canonicalize where the path
/// exists and drop the `/private` prefix either way.
pub fn same_socket(a: &str, b: &str) -> bool {
    fn normalize(path: &str) -> PathBuf {
        let resolved = Path::new(path)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(path));
        match resolved.strip_prefix("/private") {
            Ok(rest) => Path::new("/").join(rest),
            Err(_) => resolved,
        }
    }
    !a.is_empty() && !b.is_empty() && normalize(a) == normalize(b)
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
    rows.sort_by_key(|row| std::cmp::Reverse(row.0));
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

pub fn get_client_mode(target: &str) -> String {
    if target.is_empty() {
        return "unknown".to_string();
    }
    match display_value(target, "#{client_control_mode}").as_deref() {
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
