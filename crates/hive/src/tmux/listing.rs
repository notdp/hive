use super::run::run;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaneInfo {
    pub pane_id: String,
    pub title: String,
    pub command: String,
    pub role: String,
    pub agent: String,
    pub team: String,
    pub cli: String,
    pub group: String,
}

impl PaneInfo {
    /// A pane standing for a member: hosting its engine (`agent`) or
    /// mirroring it read-only (`mirror`).
    pub fn is_member_pane(&self) -> bool {
        self.role == "agent" || self.role == "mirror"
    }
}

// Adding a column means touching three places: the format string, its field
// count const, and the positional `p[n]` reads in the parser below.
pub const PANE_BASE_FMT: &str = concat!(
    "#{pane_id}\t#{pane_title}\t#{pane_current_command}\t#{@hive-role}\t",
    "#{@hive-agent}\t#{@hive-team}\t#{@hive-cli}\t#{@hive-group}"
);
const PANE_FIELD_COUNT: usize = 8;

/// `#{@hive-team}` read as a window tag. A window format reads pane options
/// through (the active pane's `@hive-team` answers for a window that has
/// none), so the hidden window parking a mirror pane would scan as a second
/// team window; `@hive-hidden` masks it.
pub const WINDOW_TEAM_FMT: &str = "#{?@hive-hidden,,#{@hive-team}}";

pub const TEAM_WINDOW_FMT: &str = concat!(
    "#{session_name}:#{window_index}\t#{window_name}\t#{window_id}\t",
    "#{?@hive-hidden,,#{@hive-team}}\t#{@hive-workspace}\t#{@hive-created}\t#{@hive-pr}"
);
const TEAM_WINDOW_FIELD_COUNT: usize = 7;

/// Entry of `list_team_windows_status`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TeamWindow {
    pub window: String,
    pub window_name: String,
    pub window_id: String,
    pub team: String,
    pub workspace: String,
    pub created: String,
    pub pr: String,
}

fn split_fields(line: &str, count: usize) -> Vec<String> {
    let mut parts: Vec<String> = line.split('\t').map(str::to_string).collect();
    while parts.len() < count {
        parts.push(String::new());
    }
    parts.truncate(count);
    parts
}

/// List all panes with command and hive identity (@hive-*).
pub fn list_panes_full(target: &str) -> Vec<PaneInfo> {
    list_panes_full_or_none(target).unwrap_or_default()
}

/// Status-aware `list_panes_full`: None when tmux did not answer.
///
/// A successful-but-empty listing is a real empty window; None means the
/// caller cannot tell missing panes from a transient tmux failure and must
/// not take irreversible action on it (same contract as `is_pane_alive`).
pub fn list_panes_full_or_none(target: &str) -> Option<Vec<PaneInfo>> {
    let r = run(&["list-panes", "-t", target, "-F", PANE_BASE_FMT], false, 5).ok()?;
    if r.returncode != 0 {
        return None;
    }
    Some(parse_panes_full(&r.stdout))
}

/// List every pane across all sessions/windows with hive identity tags.
pub fn list_panes_all() -> Vec<PaneInfo> {
    list_panes_all_status().0.unwrap_or_default()
}

/// True only when tmux stderr proves there is no server to talk to.
///
/// Proven messages: "no server running on <path>" (clean shutdown) and
/// "error connecting to <path> (No such file or directory)" (socket gone).
/// Anything else — permission denied, connection refused, unexpected text —
/// stays unknown: a server may well be alive behind the failure.
fn stderr_means_no_server(stderr: &str) -> bool {
    let low = stderr.to_lowercase();
    if low.contains("no server running") {
        return true;
    }
    low.contains("error connecting") && low.contains("no such file or directory")
}

/// Status-aware `list_panes_all`: `(panes, "ok")` on success.
///
/// `(None, "no-server")` when no tmux server is running (nothing can be
/// live), `(None, "unknown")` on any other failure — callers must not
/// read unknown as "dead" (same contract as `is_pane_alive`).
pub fn list_panes_all_status() -> (Option<Vec<PaneInfo>>, &'static str) {
    let r = match run(&["list-panes", "-a", "-F", PANE_BASE_FMT], false, 5) {
        Ok(r) => r,
        Err(_) => return (None, "unknown"),
    };
    if r.returncode == 0 {
        return (Some(parse_panes_full(&r.stdout)), "ok");
    }
    if stderr_means_no_server(&r.stderr) {
        return (None, "no-server");
    }
    (None, "unknown")
}

/// Status-aware scan of windows carrying `@hive-team`.
///
/// Same (value, status) contract as `list_panes_all_status`. Each
/// entry: window target/name/id plus the team, workspace, and created
/// options — everything `hive ls` needs to match a live
/// team instance against a snapshot.
pub fn list_team_windows_status() -> (Option<Vec<TeamWindow>>, &'static str) {
    let r = match run(&["list-windows", "-a", "-F", TEAM_WINDOW_FMT], false, 5) {
        Ok(r) => r,
        Err(_) => return (None, "unknown"),
    };
    if r.returncode != 0 {
        if stderr_means_no_server(&r.stderr) {
            return (None, "no-server");
        }
        return (None, "unknown");
    }
    let mut out: Vec<TeamWindow> = Vec::new();
    for line in r.stdout.trim().split('\n') {
        if line.is_empty() {
            continue;
        }
        let p = split_fields(line, TEAM_WINDOW_FIELD_COUNT);
        if p[3].is_empty() {
            continue;
        }
        out.push(TeamWindow {
            window: p[0].clone(),
            window_name: p[1].clone(),
            window_id: p[2].clone(),
            team: p[3].clone(),
            workspace: p[4].clone(),
            created: p[5].clone(),
            pr: p[6].clone(),
        });
    }
    (Some(out), "ok")
}

/// The window tagged `@hive-team` *team* (hidden windows masked), as a
/// `session:index` target.
pub fn team_window_target(team: &str) -> Option<String> {
    list_team_windows_status()
        .0?
        .into_iter()
        .find(|w| w.team == team)
        .map(|w| w.window)
}

fn parse_panes_full(stdout: &str) -> Vec<PaneInfo> {
    let mut result: Vec<PaneInfo> = Vec::new();
    for line in stdout.trim().split('\n') {
        if line.is_empty() {
            continue;
        }
        let p = split_fields(line, PANE_FIELD_COUNT);
        result.push(PaneInfo {
            pane_id: p[0].clone(),
            title: p[1].clone(),
            command: p[2].clone(),
            role: p[3].clone(),
            agent: p[4].clone(),
            team: p[5].clone(),
            cli: p[6].clone(),
            group: p[7].clone(),
        });
    }
    result
}

// --- Per-pane user options (@hive-*) ---

pub fn set_pane_option(pane_id: &str, key: &str, value: &str) {
    let opt = format!("@{key}");
    let _ = run(&["set-option", "-p", "-t", pane_id, &opt, value], false, 5);
}

pub fn get_pane_option(pane_id: &str, key: &str) -> Option<String> {
    let opt = format!("@{key}");
    let r = run(&["show-options", "-p", "-v", "-t", pane_id, &opt], false, 5).ok()?;
    if r.returncode != 0 {
        return None;
    }
    let val = r.stdout.trim().to_string();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

pub fn clear_pane_option(pane_id: &str, key: &str) {
    let opt = format!("@{key}");
    let _ = run(&["set-option", "-p", "-t", pane_id, "-u", &opt], false, 5);
}

// `hive-view`, `hive-busy` and `hive-unread` are derived state (the claude
// view probe and the status tick write them), not identity — but release
// must clear them with the rest, or a reused pane keeps rendering a border
// suffix or a status chip nobody owns any more.
const PANE_TAG_KEYS: [&str; 9] = [
    "hive-role",
    "hive-agent",
    "hive-team",
    "hive-cli",
    "hive-group",
    "hive-owner",
    "hive-view",
    "hive-busy",
    "hive-unread",
];

/// Set all hive identity options on a pane.
pub fn tag_pane(pane_id: &str, role: &str, agent: &str, team: &str, cli: &str, group: &str) {
    set_pane_option(pane_id, "hive-role", role);
    set_pane_option(pane_id, "hive-agent", agent);
    set_pane_option(pane_id, "hive-team", team);
    if !cli.is_empty() {
        set_pane_option(pane_id, "hive-cli", cli);
        if cli != "claude" {
            // Only the claude view tick maintains `hive-view`, and it skips
            // non-claude panes — so a pane retagged onto another CLI in place
            // would keep its last ' -> <session>' suffix forever.
            clear_pane_option(pane_id, "hive-view");
        }
    }
    if !group.is_empty() {
        set_pane_option(pane_id, "hive-group", group);
    }
}

/// Remove all hive identity options from a pane.
pub fn clear_pane_tags(pane_id: &str) {
    for key in PANE_TAG_KEYS {
        clear_pane_option(pane_id, key);
    }
}

/// Window ids of the hidden windows parking *team*'s closed mirror pane
/// (`@hive-hidden <team>`, written by `hive mirror off`).
pub fn hidden_mirror_windows(team: &str) -> Vec<String> {
    let fmt = format!("#{{window_id}}\t#{{@{}}}", super::HIDDEN_WINDOW_KEY);
    let r = match run(&["list-windows", "-a", "-F", &fmt], false, 5) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    r.stdout
        .lines()
        .filter_map(|line| {
            let (window, hidden) = line.split_once('\t')?;
            (hidden == team && !window.is_empty()).then(|| window.to_string())
        })
        .collect()
}

/// The parked mirror pane of *team*: the first `mirror` pane in one of its
/// hidden windows. A second one is a leftover for `hive delete`.
pub fn hidden_mirror_pane(team: &str) -> Option<String> {
    let windows = hidden_mirror_windows(team);
    if windows.is_empty() {
        return None;
    }
    let r = run(
        &[
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{window_id}\t#{@hive-role}",
        ],
        false,
        5,
    )
    .ok()?;
    r.stdout.lines().find_map(|line| {
        let mut parts = line.split('\t');
        let pane = parts.next()?;
        let window = parts.next()?;
        let role = parts.next().unwrap_or_default();
        (role == "mirror" && windows.iter().any(|w| w == window)).then(|| pane.to_string())
    })
}
