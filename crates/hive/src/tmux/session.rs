use super::run::{run, run_from};
use std::process::Command;

// --- Session ---

pub fn has_session(name: &str) -> bool {
    match run(&["has-session", "-t", name], false, 5) {
        Ok(r) => r.returncode == 0,
        // A missing tmux reads as "no".
        Err(_) => false,
    }
}

/// Create a detached tmux session. Returns the initial pane id.
///
/// The client runs from `$HIVE_HOME`: when no server is up, tmux forks the
/// server out of this client and the server keeps that working directory
/// for life. A caller's checkout or worktree can be deleted later, and a
/// server whose own cwd is gone cannot `getcwd()`, after which tmux skips
/// the `chdir` for every `-c` it is given (spawn.c) and every new pane is
/// born in the dead directory. hive's own root lives as long as hive's
/// state does; `/` stands in when it cannot be created.
pub fn new_session(
    name: &str,
    width: u32,
    height: u32,
    cwd: Option<&str>,
    command: Option<&str>,
) -> anyhow::Result<String> {
    let cwd = super::pane_cwd(cwd)?;
    let w = width.to_string();
    let h = height.to_string();
    let mut args = vec![
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
    ];
    if let Some(cwd) = cwd {
        args.extend(["-c", cwd]);
    }
    if let Some(command) = command {
        args.push(command);
    }
    let root = crate::paths::hive_home();
    let from = match std::fs::create_dir_all(&root) {
        Ok(()) => root.to_string_lossy().into_owned(),
        Err(_) => "/".to_string(),
    };
    let r = run_from(&args, true, 5, Some(&from))?;
    Ok(r.stdout.trim().to_string())
}

pub fn kill_session(name: &str) {
    let _ = run(&["kill-session", "-t", name], false, 5);
}

pub fn kill_window(target: &str) {
    let _ = run(&["kill-window", "-t", target], false, 5);
}

/// Create a new tmux window in *session*. Returns (window_target, pane_id).
pub fn new_window(
    session: &str,
    name: &str,
    cwd: Option<&str>,
    detach: bool,
    command: Option<&str>,
) -> anyhow::Result<(String, String)> {
    let cwd = super::pane_cwd(cwd)?;
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
    if let Some(command) = command {
        args.push(command);
    }
    let r = run(&args, true, 5)?;
    let out = r.stdout.trim().to_string();
    match out.split_once('\t') {
        None => Ok((out, String::new())),
        Some((target, pane_id)) => Ok((target.to_string(), pane_id.to_string())),
    }
}

/// Break *pane_id* out into its own new window, in the session *target*
/// names when given. Returns (window_target, pane_id).
///
/// The pane's running process (e.g. agent CLI) continues — only its window
/// parent changes.
pub fn break_pane(
    pane_id: &str,
    name: &str,
    detach: bool,
    target: Option<&str>,
) -> anyhow::Result<(String, String)> {
    let mut args: Vec<&str> = vec!["break-pane", "-s", pane_id];
    if detach {
        args.push("-d");
    }
    if let Some(target) = target {
        args.push("-t");
        args.push(target);
    }
    if !name.is_empty() {
        args.push("-n");
        args.push(name);
    }
    args.extend(["-P", "-F", "#{session_name}:#{window_index}\t#{pane_id}"]);
    let r = run(&args, true, 5)?;
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

/// Move pane *src* into *dst*'s window, left of *dst*, without selecting
/// it. The window *src* leaves closes by itself when it was its only pane.
pub fn join_pane_before(src: &str, dst: &str) {
    let _ = run(
        &["join-pane", "-h", "-b", "-d", "-s", src, "-t", dst],
        false,
        5,
    );
}

/// Return (width, height) for *window_target*, or (0, 0) on error.
pub fn window_size(window_target: &str) -> (u32, u32) {
    let r = match run(
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
    match run(
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
    let _ = run(&["select-window", "-t", window_target], false, 5);
}

/// Move the *calling client* to *window_target*.
///
/// The inside-tmux jump of `hive attach`. `select_window` cannot do this
/// job: it sets the current window of the window's own session, so a client
/// attached to another session stays where it is.
pub fn switch_client(window_target: &str) {
    let _ = run(&["switch-client", "-t", window_target], false, 5);
}

/// Session names on this server, for name allocation only.
pub(crate) fn session_names() -> Vec<String> {
    run(&["list-sessions", "-F", "#{session_name}"], false, 5)
        .ok()
        .filter(|r| r.returncode == 0)
        .map(|r| r.stdout.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Switch one explicitly selected client, reporting a failed jump.
pub(crate) fn switch_named_client(client: &str, window: &str) -> anyhow::Result<()> {
    run(&["switch-client", "-c", client, "-t", window], true, 5)?;
    Ok(())
}
