//! The team session's status bar and the two bindings that drive the orch
//! mirror. Session options only (`status*` are session-scoped), so a human's
//! global status config is untouched; every value on the bar is a tmux
//! option the CLI or the hived wrote — no `#()` shell-outs, the bar never
//! forks.

use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

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

/// Rows for the team session bar and the two server-global bindings
/// (idempotent: every row is a plain set, and the prefix+m probe reads the
/// same fallback back from behind hive's own binding).
fn install_status_rows(session_id: &str) -> Vec<Vec<String>> {
    let hive = crate::shell::shlex_quote(&crate::paths::self_exe());
    let mut rows = team_status_argv(session_id, crate::view_theme::active_theme_kind());
    rows.push(status_click_binding(&hive, &status_click_fallback()));
    rows.push(mirror_key_binding(&hive, &prefix_m_fallback()));
    rows
}

/// The two session hooks that fire when a terminal arrives at a session
/// showing a team window — a fresh attach, or a client switching over from
/// another session — each running `hive wake` on the client's session. A
/// desk that retired because nobody was watching (`hived.sleep
/// unwatched`) comes back the moment someone looks, so the bar and the
/// colours are live again without a hive verb being typed.
///
/// Each hook is an indexed array: hive takes one index per hive home and
/// leaves every other entry — the human's own hook, another hive home's
/// wake — untouched, so a session that lends a team its window keeps
/// what the human configured on it.
pub(crate) const WAKE_HOOKS: [&str; 2] = ["client-attached", "client-session-changed"];

/// The engine homes a hook carries when the installing process has them
/// set: the wake runs under the tmux server's environment, which is
/// whatever its first client had, not the caller's.
const WAKE_ENGINE_HOMES: [&str; 4] = [
    "CLAUDE_HOME",
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "GROK_HOME",
];

/// The hive home a wake hook is installed for: the resolved absolute path,
/// which is also how the installer and the remover recognise their own
/// entries, so a relative `HIVE_HOME` spelling finds the entry it baked.
fn wake_hive_home() -> String {
    std::path::absolute(crate::paths::hive_home())
        .unwrap_or_else(|_| crate::paths::hive_home())
        .to_string_lossy()
        .into_owned()
}

/// `HIVE_HOME`, the caller's `HOME` and its engine homes, as the `VAR=value`
/// assignments the wake command starts with. The hive home is the
/// resolved absolute path, set whether or not the caller had it in its
/// environment: the hook must name the home it was installed for, never
/// resolve one from the server's environment.
pub(crate) fn wake_environment() -> Vec<(String, String)> {
    let mut env = vec![("HIVE_HOME".to_string(), wake_hive_home())];
    if let Ok(user_home) = std::env::var("HOME") {
        if !user_home.is_empty() {
            env.push(("HOME".to_string(), user_home));
        }
    }
    for key in WAKE_ENGINE_HOMES {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                env.push((key.to_string(), value));
            }
        }
    }
    env
}

/// The shell line a wake hook runs: the environment assignments, the
/// binary and `wake --session` naming the client's session by id.
/// `#{q:session_id}` is tmux format quoting — run-shell expands it to
/// `\$3`, which reaches `sh` as the id itself; a shell quote around it
/// would keep the backslash. Every argv piece is shell-quoted here; the
/// tmux double-quote escaping of the whole line is `wake_run_shell`'s.
/// Output is discarded: run-shell shows any stdout in view mode over the
/// active pane — a member's TUI — until someone presses q, and a nonzero
/// exit the same way; the hook must never do that to a member.
pub(crate) fn wake_shell_line(hive: &str, env: &[(String, String)]) -> String {
    let mut line = String::new();
    for (key, value) in env {
        line.push_str(key);
        line.push('=');
        line.push_str(&crate::shell::shlex_quote(value));
        line.push(' ');
    }
    line.push_str(&crate::shell::shlex_quote(hive));
    line.push_str(" wake --session #{q:session_id} >/dev/null 2>&1 || true");
    line
}

pub(crate) fn wake_run_shell(shell_line: &str) -> String {
    format!(
        "run-shell -b \"{}\"",
        crate::shell::tmux_dquote_escape(shell_line)
    )
}

/// The `HIVE_HOME=<home>` assignment a wake hook of *home* starts with —
/// the mark that tells this hive home's entries from every other.
fn wake_home_token(home: &str) -> String {
    format!("HIVE_HOME={} ", crate::shell::shlex_quote(home))
}

/// One entry of a session's hook array as `show-hooks` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HookEntry {
    pub hook: String,
    pub index: u32,
    /// The command as tmux prints it (`run-shell -b "…"`).
    pub command: String,
}

/// `show-hooks -t <session>` output → the entries of the wake hooks.
/// tmux prints each array item as `name[index] command`; an entry set
/// without an index sits at 0.
pub(crate) fn parse_hook_entries(listing: &str) -> Vec<HookEntry> {
    listing
        .lines()
        .filter_map(|line| {
            let (name, command) = line.split_once(' ')?;
            let (hook, index) = match name.split_once('[') {
                Some((hook, rest)) => (hook, rest.strip_suffix(']')?.parse::<u32>().ok()?),
                None => (name, 0),
            };
            WAKE_HOOKS.contains(&hook).then(|| HookEntry {
                hook: hook.to_string(),
                index,
                command: command.to_string(),
            })
        })
        .collect()
}

/// The shell line inside a listed `run-shell -b "…"` entry, with tmux's
/// printing escapes undone; None for any other shape of command. An older
/// tmux may print the whole command as one quoted string: that layer is
/// undone first.
pub(crate) fn run_shell_body(command: &str) -> Option<String> {
    let unwrap = |text: &str| -> Option<String> {
        let inner = text.strip_prefix('"')?.strip_suffix('"')?;
        Some(crate::shell::tmux_dquote_unescape(inner))
    };
    let whole = if command.starts_with('"') {
        unwrap(command)?
    } else {
        command.to_string()
    };
    let rest = whole.strip_prefix("run-shell -b ")?;
    unwrap(rest)
}

/// Whether a listed entry is this hive home's wake: one whose command
/// starts with this home's `HIVE_HOME` assignment. The home an entry
/// bakes is the only proof of whose it is — an older, home-less
/// `wake --window` entry names a binary, and a binary is shared by every
/// home installed from it, so such an entry is nobody's to claim, update
/// or remove.
pub(crate) fn owned_wake_entry(entry: &HookEntry, home: &str) -> bool {
    run_shell_body(&entry.command)
        .is_some_and(|body| body.starts_with(&wake_home_token(home)) && body.contains(" wake --"))
}

/// How long an installer or remover waits for the server's hook lock, and
/// how often it retries inside that budget. A holder is another hive
/// process mid-install on the same server, gone within milliseconds; a
/// budget rather than a blocking wait keeps a stuck one a loud failure.
const HOOK_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const HOOK_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// The socket the tmux client hive runs reaches, resolved the way that
/// client resolves it and without asking it: the one `TMUX` names inside
/// tmux, else the default server's under `TMUX_TMPDIR` (or `/tmp`).
fn reached_socket_path() -> PathBuf {
    if let Some(path) = std::env::var("TMUX")
        .ok()
        .and_then(|tmux| tmux.split(',').next().map(str::trim).map(str::to_owned))
        .filter(|path| !path.is_empty())
    {
        return PathBuf::from(path);
    }
    let tmpdir = std::env::var("TMUX_TMPDIR")
        .ok()
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| "/tmp".to_string());
    PathBuf::from(tmpdir)
        .join(format!("tmux-{}", unsafe { libc::getuid() }))
        .join("default")
}

/// Where the wake hook lock of the reached server lives: beside its
/// socket, named after it, so every hive home on that server finds the
/// same file and no home's own tree could hold it.
fn hook_lock_path() -> PathBuf {
    let socket = reached_socket_path();
    let name = socket
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    socket.with_file_name(format!("{name}.hive-hooks.lock"))
}

/// The lock every installer and remover of a wake hook holds while it
/// reads a session's hook arrays and writes its entry back: the arrays
/// are read-modify-write state shared by every hive home on the server,
/// and two homes that each read index N free would each write it, the
/// second wiping the first's wake. The lock is the server's, not any
/// home's (`hook_lock_path`), and the flock goes with the descriptor, so
/// a holder that dies holds nothing. Held for the whole of one install
/// or remove, released on drop.
struct HookArrayLock {
    file: std::fs::File,
}

impl HookArrayLock {
    /// The reached server's lock, within `HOOK_LOCK_TIMEOUT`; an error
    /// when the file cannot be opened or the lock is still held at the
    /// deadline. The socket directory is made the way tmux makes it (the
    /// user's, mode 0700) when no server has made it yet.
    fn acquire() -> anyhow::Result<HookArrayLock> {
        use std::os::unix::fs::DirBuilderExt;
        let path = hook_lock_path();
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|err| {
                    anyhow::anyhow!(
                        "cannot make the tmux socket directory {}: {err}",
                        dir.display()
                    )
                })?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|err| {
                anyhow::anyhow!("cannot open the wake hook lock {}: {err}", path.display())
            })?;
        let deadline = Instant::now() + HOOK_LOCK_TIMEOUT;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(HookArrayLock { file });
            }
            let err = std::io::Error::last_os_error();
            // EINTR retries on the same deadline; a busy lock is the only
            // other reason to keep trying.
            if !matches!(
                err.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) {
                anyhow::bail!("cannot lock the wake hook lock {}: {err}", path.display());
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "the wake hook lock {} is still held after {}s",
                    path.display(),
                    HOOK_LOCK_TIMEOUT.as_secs()
                );
            }
            thread::sleep(HOOK_LOCK_RETRY_INTERVAL);
        }
    }
}

impl Drop for HookArrayLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// The wake hook entries on *session_id*, or the error when tmux does not
/// answer for it.
fn wake_hook_entries(session_id: &str) -> anyhow::Result<Vec<HookEntry>> {
    let listed = run(&["show-hooks", "-t", session_id], true, 5)?;
    Ok(parse_hook_entries(&listed.stdout))
}

/// Install this hive home's wake on *session_id* — the session the team's
/// display sits in, hive's own or one the human lent — one indexed entry
/// per hook: its own entry is updated in place, a first install takes the
/// lowest free index, and nothing else in the array is written. The read
/// of the arrays and the writes back are one critical section under the
/// server's hook lock, so another home's installer sees this entry before
/// it picks an index. Every tmux command is checked: a hook that failed to
/// install is reported, because a desk retiring unwatched behind it could
/// not be woken.
pub fn install_wake_hooks(session_id: &str) -> anyhow::Result<()> {
    if session_id.is_empty() {
        anyhow::bail!("no session to install the wake hooks on");
    }
    let hive = crate::paths::self_exe();
    let home = wake_hive_home();
    let command = wake_run_shell(&wake_shell_line(&hive, &wake_environment()));
    let _lock = HookArrayLock::acquire()?;
    let entries = wake_hook_entries(session_id)?;
    for hook in WAKE_HOOKS {
        let of_hook: Vec<&HookEntry> = entries.iter().filter(|e| e.hook == hook).collect();
        let mut own = of_hook
            .iter()
            .filter(|e| owned_wake_entry(e, &home))
            .map(|e| e.index);
        let index = match own.next() {
            Some(index) => index,
            None => (0..)
                .find(|i| of_hook.iter().all(|e| e.index != *i))
                .unwrap_or(0),
        };
        // A second entry of this home's is an older install's leftover.
        for extra in own {
            run(
                &[
                    "set-hook",
                    "-u",
                    "-t",
                    session_id,
                    &format!("{hook}[{extra}]"),
                ],
                true,
                5,
            )?;
        }
        run(
            &[
                "set-hook",
                "-t",
                session_id,
                &format!("{hook}[{index}]"),
                &command,
            ],
            true,
            5,
        )?;
    }
    Ok(())
}

/// Remove this hive home's wake entries from *session_id*, and only those:
/// what `hive delete` and a display that moved on do once no team of this
/// home shows in the session. The entries are read and unset under the
/// server's hook lock, so an index is never unset from a listing another
/// home has since written to. A session that is gone has nothing to
/// remove.
pub fn remove_wake_hooks(session_id: &str) -> anyhow::Result<()> {
    if session_id.is_empty() {
        return Ok(());
    }
    let home = wake_hive_home();
    let _lock = HookArrayLock::acquire()?;
    let Ok(entries) = wake_hook_entries(session_id) else {
        return Ok(());
    };
    for entry in entries.iter().filter(|e| owned_wake_entry(e, &home)) {
        run(
            &[
                "set-hook",
                "-u",
                "-t",
                session_id,
                &format!("{}[{}]", entry.hook, entry.index),
            ],
            true,
            5,
        )?;
    }
    Ok(())
}

pub fn install_team_status(session_id: &str) {
    for row in install_status_rows(session_id) {
        let args: Vec<&str> = row.iter().map(String::as_str).collect();
        let _ = run(&args, false, 5);
    }
    if let Err(err) = install_wake_hooks(session_id) {
        eprintln!("warning: wake hooks on session {session_id}: {err}");
    }
}

pub(crate) fn install_team_status_checked(session_id: &str) -> anyhow::Result<()> {
    for row in install_status_rows(session_id) {
        let args: Vec<&str> = row.iter().map(String::as_str).collect();
        run(&args, true, 5)?;
    }
    install_wake_hooks(session_id)
}
