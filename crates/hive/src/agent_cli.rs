// pending port (wave 2) — only the seam claude_view needs is ported so far.

/// Python agent_cli.normalize_command: basename, lowercase, strip leading
/// dashes, alias-fold (claude-code/claudecode/claude.exe -> claude).
fn normalize_command(command: &str) -> String {
    let value = command.trim().to_lowercase();
    let value = value.rsplit('/').next().unwrap_or("").trim_start_matches('-');
    match value {
        "claude-code" | "claudecode" | "claude.exe" => "claude".to_string(),
        other => other.to_string(),
    }
}

fn is_claude(token: &str) -> bool {
    normalize_command(token) == "claude"
}

/// Pid of the live claude process on *pane_id*'s tty (process evidence only,
/// same matchers as Python detect_profile_from_process restricted to claude:
/// comm/argv[0], or the verified `node <.../claude> ...` wrapper shape).
pub fn claude_pid_for_pane(pane_id: &str) -> Option<i32> {
    let tty = crate::tmux::get_pane_tty(pane_id)?;
    for process in crate::tmux::list_tty_processes(&tty) {
        let argv_parts: Vec<String> = shlex_split(&process.argv);
        let matched = is_claude(&process.command)
            || argv_parts.first().map(|p| is_claude(p)).unwrap_or(false)
            || (argv_parts.len() >= 2
                && normalize_command(&argv_parts[0]) == "node"
                && is_claude(&argv_parts[1]));
        if matched {
            if let Ok(pid) = process.pid.parse::<i32>() {
                return Some(pid);
            }
        }
    }
    None
}

/// Python shlex.split with a whitespace-split fallback on parse errors.
fn shlex_split(argv: &str) -> Vec<String> {
    // ponytail: plain whitespace split — the quoted-token cases shlex handles
    // don't change which token is argv[0]/argv[1] for real ps output; the
    // wave-2 agent_cli port replaces this file wholesale.
    argv.split_whitespace().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_command_folds_aliases_and_paths() {
        assert_eq!(normalize_command("/usr/local/bin/Claude-Code"), "claude");
        assert_eq!(normalize_command("claude.exe"), "claude");
        assert_eq!(normalize_command("-zsh"), "zsh");
    }
}
