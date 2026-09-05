use super::run::_run;
use std::process::Command;

// --- Session ---

pub fn has_session(name: &str) -> bool {
    match _run(&["has-session", "-t", name], false, 5) {
        Ok(r) => r.returncode == 0,
        // Python would raise OSError here (missing tmux); read it as "no".
        Err(_) => false,
    }
}

/// Create a detached tmux session. Returns the initial pane id.
pub fn new_session(name: &str, width: u32, height: u32) -> anyhow::Result<String> {
    let w = width.to_string();
    let h = height.to_string();
    let r = _run(
        &[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            &w,
            "-y",
            &h,
            "-P",
            "-F",
            "#{pane_id}",
        ],
        true,
        5,
    )?;
    Ok(r.stdout.trim().to_string())
}

pub fn kill_session(name: &str) {
    let _ = _run(&["kill-session", "-t", name], false, 5);
}

pub fn kill_window(target: &str) {
    let _ = _run(&["kill-window", "-t", target], false, 5);
}

/// Create a new tmux window in *session*. Returns (window_target, pane_id).
pub fn new_window(
    session: &str,
    name: &str,
    cwd: Option<&str>,
    detach: bool,
) -> anyhow::Result<(String, String)> {
    // Force `-t` to reference a session, not a window index. Bare numeric
    // session names (e.g. "613") are ambiguous and tmux can treat `-t 613`
    // as an index rather than a session, which fails with "index N in use"
    // once any window exists at that index.
    let target = if session.contains(':') || session.starts_with('$') {
        session.to_string()
    } else {
        format!("{session}:")
    };
    let mut args: Vec<&str> = vec!["new-window", "-t", &target];
    if detach {
        args.push("-d");
    }
    if !name.is_empty() {
        args.push("-n");
        args.push(name);
    }
    if let Some(cwd) = cwd {
        args.push("-c");
        args.push(cwd);
    }
    args.extend(["-P", "-F", "#{session_name}:#{window_index}\t#{pane_id}"]);
    let r = _run(&args, true, 5)?;
    let out = r.stdout.trim().to_string();
    match out.split_once('\t') {
        None => Ok((out, String::new())),
        Some((target, pane_id)) => Ok((target.to_string(), pane_id.to_string())),
    }
}

/// Break *pane_id* out into its own new window. Returns (window_target, pane_id).
///
/// The pane's running process (e.g. agent CLI) continues — only its window
/// parent changes.
pub fn break_pane(pane_id: &str, name: &str, detach: bool) -> anyhow::Result<(String, String)> {
    let mut args: Vec<&str> = vec!["break-pane", "-s", pane_id];
    if detach {
        args.push("-d");
    }
    if !name.is_empty() {
        args.push("-n");
        args.push(name);
    }
    args.extend(["-P", "-F", "#{session_name}:#{window_index}\t#{pane_id}"]);
    let r = _run(&args, true, 5)?;
    let out = r.stdout.trim().to_string();
    match out.split_once('\t') {
        None => Ok((out, pane_id.to_string())),
        Some((target, new_pane_id)) => {
            let new_pane_id = if new_pane_id.is_empty() {
                pane_id
            } else {
                new_pane_id
            };
            Ok((target.to_string(), new_pane_id.to_string()))
        }
    }
}

/// Return (width, height) for *window_target*, or (0, 0) on error.
pub fn window_size(window_target: &str) -> (u32, u32) {
    let r = match _run(
        &[
            "display-message",
            "-t",
            window_target,
            "-p",
            "#{window_width}\t#{window_height}",
        ],
        false,
        5,
    ) {
        Ok(r) => r,
        Err(_) => return (0, 0),
    };
    let out = r.stdout.trim();
    match out.split_once('\t') {
        None => (0, 0),
        Some((w, h)) => match (w.parse(), h.parse()) {
            (Ok(w), Ok(h)) => (w, h),
            _ => (0, 0),
        },
    }
}

/// True when a pane in *window_target* is zoomed (unknown reads as False).
pub fn window_zoomed(window_target: &str) -> bool {
    match _run(
        &[
            "display-message",
            "-t",
            window_target,
            "-p",
            "#{window_zoomed_flag}",
        ],
        false,
        5,
    ) {
        Ok(r) => r.stdout.trim() == "1",
        Err(_) => false,
    }
}

/// Replace this process with `tmux attach` focused on *window_target*.
///
/// The outside-tmux tail of `hive attach`: attach to the session and select
/// the team's window in one tmux command chain. Only returns on exec
/// failure.
pub fn exec_attach(session: &str, window_target: &str) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let err = Command::new("tmux")
        .args([
            "attach",
            "-t",
            session,
            ";",
            "select-window",
            "-t",
            window_target,
        ])
        .exec();
    Err(err.into())
}

pub fn select_window(window_target: &str) {
    let _ = _run(&["select-window", "-t", window_target], false, 5);
}

/// Move the *calling client* to *window_target*.
///
/// The inside-tmux jump of `hive attach`. `select_window` cannot do this
/// job: it sets the current window of the window's own session, so a client
/// attached to another session stays where it is.
pub fn switch_client(window_target: &str) {
    let _ = _run(&["switch-client", "-t", window_target], false, 5);
}
