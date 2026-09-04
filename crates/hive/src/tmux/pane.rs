use super::context::display_value;
use super::run::{_run, _run_output, exec_capture, TmuxError};

// --- Pane ---

/// Split a window/pane. Returns the new pane id.
///
/// detach=true (default at call sites) keeps focus on the original pane (-d flag).
pub fn split_window(
    target: &str,
    horizontal: bool,
    size: Option<&str>,
    detach: bool,
    cwd: Option<&str>,
) -> anyhow::Result<String> {
    let mut args: Vec<&str> = vec!["split-window", "-t", target];
    if detach {
        args.push("-d");
    }
    args.push(if horizontal { "-h" } else { "-v" });
    if let Some(size) = size {
        if !size.is_empty() {
            args.push("-l");
            args.push(size);
        }
    }
    if let Some(cwd) = cwd {
        args.push("-c");
        args.push(cwd);
    }
    args.extend(["-P", "-F", "#{pane_id}"]);
    match _run(&args, true, 5) {
        Ok(r) => Ok(r.stdout.trim().to_string()),
        Err(TmuxError::CalledProcess { stderr, .. }) => {
            let stderr = stderr.trim();
            let detail = if stderr.is_empty() {
                String::new()
            } else {
                format!(" ({stderr})")
            };
            Err(anyhow::anyhow!(
                "tmux refused to split {target}{detail} — the window is likely \
full; kill a finished member (hive kill <name>) and retry"
            ))
        }
        Err(e) => Err(e.into()),
    }
}

/// Send literal text to a pane, then optionally press Enter.
///
/// Uses two separate tmux invocations to avoid command-chaining (;)
/// interfering with literal text parsing, which caused truncation.
pub fn send_keys(pane_id: &str, text: &str, enter: bool) -> anyhow::Result<()> {
    _run(&["send-keys", "-t", pane_id, "-l", text], true, 5)?;
    if enter {
        _run(&["send-keys", "-t", pane_id, "Enter"], true, 5)?;
    }
    Ok(())
}

/// Send a special key (Escape, C-c, C-n, etc.).
pub fn send_key(pane_id: &str, key: &str) -> anyhow::Result<()> {
    _run(&["send-keys", "-t", pane_id, key], true, 5)?;
    Ok(())
}

/// Send multiple keys in one tmux call (atomic w.r.t. tmux server).
pub fn send_keys_batch(pane_id: &str, keys: &[&str]) -> anyhow::Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let mut args: Vec<&str> = vec!["send-keys", "-t", pane_id];
    args.extend_from_slice(keys);
    _run(&args, true, 5)?;
    Ok(())
}

/// Load data into a named tmux buffer via stdin.
///
/// Errors on failure (nonzero exit or timeout): callers clear the pane's
/// input on the strength of the buffer holding the draft, so a save that did
/// not happen must not read as one.
pub fn load_buffer(name: &str, data: &str) -> anyhow::Result<()> {
    let argv: Vec<String> = ["tmux", "load-buffer", "-b", name, "-"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let r = exec_capture(&argv, 5, Some(data))?;
    if r.returncode != 0 {
        return Err(TmuxError::CalledProcess {
            returncode: r.returncode,
            stderr: r.stderr,
        }
        .into());
    }
    Ok(())
}

/// Paste a named tmux buffer into a pane (optionally with bracketed-paste sequences).
pub fn paste_buffer(name: &str, target: &str, bracketed: bool) {
    let mut args: Vec<&str> = vec!["paste-buffer", "-b", name, "-t", target];
    if bracketed {
        args.insert(1, "-p");
    }
    let _ = _run(&args, false, 5);
}

pub fn delete_buffer(name: &str) {
    let _ = _run(&["delete-buffer", "-b", name], false, 5);
}

/// The pid of the process tmux runs in *pane_id*, when tmux answers.
///
/// The pane is a display record and disappears the instant kill-pane runs;
/// the process it hosted takes longer to go. A caller that must know the
/// process is really gone reads this first and waits on the pid.
pub fn pane_pid(pane_id: &str) -> Option<u32> {
    display_value(pane_id, "#{pane_pid}")?
        .trim()
        .parse()
        .ok()
        .filter(|pid| *pid > 0)
}

pub fn is_pane_in_mode(pane_id: &str) -> bool {
    display_value(pane_id, "#{pane_in_mode}").as_deref() == Some("1")
}

pub fn cancel_pane_mode(pane_id: &str) {
    let _ = _run(&["copy-mode", "-q", "-t", pane_id], false, 5);
}

/// Capture pane content.
pub fn capture_pane(pane_id: &str, lines: u32, preserve_styles: bool) -> anyhow::Result<String> {
    let start = format!("-{lines}");
    let mut args: Vec<&str> = vec!["capture-pane", "-t", pane_id];
    if preserve_styles {
        args.push("-e");
    }
    args.extend(["-p", "-S", &start]);
    _run_output(&args)
}

pub fn is_pane_alive(pane_id: &str) -> bool {
    let r = match _run(
        &["list-panes", "-a", "-F", "#{pane_id} #{pane_dead}"],
        false,
        5,
    ) {
        Ok(r) => r,
        Err(_) => return true,
    };
    if r.returncode != 0 {
        // tmux didn't answer (timeout / transient failure): unknown is not
        // dead. Callers take irreversible action on False (daemon reap, team
        // GC), so only a successful listing may declare a pane dead.
        return true;
    }
    for line in r.stdout.trim().split('\n') {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[0] == pane_id {
            return parts[1] == "0";
        }
    }
    false
}

pub fn kill_pane(pane_id: &str) {
    let _ = _run(&["kill-pane", "-t", pane_id], false, 5);
}

pub fn kill_window(target: &str) {
    let _ = _run(&["kill-window", "-t", target], false, 5);
}

// --- Layout & Appearance ---

pub fn select_layout(target: &str, layout: &str) {
    let _ = _run(&["select-layout", "-t", target, layout], false, 5);
}

pub fn set_pane_title(pane_id: &str, title: &str) {
    let _ = _run(&["select-pane", "-t", pane_id, "-T", title], false, 5);
}

// A claude member pane is an attach *viewer*: the human can switch it to
// another bg session while the pane keeps its member tags. The hived's view
// probe writes what is really on screen into `@hive-view` (empty while the
// pane shows its own member), so the border reads "name -> what you are
// actually looking at" without the format having to guess from the title.
// Both halves carry the team: with several teams on screen, a bare member
// name says nothing about which team a pane belongs to, and
// the view suffix already names its member as `<team>.<member>`.
pub const _HIVE_PANE_BORDER_FORMAT: &str = concat!(
    " #{?@hive-notify-active,#[fg=colour220]#[bold][!] #[default],}",
    "#{?@hive-agent,#{?@hive-team,#{@hive-team}.,}#{@hive-agent}",
    "#{?@hive-view,#[fg=colour220] -> #{@hive-view}#[default],}",
    ",#{pane_title}} "
);

/// Enable pane border labels for a window.
///
/// Hive-tagged panes show their member name; untagged panes fall back to the
/// native tmux pane title.
pub fn enable_pane_border_status(target: &str) {
    let _ = _run(
        &[
            "set-window-option",
            "-t",
            target,
            "pane-border-status",
            "top",
        ],
        false,
        5,
    );
    let _ = _run(
        &[
            "set-window-option",
            "-t",
            target,
            "pane-border-format",
            _HIVE_PANE_BORDER_FORMAT,
        ],
        false,
        5,
    );
}

/// Apply tmux window options expected for Hive-managed panes.
pub fn configure_hive_window(target: &str) {
    enable_pane_border_status(target);
    set_window_option(target, "monitor-activity", "off");
    set_window_option(target, "monitor-bell", "off");
}

pub fn set_window_option(target: &str, option: &str, value: &str) {
    let _ = _run(
        &["set-window-option", "-t", target, option, value],
        false,
        5,
    );
}

pub fn get_window_option(target: &str, key: &str) -> Option<String> {
    let fmt = format!("#{{@{key}}}");
    let r = _run(&["display-message", "-t", target, "-p", &fmt], false, 5).ok()?;
    let val = r.stdout.trim().to_string();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

/// Global (server-wide) window-option value — read-only, no target.
///
/// Values keep their exact spacing (status formats carry meaningful
/// leading/trailing padding); only the trailing newline is removed.
pub fn get_global_window_option(option: &str) -> Option<String> {
    let r = _run(&["show-options", "-w", "-g", "-v", option], false, 5).ok()?;
    let val = r.stdout.trim_end_matches('\n').to_string();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

pub fn clear_window_option(target: &str, option: &str) {
    let _ = _run(&["set-window-option", "-t", target, "-u", option], false, 5);
}

/// List all pane ids in a window/session.
pub fn list_panes(target: &str) -> Vec<String> {
    match _run(&["list-panes", "-t", target, "-F", "#{pane_id}"], false, 5) {
        Ok(r) => r
            .stdout
            .trim()
            .split('\n')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Replace whatever runs in `pane_id` with `command` (a shell line).
pub fn respawn_pane(pane_id: &str, command: &str) -> anyhow::Result<()> {
    _run(&["respawn-pane", "-k", "-t", pane_id, command], true, 5)?;
    Ok(())
}

/// Swap two panes' positions in the window (and in its pane order).
pub fn swap_pane(src: &str, dst: &str) {
    let _ = _run(&["swap-pane", "-d", "-s", src, "-t", dst], false, 5);
}
