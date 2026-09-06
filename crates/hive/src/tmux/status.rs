//! The team session's status bar and the two bindings that drive the orch
//! mirror. Session options only (`status*` are session-scoped), so a human's
//! global status config is untouched; every value on the bar is a tmux
//! option the CLI or the hived wrote — no `#()` shell-outs, the bar never
//! forks.

use super::run::run;

/// The bar's colours, one set per appearance. The bar follows the same
/// switch as the viewer (`view.theme`, `HIVE_VIEW_THEME`, then detection),
/// resolved once at install: a theme change takes effect at the next
/// session build.
pub struct StatusPalette {
    pub bar: &'static str,
    pub team_bg: &'static str,
    pub team_fg: &'static str,
    pub chip_bg: &'static str,
    pub chip_active_bg: &'static str,
    pub mirror_bg: &'static str,
    pub muted: &'static str,
    pub busy: &'static str,
    pub open: &'static str,
    pub alert: &'static str,
    pub ticker_bg: &'static str,
    pub ticker_fg: &'static str,
}

pub const STATUS_DARK: StatusPalette = StatusPalette {
    bar: "bg=colour235,fg=colour250",
    team_bg: "colour214",
    team_fg: "colour235",
    chip_bg: "colour236",
    chip_active_bg: "colour240",
    mirror_bg: "colour238",
    muted: "colour245",
    busy: "colour214",
    open: "colour114",
    alert: "colour203",
    ticker_bg: "colour234",
    ticker_fg: "colour250",
};

pub const STATUS_LIGHT: StatusPalette = StatusPalette {
    bar: "bg=colour254,fg=colour236",
    team_bg: "colour172",
    team_fg: "colour255",
    chip_bg: "colour252",
    chip_active_bg: "colour248",
    mirror_bg: "colour250",
    muted: "colour243",
    busy: "colour166",
    open: "colour28",
    alert: "colour160",
    ticker_bg: "colour255",
    ticker_fg: "colour238",
};

pub fn status_palette(kind: crate::view_theme::ThemeKind) -> &'static StatusPalette {
    match kind {
        crate::view_theme::ThemeKind::Dark => &STATUS_DARK,
        crate::view_theme::ThemeKind::Light => &STATUS_LIGHT,
    }
}

/// Line 0: the team chip, the orch mirror chip, one chip per pane of the
/// current window (mirror excluded, active pane bold), then `PR<n>`,
/// session name and clock.
pub fn team_status_format_0(p: &StatusPalette) -> String {
    format!(
        concat!(
            "#[bg={team_bg},fg={team_fg},bold] #{{@hive-team}} #[default] ",
            // orch chip only when the window records a mirror choice (`hive
            // mirror`, or `on` written at build for a session mirror);
            // ▴ = closed (parked), ▾ = open (the mirror sits below)
            "#{{?@hive-mirror,#[range=user|hive-mirror]",
            "#{{?#{{==:#{{@hive-mirror}},off}},#[bg={mirror_bg}#,fg={muted}] ▴ orch ,#[bg={mirror_bg}#,fg={open}] ▾ orch }}",
            "#[norange]#[default] ,}}",
            "#{{P:#{{?#{{==:#{{@hive-role}},mirror}},,",
            "#[range=pane|#{{pane_id}}]#{{?pane_active,#[bg={chip_active_bg}#,bold],#[bg={chip_bg}]}}",
            "#{{?@hive-agent,",
            "#{{?@hive-notify-active,#[fg={alert}#,bold] ✱ ,",
            "#{{?@hive-unread,#[fg={alert}] ✱ ,",
            "#{{?@hive-busy,#[fg={busy}] ● ,#[fg={muted}] ○ }}}}}}#{{@hive-agent}} ,",
            "#[fg={muted}] #{{pane_current_command}} }}",
            "#[norange]#[default] }}}}",
            "#[align=right]#[fg={muted}]#{{?@hive-pr,PR#{{@hive-pr}} · ,}}#{{session_name}} · %H:%M "
        ),
        team_bg = p.team_bg,
        team_fg = p.team_fg,
        mirror_bg = p.mirror_bg,
        muted = p.muted,
        open = p.open,
        chip_active_bg = p.chip_active_bg,
        chip_bg = p.chip_bg,
        alert = p.alert,
        busy = p.busy,
    )
}

/// Line 1: the pending notify text (cleared by the select hook), then the
/// hived's ticker.
pub fn team_status_format_1(p: &StatusPalette) -> String {
    format!(
        concat!(
            "#[bg={ticker_bg},fg={ticker_fg}] ",
            "#{{?@hive-notify-text,#[fg={alert}#,bold]✱ #{{@hive-notify-text}}#[default]#[bg={ticker_bg}#,fg={ticker_fg}]   │   ,}}",
            "#{{@hive-ticker}}"
        ),
        ticker_bg = p.ticker_bg,
        ticker_fg = p.ticker_fg,
        alert = p.alert,
    )
}
/// tmux's stock root-table status click as 3.4 ships it: the else branch
/// of the hive click when the server has no click of its own to keep
/// (3.7 ships a different stock click, so the live key is read first).
pub const STOCK_STATUS_CLICK: &str = "select-window -t =";
/// Server option remembering what the status click ran before hive bound
/// it — the same arrangement as `PREFIX_M_FALLBACK_OPTION`.
pub const STATUS_CLICK_FALLBACK_OPTION: &str = "@hive-status-click";
/// Window option tagging the hidden window that parks a closed mirror;
/// value = team name.
pub const HIDDEN_WINDOW_KEY: &str = "hive-hidden";

/// The session options, targeted by session id (`set-option -t =name`
/// is refused).
pub fn team_status_argv(session_id: &str, kind: crate::view_theme::ThemeKind) -> Vec<Vec<String>> {
    let p = status_palette(kind);
    [
        // The chips are click targets: `mouse` is a session option, so the
        // team session turns it on for itself whatever the global says.
        ("mouse", "on".to_string()),
        ("status", "2".to_string()),
        ("status-style", p.bar.to_string()),
        ("status-left", String::new()),
        ("status-right", String::new()),
        ("status-format[0]", team_status_format_0(p)),
        ("status-format[1]", team_status_format_1(p)),
    ]
    .into_iter()
    .map(|(option, value)| {
        vec![
            "set-option".to_string(),
            "-t".to_string(),
            session_id.to_string(),
            option.to_string(),
            value,
        ]
    })
    .collect()
}

/// The shell line both bindings run: `hive mirror` on the clicked/current
/// window. A run-shell job carries no TMUX_PANE, so the window travels as
/// an argument; `q:` shell-quotes it. `hive` is shell-quoted already and
/// gets tmux's double-quote escaping on top (a `$` in the path). Output
/// is discarded: run-shell shows any stdout in view mode over the active
/// pane — a member's TUI — until someone presses q, and a nonzero exit
/// the same way; the binding must never do that to a member.
pub(crate) fn mirror_run_shell(hive: &str) -> String {
    let hive = crate::shell::tmux_dquote_escape(hive);
    format!("run-shell -b \"{hive} mirror --window '#{{q:session_name}}:#{{window_index}}' >/dev/null 2>&1 || true\"")
}

/// `bind-key` argv for the status click: the orch chip runs `hive mirror`,
/// a pane chip selects that pane, anything else is *fallback*, the click
/// the server had before.
pub fn status_click_binding(hive: &str, fallback: &str) -> Vec<String> {
    vec![
        "bind-key".to_string(),
        "-T".to_string(),
        "root".to_string(),
        "MouseDown1Status".to_string(),
        "if-shell".to_string(),
        "-F".to_string(),
        "#{==:#{mouse_status_range},hive-mirror}".to_string(),
        mirror_run_shell(hive),
        format!(
            "if-shell -F \"#{{==:#{{mouse_status_range}},pane}}\" \"select-pane -t =\" \"{fallback}\""
        ),
    ]
}

/// What a status click runs on a line hive does not own: the command
/// found on `MouseDown1Status` when it is not hive's, remembered in
/// `STATUS_CLICK_FALLBACK_OPTION`; what that option remembers when the
/// key already carries hive's binding; tmux 3.4's stock click when the
/// key is unbound.
pub(crate) fn status_click_fallback() -> String {
    remembered_fallback(
        "root",
        "MouseDown1Status",
        "hive-mirror",
        STATUS_CLICK_FALLBACK_OPTION,
    )
    .unwrap_or_else(|| STOCK_STATUS_CLICK.to_string())
}

/// The command on `key` in `table` when it is not hive's (`marker` absent)
/// — remembered in `option` on the way — else what `option` remembers;
/// None when the key is unbound and nothing was remembered.
fn remembered_fallback(table: &str, key: &str, marker: &str, option: &str) -> Option<String> {
    let listed = run(&["list-keys", "-T", table], false, 5)
        .ok()
        .filter(|r| r.returncode == 0)
        .map(|r| r.stdout)
        .unwrap_or_default();
    // An unbound key has nothing to keep; only hive's own binding on it
    // sends the probe to what was remembered when that binding went on.
    let command = bound_command_for(&listed, table, key)?;
    if !command.contains(marker) {
        let _ = run(&["set-option", "-s", option, &command], false, 5);
        return Some(command);
    }
    run(&["show-options", "-s", "-v", option], false, 5)
        .ok()
        .filter(|r| r.returncode == 0)
        .map(|r| r.stdout.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Server option remembering what prefix+m ran before hive bound it, so a
/// later install (another team session on the same server) finds hive's
/// own binding on the key and still has the user's command for the else
/// branch.
pub const PREFIX_M_FALLBACK_OPTION: &str = "@hive-prefix-m";

/// The command bound to prefix+m in a `list-keys -T prefix` table (or the
/// one line 3.4's `list-keys -T prefix m` prints), with tmux's `\;`
/// command separator turned into the ` ; ` an if-shell branch string
/// splits on. None when the key is unbound. Read off the whole table:
/// tmux 3.7 prints nothing for `list-keys -T prefix m`.
pub fn bound_command(listed: &str) -> Option<String> {
    bound_command_for(listed, "prefix", "m")
}

/// `bound_command` for any table and key.
pub fn bound_command_for(listed: &str, table: &str, key: &str) -> Option<String> {
    listed.lines().find_map(|line| {
        let mut rest = line.trim().strip_prefix("bind-key")?.trim_start();
        if let Some(after) = rest.strip_prefix("-r ") {
            rest = after.trim_start();
        }
        for token in ["-T", table, key] {
            rest = rest.strip_prefix(token)?;
            // The key must end here: `m` is not `mm`.
            if !rest.starts_with(char::is_whitespace) {
                return None;
            }
            rest = rest.trim_start();
        }
        (!rest.is_empty()).then(|| rest.replace(" \\; ", " ; "))
    })
}

/// What prefix+m runs on a non-team window: the command found on the key
/// when it is not hive's (tmux's stock `select-pane -m`, or the user's),
/// remembered in `PREFIX_M_FALLBACK_OPTION`; what that option remembers
/// when the key already carries hive's binding; "" when the key is unbound.
pub(crate) fn prefix_m_fallback() -> String {
    remembered_fallback("prefix", "m", "mirror --window", PREFIX_M_FALLBACK_OPTION)
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
        mirror_run_shell(hive),
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
    let hive = crate::shell::shlex_quote(&crate::paths::self_exe());
    let mut rows = team_status_argv(session_id, crate::view_theme::active_theme_kind());
    rows.push(status_click_binding(&hive, &status_click_fallback()));
    rows.push(mirror_key_binding(&hive, &prefix_m_fallback()));
    for row in rows {
        let args: Vec<&str> = row.iter().map(String::as_str).collect();
        let _ = run(&args, false, 5);
    }
}
