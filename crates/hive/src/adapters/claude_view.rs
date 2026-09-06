//! Which bg session a claude member's pane is actually showing.
//!
//! A claude member pane is an attach *viewer*: the human can press the panel
//! key inside it, land in claude's own agent panel, and open any other bg
//! session there. The pane keeps its member identity — tags, job record,
//! delivery address — while the screen shows something else.
//!
//! This is display truth only. Nothing here routes a delivery; it answers
//! what the border should label, and — via `interactive_claude_pid` — which
//! panes are *not* viewers, the only ones tmux keys may be sent to.
//!
//! Three signals, in the order they are trusted (2.1.240, real-machine
//! verified): the attach journal (whether a session is displayed, never
//! which), viewer argv (`claude attach <jobId>` names the job outright,
//! absent after the re-exec to `claude agents`), and `#{pane_title}` (the
//! only carrier of *which* in panel mode; it latches after the viewer dies,
//! so it is read only behind the journal and argv gates).

use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::adapters::base::read_json_object;
use crate::adapters::claude_sessions::{config_dir, pid_alive};

const VIEWER_SUBCOMMANDS: [&str; 2] = ["attach", "agents"];
const PROC_START_FORMAT: &str = "%a %b %d %H:%M:%S %Y";
const PROC_START_TOLERANCE: f64 = 2.0; // seconds; the two clocks are the same clock
const LABEL_MAX: usize = 28; // a border suffix, not a log line

/// What *this* pane's viewer is showing.
///
/// `certainty` is `certain` (process or journal evidence), `likely` (the
/// pane title named it) or `unknown`. `kind` is `member_view` (a hive
/// member's job — `job_id`/`member` name it), `foreign` (some other
/// session), `list_view` (the panel's list, nothing displayed) or
/// `no_viewer`. `job_id`/`member` are empty when unresolved; `title`
/// carries the displayed session's own name when that is all there is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaneView {
    pub certainty: String,
    pub kind: String,
    pub job_id: String,
    pub member: String,
    pub title: String,
    pub why: String,
}

// --------------------------------------------------------------------------
// tmux inputs (thin seams so the probe's unit tests can drive them through
// the testhook without a tmux server)
// --------------------------------------------------------------------------
fn pane_tty(pane_id: &str) -> Option<String> {
    #[cfg(test)]
    {
        if let Some(v) = testhook::with(|_| Some("/dev/ttys012".to_string())) {
            return v;
        }
    }
    crate::tmux::get_pane_tty(pane_id)
}

fn tty_processes(tty: &str) -> Vec<crate::tmux::TTYProcessInfo> {
    #[cfg(test)]
    {
        if let Some(v) = testhook::with(|p| {
            if p.argv.is_empty() {
                vec![crate::tmux::TTYProcessInfo {
                    pid: "99".to_string(),
                    command: "-zsh".to_string(),
                    argv: "-zsh".to_string(),
                }]
            } else {
                vec![crate::tmux::TTYProcessInfo {
                    pid: p.viewer_pid.to_string(),
                    command: "2.1.240".to_string(),
                    argv: p.argv.clone(),
                }]
            }
        }) {
            return v;
        }
    }
    crate::tmux::list_tty_processes(tty)
}

fn pane_title(pane_id: &str) -> Option<String> {
    #[cfg(test)]
    {
        if let Some(v) = testhook::with(|p| Some(p.title.clone())) {
            return v;
        }
    }
    crate::tmux::get_pane_title(pane_id)
}

fn panes_all() -> Vec<crate::tmux::PaneInfo> {
    #[cfg(test)]
    {
        if let Some(v) = testhook::with(|p| p.panes.clone()) {
            return v;
        }
    }
    crate::tmux::list_panes_all()
}

// --------------------------------------------------------------------------
// attach journal
// --------------------------------------------------------------------------
pub fn journal_dir() -> PathBuf {
    config_dir().join("daemon").join("attach-journal")
}

/// Cheap change token for the journal: the entry names.
///
/// Every attach, switch and detach adds or removes a file, so an unchanged
/// list means no viewer changed what it displays. A missing directory (older
/// claude, other config tree) signs as empty — callers then only ever see
/// `list_view`, i.e. the border simply carries no view label.
pub fn journal_signature() -> Vec<String> {
    let Ok(entries) = fs::read_dir(journal_dir()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.ends_with(".json").then_some(name)
        })
        .collect();
    names.sort();
    names
}

/// Parse a `%a %b %d %H:%M:%S %Y` timestamp with libc `strptime`: the whole
/// string must match, and the month/day names are the C locale's.
fn strptime_lstart(text: &str) -> Option<libc::tm> {
    let c_text = CString::new(text).ok()?;
    let c_fmt = CString::new(PROC_START_FORMAT).ok()?;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let end = unsafe { libc::strptime(c_text.as_ptr(), c_fmt.as_ptr(), &mut tm) };
    if end.is_null() || unsafe { *end } != 0 {
        return None;
    }
    tm.tm_isdst = -1; // let mktime decide DST
    Some(tm)
}

fn mktime_local(tm: libc::tm) -> f64 {
    let mut tm = tm;
    unsafe { libc::mktime(&mut tm) as f64 }
}

fn timegm_utc(tm: libc::tm) -> f64 {
    let mut tm = tm;
    unsafe { libc::timegm(&mut tm) as f64 }
}

/// Process start time of *pid* in epoch seconds, or None.
fn pid_start_epoch(pid: i32) -> Option<f64> {
    // ponytail: no 5s subprocess timeout — `ps -p` answers promptly; add a
    // spawn+poll harness only if it ever wedges.
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        return None;
    }
    Some(mktime_local(strptime_lstart(&text)?))
}

/// True when *pid* really started when the journal entry says it did.
fn start_matches(claimed: &str, pid: i32) -> bool {
    // ponytail: the journal renders procStart in UTC (verified on 2.1.240)
    // while ps prints local time; both readings are accepted rather than
    // pinning a timezone the daemon never documented.
    let text = claimed.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return false;
    }
    let Some(parsed) = strptime_lstart(&text) else {
        return false;
    };
    let Some(actual) = pid_start_epoch(pid) else {
        return false;
    };
    [timegm_utc(parsed), mktime_local(parsed)]
        .iter()
        .any(|candidate| (actual - candidate).abs() <= PROC_START_TOLERANCE)
}

/// The live attach entry naming *pid* — i.e. that viewer has a session on
/// screen right now — or None.
///
/// Dead viewers leave their entries behind, so an entry only counts when its
/// pid is alive *and* started when the entry recorded it (a recycled pid
/// must never read as an open session).
pub fn attach_entry_for_pid(pid: i32) -> Option<Map<String, Value>> {
    let entries = fs::read_dir(journal_dir()).ok()?;
    let paths: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
        .map(|e| e.path())
        .collect();
    for path in paths {
        let Some(data) = read_json_object(&path) else {
            continue;
        };
        if data.get("pid").and_then(Value::as_i64) != Some(pid as i64) {
            continue;
        }
        if !pid_alive(pid) {
            continue;
        }
        let claimed = data.get("procStart").and_then(Value::as_str).unwrap_or("");
        if !start_matches(claimed, pid) {
            continue;
        }
        return Some(data);
    }
    None
}

// --------------------------------------------------------------------------
// viewer process + hive member index
// --------------------------------------------------------------------------
/// argv[0] is the resolved binary path: `~/.local/bin/claude` normally, but
/// the install is a version-named symlink tree, so a bare version basename
/// counts too (`^\d+(\.\d+)+$`).
fn version_basename(base: &str) -> bool {
    let parts: Vec<&str> = base.split('.').collect();
    parts.len() >= 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Minimal POSIX shlex: whitespace-separated words with quote and backslash
/// handling; None on an unterminated quote/escape (the caller then falls
/// back to a plain whitespace split).
fn shlex_split(s: &str) -> Option<Vec<String>> {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            Some(_) => {
                // inside double quotes
                if c == '"' {
                    quote = None;
                } else if c == '\\' {
                    match chars.next() {
                        Some(n) if n == '"' || n == '\\' || n == '$' || n == '`' => cur.push(n),
                        Some(n) => {
                            cur.push('\\');
                            cur.push(n);
                        }
                        None => return None,
                    }
                } else {
                    cur.push(c);
                }
            }
            None => {
                if c.is_whitespace() {
                    if in_word {
                        parts.push(std::mem::take(&mut cur));
                        in_word = false;
                    }
                } else if c == '\'' || c == '"' {
                    quote = Some(c);
                    in_word = true;
                } else if c == '\\' {
                    match chars.next() {
                        Some(n) => {
                            cur.push(n);
                            in_word = true;
                        }
                        None => return None,
                    }
                } else {
                    cur.push(c);
                    in_word = true;
                }
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_word {
        parts.push(cur);
    }
    Some(parts)
}

/// `(subcommand, first argument)` for a claude viewer argv, else None.
///
/// Hidden subcommands are only recognized at argv[1], which is also what
/// makes this safe: an argv that merely mentions "claude attach" (a grep, an
/// editor) never matches.
fn viewer_argv(argv: &str) -> Option<(String, String)> {
    let parts =
        shlex_split(argv).unwrap_or_else(|| argv.split_whitespace().map(str::to_string).collect());
    if parts.len() < 2 {
        return None;
    }
    let base = Path::new(&parts[0])
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if base != "claude" && !version_basename(&base) {
        return None;
    }
    if !VIEWER_SUBCOMMANDS.contains(&parts[1].as_str()) {
        return None;
    }
    Some((parts[1].clone(), parts.get(2).cloned().unwrap_or_default()))
}

/// `(pid, subcommand, argument)` of the claude viewer on *pane_id*'s tty, or
/// None when no viewer is running there.
///
/// The engine (argv `claude bg-spare`) lives on claude's own supervisor,
/// never on a pane tty, so it can never match here.
pub fn viewer_for_pane(pane_id: &str) -> Option<(i32, String, String)> {
    let tty = pane_tty(pane_id).unwrap_or_default();
    for process in tty_processes(&tty) {
        let Some((subcommand, argument)) = viewer_argv(&process.argv) else {
            continue;
        };
        return process
            .pid
            .trim()
            .parse::<i32>()
            .ok()
            .map(|pid| (pid, subcommand, argument));
    }
    None
}

/// Pid of a *plain interactive* claude TUI on *pane_id*'s tty, or None.
///
/// The only shape tmux keystrokes may be typed into. An attach viewer is a
/// claude process on the tty too, but its keyboard belongs to whichever
/// session it currently displays — another member's, or one the human opened
/// from the panel — so keys sent there land in a stranger's composer. Hive
/// members never reach this: their keystrokes are addressed to the job.
pub fn interactive_claude_pid(pane_id: &str) -> Option<i32> {
    let pid = crate::agent_cli::claude_pid_for_pane(pane_id)?;
    if viewer_for_pane(pane_id).is_some() {
        return None;
    }
    Some(pid)
}

/// `("<team>.<member>", jobId)` for every claude member pane on the server —
/// the job *name* a member's engine registers under, which is what the panel
/// writes into the pane title.
///
/// The name is rebuilt from the pane tags rather than read from the ledger.
/// That holds because `hive claude` mints a member's job under
/// `<team>.<member>`; a pane rebound with `--resume` to a job minted under
/// some other name is only unresolvable *by title* (the argv branch matches
/// by job id), which costs a border label, nothing more.
pub fn member_job_index(panes: Option<&[crate::tmux::PaneInfo]>) -> Vec<(String, String)> {
    let owned;
    let rows: &[crate::tmux::PaneInfo] = match panes {
        Some(rows) => rows,
        None => {
            owned = panes_all();
            &owned
        }
    };
    let mut index: Vec<(String, String)> = Vec::new();
    for pane in rows {
        if pane.cli != "claude" || pane.agent.is_empty() || pane.team.is_empty() {
            continue;
        }
        let Some(job_id) = crate::adapters::claude_bg::job_id_for_pane(&pane.pane_id) else {
            continue;
        };
        if job_id.is_empty() {
            continue;
        }
        let key = format!("{}.{}", pane.team, pane.agent);
        if let Some(slot) = index.iter_mut().find(|(name, _)| *name == key) {
            slot.1 = job_id;
        } else {
            index.push((key, job_id));
        }
    }
    index
}

/// True when *title* carries *name* as a whole token.
///
/// The panel writes the bare session name and may decorate it (spinner,
/// counters, and tmux flattens every non-ASCII byte to '_'), so equality is
/// too strict — but containment is too loose: a foreign session named
/// `probe.red-notes` would then read as member `probe.red` and keystrokes
/// would land in the wrong session. A name character on either side (`\w`,
/// `.` or `-`) means the title names something else, which also keeps prefix
/// siblings (`probe.red` vs `probe.red2`) apart.
fn title_names(title: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let is_name_char = |c: char| c.is_alphanumeric() || c == '_' || c == '.' || c == '-';
    let mut start = 0;
    while let Some(pos) = title[start..].find(name) {
        let at = start + pos;
        let before_ok = title[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !is_name_char(c));
        let after_ok = title[at + name.len()..]
            .chars()
            .next()
            .is_none_or(|c| !is_name_char(c));
        if before_ok && after_ok {
            return true;
        }
        start = at + title[at..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// A quoted repr for the printable strings that reach a `why` line: quote
/// choice, backslash/quote escapes; exotic controls are left as-is.
fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' if quote == '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// What *pane_id* is displaying right now.
///
/// Pass *panes* (a `tmux::list_panes_all()` result) when the caller already
/// has one; the member index is built from it.
pub fn view_for_pane(pane_id: &str, panes: Option<&[crate::tmux::PaneInfo]>) -> PaneView {
    let Some((pid, subcommand, argument)) = viewer_for_pane(pane_id) else {
        // The title is a latched leftover of whatever the dead viewer showed
        // last — never evidence that anything is on screen.
        return PaneView {
            certainty: "certain".to_string(),
            kind: "no_viewer".to_string(),
            why: "no claude viewer on the pane tty".to_string(),
            ..Default::default()
        };
    };
    if attach_entry_for_pid(pid).is_none() {
        return PaneView {
            certainty: "certain".to_string(),
            kind: "list_view".to_string(),
            why: format!("viewer {pid} has no live attach entry"),
            ..Default::default()
        };
    }

    let index = member_job_index(panes);
    if subcommand == "attach" && crate::adapters::claude_bg::looks_like_job_id(&argument) {
        let member = index
            .iter()
            .find(|(_, job)| *job == argument)
            .map(|(name, _)| name.clone())
            .unwrap_or_default();
        return PaneView {
            certainty: "certain".to_string(),
            kind: if member.is_empty() {
                "foreign"
            } else {
                "member_view"
            }
            .to_string(),
            job_id: argument,
            member,
            why: format!("viewer {pid} argv still names the job"),
            ..Default::default()
        };
    }

    let title = pane_title(pane_id).unwrap_or_default().trim().to_string();
    if title.is_empty() {
        return PaneView {
            certainty: "unknown".to_string(),
            kind: "foreign".to_string(),
            why: format!("viewer {pid} has a session open, title empty"),
            ..Default::default()
        };
    }
    let matches: Vec<&(String, String)> = index
        .iter()
        .filter(|(name, _)| title_names(&title, name))
        .collect();
    if matches.len() == 1 {
        let (name, job) = matches[0];
        return PaneView {
            certainty: "likely".to_string(),
            kind: "member_view".to_string(),
            job_id: job.clone(),
            member: name.clone(),
            title: title.clone(),
            why: format!("title {} names the member", py_repr(&title)),
        };
    }
    if matches.is_empty() {
        return PaneView {
            certainty: "likely".to_string(),
            kind: "foreign".to_string(),
            title: title.clone(),
            why: format!("title {} is no hive member", py_repr(&title)),
            ..Default::default()
        };
    }
    PaneView {
        certainty: "unknown".to_string(),
        kind: "foreign".to_string(),
        title: title.clone(),
        why: format!(
            "title {} matched {} members",
            py_repr(&title),
            matches.len()
        ),
        ..Default::default()
    }
}

/// Border suffix for *view*: what to show after the member's own name.
///
/// Empty means "nothing to add" — the pane shows its own member, the panel
/// list, or nothing identifiable. `#` is stripped so a session name can
/// never inject a tmux format into the border.
pub fn view_label(view: &PaneView, own_job_id: &str) -> String {
    if !view.job_id.is_empty() && view.job_id == own_job_id {
        return String::new();
    }
    let label = if view.kind == "member_view" && !view.member.is_empty() {
        view.member.clone()
    } else if view.kind == "foreign" && view.certainty == "likely" {
        view.title.clone()
    } else {
        return String::new();
    };
    label
        .replace('#', "")
        .trim()
        .chars()
        .take(LABEL_MAX)
        .collect()
}

#[cfg(test)]
mod testhook {
    //! The probe's tmux inputs: viewer argv, pane title, member panes. The
    //! viewer pid defaults to this test process so journal liveness and
    //! start-time checks run against a real process.
    use std::cell::RefCell;

    pub struct Probe {
        pub argv: String,
        pub title: String,
        pub viewer_pid: i32,
        pub panes: Vec<crate::tmux::PaneInfo>,
    }

    thread_local! {
        pub static PROBE: RefCell<Option<Probe>> = const { RefCell::new(None) };
    }

    pub fn with<T>(f: impl FnOnce(&Probe) -> T) -> Option<T> {
        PROBE.with(|p| p.borrow().as_ref().map(f))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;
    use serde_json::json;

    const PANE: &str = "%7";
    const JOB: &str = "cafe1234";
    const OTHER_JOB: &str = "beef5678";
    const DEAD_PID: i32 = 4242424; // out of range on macOS/Linux: never a live process

    fn me() -> i32 {
        std::process::id() as i32
    }

    fn pane(pane_id: &str, agent: &str, team: &str) -> crate::tmux::PaneInfo {
        crate::tmux::PaneInfo {
            pane_id: pane_id.to_string(),
            title: String::new(),
            command: "2.1.240".to_string(),
            role: "agent".to_string(),
            agent: agent.to_string(),
            team: team.to_string(),
            cli: "claude".to_string(),
            group: String::new(),
        }
    }

    /// An isolated claude config tree: pane job records and attach journal.
    struct Home {
        dir: tempfile::TempDir,
        _env: EnvGuard,
    }

    fn claude_home() -> Home {
        let mut env_guard = EnvGuard::cleared(&crate::testenv::CLAUDE_VARS);
        let dir = tempfile::tempdir().unwrap();
        env_guard.set("CLAUDE_HOME", dir.path());
        fs::create_dir_all(dir.path().join("daemon").join("attach-journal")).unwrap();
        let _ = crate::adapters::claude_bg::write_pane_job(PANE, JOB, "session-1", "/tmp");
        Home {
            dir,
            _env: env_guard,
        }
    }

    /// A journal entry renders the start time in UTC; ps prints local time.
    fn proc_start_utc(pid: i32) -> String {
        let out = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "lstart="])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let epoch = mktime_local(strptime_lstart(&text).unwrap()) as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::gmtime_r(&epoch, &mut tm) };
        let fmt = CString::new(PROC_START_FORMAT).unwrap();
        let mut buf = [0u8; 64];
        let n = unsafe {
            libc::strftime(
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                fmt.as_ptr(),
                &tm,
            )
        };
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    fn journal(home: &Home, pid: i32, proc_start: &str, name: &str) {
        let proc_start = if proc_start.is_empty() {
            proc_start_utc(pid)
        } else {
            proc_start.to_string()
        };
        fs::write(
            home.dir
                .path()
                .join("daemon")
                .join("attach-journal")
                .join(format!("{name}.json")),
            json!({
                "gestureId": name,
                "surface": "bg_cli",
                "startedAtEpochMs": 1787651900942u64,
                "pid": pid,
                "procStart": proc_start,
            })
            .to_string(),
        )
        .unwrap();
    }

    struct ProbeGuard;

    impl Drop for ProbeGuard {
        fn drop(&mut self) {
            testhook::PROBE.with(|p| *p.borrow_mut() = None);
        }
    }

    fn probe() -> ProbeGuard {
        testhook::PROBE.with(|p| {
            *p.borrow_mut() = Some(testhook::Probe {
                argv: String::new(),
                title: String::new(),
                viewer_pid: me(),
                panes: vec![pane(PANE, "red", "probe")],
            })
        });
        ProbeGuard
    }

    fn set_probe(f: impl FnOnce(&mut testhook::Probe)) {
        testhook::PROBE.with(|p| f(p.borrow_mut().as_mut().unwrap()));
    }

    // --- the states a member pane can be in --------------------------------

    #[test]
    fn test_no_viewer_discards_the_latched_title() {
        // The title latches after the viewer dies: it is not evidence of a
        // view.
        let home = claude_home();
        let _probe = probe();
        set_probe(|p| p.title = "probe.red".to_string());
        journal(&home, me(), "", "gesture");

        let view = view_for_pane(PANE, None);

        assert_eq!(
            (
                view.certainty.as_str(),
                view.kind.as_str(),
                view.job_id.as_str()
            ),
            ("certain", "no_viewer", "")
        );
    }

    #[test]
    fn test_attach_argv_names_the_job() {
        let home = claude_home();
        let _probe = probe();
        set_probe(|p| {
            p.argv = "claude attach cafe1234".to_string();
            p.title = "stale nonsense".to_string();
        });
        journal(&home, me(), "", "gesture");

        let view = view_for_pane(PANE, None);

        assert_eq!(
            (view.certainty.as_str(), view.kind.as_str()),
            ("certain", "member_view")
        );
        assert_eq!(
            (view.job_id.as_str(), view.member.as_str()),
            (JOB, "probe.red")
        );
    }

    #[test]
    fn test_attach_argv_of_a_job_hive_does_not_own_is_foreign() {
        let home = claude_home();
        let _probe = probe();
        set_probe(|p| p.argv = "claude attach beef5678".to_string());
        journal(&home, me(), "", "gesture");

        let view = view_for_pane(PANE, None);

        assert_eq!(
            (
                view.certainty.as_str(),
                view.kind.as_str(),
                view.job_id.as_str()
            ),
            ("certain", "foreign", OTHER_JOB)
        );
    }

    #[test]
    fn test_panel_without_a_journal_entry_is_the_list_view() {
        // Back on the panel list: the entry is gone, the title still names
        // the session that was open a moment ago.
        let _home = claude_home();
        let _probe = probe();
        set_probe(|p| {
            p.argv = "claude agents".to_string();
            p.title = "probe.red".to_string();
        });

        let view = view_for_pane(PANE, None);

        assert_eq!(
            (
                view.certainty.as_str(),
                view.kind.as_str(),
                view.job_id.as_str()
            ),
            ("certain", "list_view", "")
        );
    }

    #[test]
    fn test_panel_with_an_entry_resolves_the_member_from_the_title() {
        let home = claude_home();
        let _probe = probe();
        set_probe(|p| {
            p.argv = "claude agents".to_string();
            p.title = "probe.red".to_string();
        });
        journal(&home, me(), "", "gesture");

        let view = view_for_pane(PANE, None);

        assert_eq!(
            (view.certainty.as_str(), view.kind.as_str()),
            ("likely", "member_view")
        );
        assert_eq!(
            (view.job_id.as_str(), view.member.as_str()),
            (JOB, "probe.red")
        );
    }

    #[test]
    fn test_panel_title_may_be_decorated_by_the_tui() {
        // tmux flattens every non-ASCII byte in a title to '_'.
        let home = claude_home();
        let _probe = probe();
        set_probe(|p| {
            p.argv = "claude agents".to_string();
            p.title = "_ probe.red _ 3 messages".to_string();
        });
        journal(&home, me(), "", "gesture");

        let view = view_for_pane(PANE, None);

        assert_eq!(
            (view.kind.as_str(), view.member.as_str()),
            ("member_view", "probe.red")
        );
    }

    #[test]
    fn test_a_session_that_is_no_hive_member_is_foreign() {
        let home = claude_home();
        let _probe = probe();
        set_probe(|p| {
            p.argv = "claude agents".to_string();
            p.title = "someone-elses-job".to_string();
        });
        journal(&home, me(), "", "gesture");

        let view = view_for_pane(PANE, None);

        assert_eq!(
            (
                view.certainty.as_str(),
                view.kind.as_str(),
                view.job_id.as_str()
            ),
            ("likely", "foreign", "")
        );
        assert_eq!(view.title, "someone-elses-job");
    }

    /// A second member whose name has this pane's member as a prefix.
    fn sibling_member(home: &Home) {
        set_probe(|p| {
            p.argv = "claude agents".to_string();
            p.panes = vec![pane(PANE, "red", "probe"), pane("%8", "red2", "probe")];
        });
        let _ = crate::adapters::claude_bg::write_pane_job("%8", OTHER_JOB, "session-2", "/tmp");
        journal(home, me(), "", "gesture");
    }

    #[test]
    fn test_a_prefix_named_sibling_resolves_to_itself() {
        let home = claude_home();
        let _probe = probe();
        sibling_member(&home);
        set_probe(|p| p.title = "probe.red2".to_string());

        let view = view_for_pane(PANE, None);

        assert_eq!(
            (
                view.kind.as_str(),
                view.member.as_str(),
                view.job_id.as_str()
            ),
            ("member_view", "probe.red2", OTHER_JOB)
        );
    }

    #[test]
    fn test_a_title_naming_two_members_resolves_to_nothing() {
        let home = claude_home();
        let _probe = probe();
        sibling_member(&home);
        set_probe(|p| p.title = "probe.red probe.red2".to_string());

        let view = view_for_pane(PANE, None);

        assert_eq!(
            (
                view.certainty.as_str(),
                view.kind.as_str(),
                view.job_id.as_str()
            ),
            ("unknown", "foreign", "")
        );
    }

    #[test]
    fn test_a_foreign_name_that_merely_contains_a_member_is_foreign() {
        // The hole a containment match would leave: keystrokes meant for
        // probe.red would land in this stranger's session.
        let home = claude_home();
        let _probe = probe();
        journal(&home, me(), "", "gesture");
        for title in ["probe.red-notes", "probe.reduce", "xprobe.red"] {
            set_probe(|p| {
                p.argv = "claude agents".to_string();
                p.title = title.to_string();
            });

            let view = view_for_pane(PANE, None);

            assert_eq!(
                (
                    view.certainty.as_str(),
                    view.kind.as_str(),
                    view.job_id.as_str()
                ),
                ("likely", "foreign", ""),
                "title {title:?}"
            );
        }
    }

    #[test]
    fn test_argument_text_is_never_identity() {
        // A grep for the attach command line is not a viewer.
        let home = claude_home();
        let _probe = probe();
        set_probe(|p| p.argv = "rg claude attach src".to_string());
        journal(&home, me(), "", "gesture");

        assert_eq!(view_for_pane(PANE, None).kind, "no_viewer");
    }

    // --- journal residue ----------------------------------------------------

    #[test]
    fn test_entry_of_a_dead_viewer_is_residue() {
        // Recycled pid: the entry names this viewer's pid, but that process
        // is long gone — the journal is full of such leftovers.
        let home = claude_home();
        let _probe = probe();
        set_probe(|p| {
            p.argv = "claude agents".to_string();
            p.title = "probe.red".to_string();
            p.viewer_pid = DEAD_PID;
        });
        journal(&home, DEAD_PID, "Tue Aug 25 09:58:20 2026", "gesture");

        assert_eq!(view_for_pane(PANE, None).kind, "list_view");
    }

    #[test]
    fn test_entry_whose_start_time_does_not_match_is_residue() {
        let home = claude_home();
        let _probe = probe();
        set_probe(|p| {
            p.argv = "claude agents".to_string();
            p.title = "probe.red".to_string();
        });
        journal(&home, me(), "Tue Aug 25 09:58:20 2026", "gesture");

        assert_eq!(view_for_pane(PANE, None).kind, "list_view");
    }

    #[test]
    fn test_a_missing_journal_directory_degrades_to_list_view() {
        let home = claude_home();
        let _probe = probe();
        set_probe(|p| {
            p.argv = "claude agents".to_string();
            p.title = "probe.red".to_string();
        });
        fs::remove_dir(home.dir.path().join("daemon").join("attach-journal")).unwrap();

        assert_eq!(journal_signature(), Vec::<String>::new());
        assert_eq!(view_for_pane(PANE, None).kind, "list_view");
    }

    #[test]
    fn test_journal_signature_tracks_entries() {
        let home = claude_home();
        assert_eq!(journal_signature(), Vec::<String>::new());
        journal(&home, me(), "", "one");
        assert_eq!(journal_signature(), vec!["one.json".to_string()]);
        journal(&home, me(), "", "two");
        assert_eq!(
            journal_signature(),
            vec!["one.json".to_string(), "two.json".to_string()]
        );
    }

    // --- border label -------------------------------------------------------

    #[test]
    fn test_view_label_is_empty_on_the_pane_s_own_member() {
        let view = PaneView {
            certainty: "certain".to_string(),
            kind: "member_view".to_string(),
            job_id: JOB.to_string(),
            member: "probe.red".to_string(),
            ..Default::default()
        };
        assert_eq!(view_label(&view, JOB), "");
    }

    #[test]
    fn test_view_label_names_another_member_and_a_foreign_session() {
        let other = PaneView {
            certainty: "likely".to_string(),
            kind: "member_view".to_string(),
            job_id: OTHER_JOB.to_string(),
            member: "comb.blue".to_string(),
            ..Default::default()
        };
        assert_eq!(view_label(&other, JOB), "comb.blue");
        let foreign = PaneView {
            certainty: "likely".to_string(),
            kind: "foreign".to_string(),
            title: "someone-elses-job".to_string(),
            ..Default::default()
        };
        assert_eq!(view_label(&foreign, JOB), "someone-elses-job");
    }

    #[test]
    fn test_view_label_says_nothing_when_no_session_is_displayed() {
        for kind in ["list_view", "no_viewer"] {
            let view = PaneView {
                certainty: "certain".to_string(),
                kind: kind.to_string(),
                ..Default::default()
            };
            assert_eq!(view_label(&view, JOB), "");
        }
        let unknown = PaneView {
            certainty: "unknown".to_string(),
            kind: "foreign".to_string(),
            title: "whatever".to_string(),
            ..Default::default()
        };
        assert_eq!(view_label(&unknown, JOB), "");
    }

    #[test]
    fn test_view_label_cannot_inject_a_tmux_format() {
        let view = PaneView {
            certainty: "likely".to_string(),
            kind: "foreign".to_string(),
            title: "#{pane_title}".to_string(),
            ..Default::default()
        };
        assert!(!view_label(&view, JOB).contains('#'));
    }
}
