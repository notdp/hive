//! The team session's status bar and the two bindings that drive the orch
//! mirror. Session options only (`status*` are session-scoped), so a human's
//! global status config is untouched; every value on the bar is a tmux
//! option the CLI or the hived wrote — no `#()` shell-outs, the bar never
//! forks.

use super::run::_run;

/// Line 0: the team chip, the orch mirror chip, one chip per pane of the
/// current window (mirror excluded, active pane bold), then `PR<n>`,
/// session name and clock.
pub const TEAM_STATUS_FORMAT_0: &str = concat!(
    "#[bg=colour214,fg=colour235,bold] #{@hive-team} #[default] ",
    // orch chip only when the window records a mirror choice (`hive mirror`,
    // or `on` written at build for a session mirror); ▸ = closed, ◂ = open
    "#{?@hive-mirror,#[range=user|hive-mirror]",
    "#{?#{==:#{@hive-mirror},off},#[bg=colour238#,fg=colour245] ▸ orch ,#[bg=colour238#,fg=colour114] ◂ orch }",
    "#[norange]#[default] ,}",
    "#{P:#{?#{==:#{@hive-role},mirror},,",
    "#[range=pane|#{pane_id}]#{?pane_active,#[bg=colour240#,bold],#[bg=colour236]}",
    "#{?@hive-agent,",
    "#{?@hive-notify-active,#[fg=colour203#,bold] ✱ ,",
    "#{?@hive-unread,#[fg=colour203] ✱ ,",
    "#{?@hive-busy,#[fg=colour214] ● ,#[fg=colour245] ○ }}}#{@hive-agent} ,",
    "#{?#{==:#{@hive-role},dock},#[fg=colour245] ⬡ board ,#[fg=colour245] #{pane_current_command} }}",
    "#[norange]#[default] }}",
    "#[align=right]#[fg=colour245]#{?@hive-pr,PR#{@hive-pr} · ,}#{session_name} · %H:%M "
);
/// Line 1: the pending notify text (cleared by the select hook), then the
/// hived's ticker.
pub const TEAM_STATUS_FORMAT_1: &str = concat!(
    "#[bg=colour234,fg=colour250] ",
    "#{?@hive-notify-text,#[fg=colour203#,bold]✱ #{@hive-notify-text}#[default]#[bg=colour234#,fg=colour250]   │   ,}",
    "#{@hive-ticker}"
);
pub const TEAM_STATUS_STYLE: &str = "bg=colour235,fg=colour250";
/// tmux's stock root-table status click, verbatim (`list-keys -T root
/// MouseDown1Status`, 3.4): the else branch of the hive click, so every
/// other status line keeps the click it always had.
pub const _STOCK_STATUS_CLICK: &str = "select-window -t =";
/// Window option tagging the hidden window that parks a closed mirror;
/// value = team name.
pub const HIDDEN_WINDOW_KEY: &str = "hive-hidden";

/// The session options, targeted by session id (`set-option -t =name`
/// is refused).
pub fn team_status_argv(session_id: &str) -> Vec<Vec<String>> {
    [
        // The chips are click targets: `mouse` is a session option, so the
        // team session turns it on for itself whatever the global says.
        ("mouse", "on"),
        ("status", "2"),
        ("status-style", TEAM_STATUS_STYLE),
        ("status-left", ""),
        ("status-right", ""),
        ("status-format[0]", TEAM_STATUS_FORMAT_0),
        ("status-format[1]", TEAM_STATUS_FORMAT_1),
    ]
    .iter()
    .map(|(option, value)| {
        vec![
            "set-option".to_string(),
            "-t".to_string(),
            session_id.to_string(),
            (*option).to_string(),
            (*value).to_string(),
        ]
    })
    .collect()
}

/// The shell line both bindings run: `hive mirror` on the clicked/current
/// window. A run-shell job carries no TMUX_PANE, so the window travels as
/// an argument; `q:` shell-quotes it.
pub fn _mirror_run_shell(hive: &str) -> String {
    format!("run-shell -b \"{hive} mirror --window '#{{q:session_name}}:#{{window_index}}'\"")
}

/// `bind-key` argv for the status click: the orch chip runs `hive mirror`,
/// a pane chip selects that pane, anything else is the stock click.
pub fn status_click_binding(hive: &str) -> Vec<String> {
    vec![
        "bind-key".to_string(),
        "-T".to_string(),
        "root".to_string(),
        "MouseDown1Status".to_string(),
        "if-shell".to_string(),
        "-F".to_string(),
        "#{==:#{mouse_status_range},hive-mirror}".to_string(),
        _mirror_run_shell(hive),
        format!(
            "if-shell -F \"#{{==:#{{mouse_status_range}},pane}}\" \"select-pane -t =\" \"{_STOCK_STATUS_CLICK}\""
        ),
    ]
}

/// Server option remembering what prefix+m ran before hive bound it, so a
/// later install (another team session on the same server) finds hive's
/// own binding on the key and still has the user's command for the else
/// branch.
pub const PREFIX_M_FALLBACK_OPTION: &str = "@hive-prefix-m";

/// The command `list-keys -T prefix m` prints (`bind-key [-r] -T prefix m
/// <command>`), with tmux's `\;` command separator turned into the ` ; `
/// an if-shell branch string splits on. None when the key is unbound.
pub fn _bound_command(listed: &str) -> Option<String> {
    let mut rest = listed.trim().strip_prefix("bind-key")?.trim_start();
    if let Some(after) = rest.strip_prefix("-r ") {
        rest = after.trim_start();
    }
    for token in ["-T", "prefix", "m"] {
        rest = rest.strip_prefix(token)?.trim_start();
    }
    (!rest.is_empty()).then(|| rest.replace(" \\; ", " ; "))
}

/// What prefix+m runs on a non-team window: the command found on the key
/// when it is not hive's (tmux's stock `select-pane -m`, or the user's),
/// remembered in `PREFIX_M_FALLBACK_OPTION`; what that option remembers
/// when the key already carries hive's binding; "" when the key is unbound.
pub fn _prefix_m_fallback() -> String {
    let listed = _run(&["list-keys", "-T", "prefix", "m"], false, 5)
        .ok()
        .filter(|r| r.returncode == 0)
        .map(|r| r.stdout)
        .unwrap_or_default();
    let Some(command) = _bound_command(&listed) else {
        return String::new();
    };
    if !command.contains("mirror --window") {
        let _ = _run(
            &["set-option", "-s", PREFIX_M_FALLBACK_OPTION, &command],
            false,
            5,
        );
        return command;
    }
    _run(
        &["show-options", "-s", "-v", PREFIX_M_FALLBACK_OPTION],
        false,
        5,
    )
    .ok()
    .filter(|r| r.returncode == 0)
    .map(|r| r.stdout.trim().to_string())
    .unwrap_or_default()
}

/// `prefix+m` runs `hive mirror` on a team window; elsewhere it runs
/// *fallback*, the command the key had before (the key table is
/// server-global, the gate is `@hive-team`).
pub fn mirror_key_binding(hive: &str, fallback: &str) -> Vec<String> {
    let mut row = vec![
        "bind-key".to_string(),
        "-T".to_string(),
        "prefix".to_string(),
        "m".to_string(),
        "if-shell".to_string(),
        "-F".to_string(),
        "#{@hive-team}".to_string(),
        _mirror_run_shell(hive),
    ];
    if !fallback.is_empty() {
        row.push(fallback.to_string());
    }
    row
}

/// Install the bar on a team session and the two server-global bindings
/// (idempotent: every row is a plain set, and the prefix+m probe reads the
/// same fallback back from behind hive's own binding).
pub fn install_team_status(session_id: &str) {
    let hive = crate::cli::util::shlex_quote(&crate::cli::util::self_exe());
    let mut rows = team_status_argv(session_id);
    rows.push(status_click_binding(&hive));
    rows.push(mirror_key_binding(&hive, &_prefix_m_fallback()));
    for row in rows {
        let args: Vec<&str> = row.iter().map(String::as_str).collect();
        let _ = _run(&args, false, 5);
    }
}
