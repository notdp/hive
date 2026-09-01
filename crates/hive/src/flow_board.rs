//! hive::flow_board — `hive flow board`: a live progress board for a flow
//! team, made to sit in a tmux pane next to the members it describes.
//!
//! It renders only truth the flow machinery already writes: the registry
//! roster with live-pane binding (`Team::load`) and the `flow.run` mailbox
//! rows on the team bus — dispatch and reply timestamps give each node its
//! state and elapsed time. Serial/parallel topology cannot be derived from
//! either, so it comes from an optional sidecar the orchestrator writes:
//! `<workspace>/artifacts/flow/board.json`,
//! `{"workflow": "...", "phases": [{"title": "...", "nodes": ["..."]}]}`.
//! Without it the board falls back to a flat node list.
//!
//! The pane tags itself `@hive-role dock` so the adaptive layout leaves the
//! strip alone (see `layout.rs`), and paints on the alternate screen —
//! in-place repaint, no scrollback pollution.

use std::io::Write as _;

use serde_json::Value;

use unicode_width::UnicodeWidthChar;

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";

#[derive(Debug, Clone, PartialEq)]
pub enum NodeState {
    /// Planned in the sidecar, not yet on the roster or bus.
    Pending,
    /// On the roster with a live pane, no task dispatched yet.
    Spawned,
    /// Dispatched, pane alive, no reply yet.
    Working,
    /// Replied to flow.run — delivered, whatever happened to the pane since.
    Done,
    /// Dispatched, pane gone, no reply: this node will never resolve.
    Gone,
}

#[derive(Debug, Clone)]
pub struct NodeRow {
    pub name: String,
    pub runtime: String,
    pub state: NodeState,
    pub elapsed: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RosterRow {
    pub name: String,
    pub cli: String,
    pub model: String,
    pub has_pane: bool,
}

/// Minutes-grade ISO-8601 UTC (`YYYY-MM-DDTHH:MM:SS...`) to epoch seconds —
/// the bus's `now_iso` shape; anything else is None.
pub fn iso_to_epoch(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // days-from-civil (Howard Hinnant), valid for the Gregorian calendar
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3_600 + mi * 60 + sec).ok()
}

/// Join roster liveness with the flow.run mailbox into per-node rows. Pure —
/// the whole state machine lives here and under test.
pub fn derive_rows(
    roster: &[RosterRow],
    events: &[crate::bus::Event],
    planned: &[String],
    now_epoch: u64,
) -> Vec<NodeRow> {
    use std::collections::HashMap;
    let mut dispatch: HashMap<&str, u64> = HashMap::new();
    let mut reply: HashMap<&str, u64> = HashMap::new();
    for ev in events {
        let ts = iso_to_epoch(&ev.created_at).unwrap_or(now_epoch);
        if ev.from == crate::flow::FLOW_SENDER {
            dispatch.entry(ev.to.as_str()).or_insert(ts);
        } else if ev.to == crate::flow::FLOW_SENDER {
            reply.entry(ev.from.as_str()).or_insert(ts);
        }
    }

    let mut names: Vec<String> = Vec::new();
    for n in planned {
        if !names.contains(n) {
            names.push(n.clone());
        }
    }
    for r in roster {
        if !names.contains(&r.name) {
            names.push(r.name.clone());
        }
    }

    names
        .into_iter()
        .map(|name| {
            let row = roster.iter().find(|r| r.name == name);
            let runtime = row
                .map(|r| {
                    if r.model.is_empty() {
                        r.cli.clone()
                    } else {
                        format!("{} · {}", r.cli, r.model)
                    }
                })
                .unwrap_or_else(|| "-".to_string());
            let sent = dispatch.get(name.as_str()).copied();
            let got = reply.get(name.as_str()).copied();
            let alive = row.map(|r| r.has_pane).unwrap_or(false);
            let (state, elapsed) = match (sent, got) {
                (Some(s), Some(g)) => (NodeState::Done, Some(g.saturating_sub(s))),
                (Some(s), None) if alive => (NodeState::Working, Some(now_epoch.saturating_sub(s))),
                (Some(s), None) => (NodeState::Gone, Some(now_epoch.saturating_sub(s))),
                (None, _) if row.is_some() => (NodeState::Spawned, None),
                (None, _) => (NodeState::Pending, None),
            };
            NodeRow {
                name,
                runtime,
                state,
                elapsed,
            }
        })
        .collect()
}

fn fmt_dur(sec: u64) -> String {
    if sec >= 60 {
        format!("{}m{:02}s", sec / 60, sec % 60)
    } else {
        format!("{sec}s")
    }
}

fn wcw(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

fn pad(s: &str, width: usize) -> String {
    let gap = width.saturating_sub(wcw(s));
    format!("{s}{}", " ".repeat(gap))
}

fn clip(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

#[derive(Debug, Default)]
struct BoardSpec {
    workflow: String,
    phases: Vec<(String, Vec<String>)>,
}

fn load_spec(workspace: &str) -> BoardSpec {
    let path = std::path::Path::new(workspace)
        .join("artifacts")
        .join("flow")
        .join("board.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return BoardSpec::default();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return BoardSpec::default();
    };
    let phases = v["phases"]
        .as_array()
        .map(|ps| {
            ps.iter()
                .filter_map(|p| {
                    let title = p["title"].as_str()?.to_string();
                    let nodes = p["nodes"]
                        .as_array()?
                        .iter()
                        .filter_map(|n| n.as_str().map(str::to_string))
                        .collect();
                    Some((title, nodes))
                })
                .collect()
        })
        .unwrap_or_default();
    BoardSpec {
        workflow: v["workflow"].as_str().unwrap_or("").to_string(),
        phases,
    }
}

extern "C" fn _restore_and_exit(_: libc::c_int) {
    const RESTORE: &[u8] = b"\x1b[?1049l\x1b[?25h";
    unsafe {
        libc::write(1, RESTORE.as_ptr() as *const libc::c_void, RESTORE.len());
        libc::_exit(0);
    }
}

fn render(
    spec: &BoardSpec,
    team: &str,
    rows: &[NodeRow],
    mail: &[(String, String, String)],
    cols: usize,
    lines_budget: usize,
    tick: u64,
) -> String {
    let phases: Vec<(String, Vec<String>)> = if spec.phases.is_empty() {
        vec![("nodes".to_string(), rows.iter().map(|r| r.name.clone()).collect())]
    } else {
        let planned: Vec<&String> = spec.phases.iter().flat_map(|(_, ns)| ns).collect();
        let stray: Vec<String> = rows
            .iter()
            .map(|r| r.name.clone())
            .filter(|n| !planned.contains(&n))
            .collect();
        let mut phases = spec.phases.clone();
        if !stray.is_empty() {
            phases.push(("nodes".to_string(), stray));
        }
        phases
    };

    let by_name = |n: &str| rows.iter().find(|r| r.name == n);
    let name_w = rows
        .iter()
        .map(|r| wcw(&r.name))
        .chain(phases.iter().flat_map(|(_, ns)| ns.iter().map(|n| wcw(n))))
        .max()
        .unwrap_or(8)
        .max(8);
    let rt_w = rows.iter().map(|r| wcw(&r.runtime)).max().unwrap_or(10).max(10);

    let mut out: Vec<String> = Vec::new();
    let clock = {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("{:02}:{:02}:{:02}Z", secs / 3600 % 24, secs / 60 % 60, secs % 60)
    };
    out.push(format!(
        "{DIM}{}{}team={team}  {clock}{RESET}",
        spec.workflow,
        if spec.workflow.is_empty() { "" } else { "  " },
    ));
    for (title, node_names) in &phases {
        let done = node_names
            .iter()
            .filter(|n| by_name(n).map(|r| r.state == NodeState::Done).unwrap_or(false))
            .count();
        let tag = if node_names.len() > 1 { "∥" } else { "→" };
        out.push(format!("{BOLD} {tag} {title}  {done}/{}{RESET}", node_names.len()));
        for n in node_names {
            let Some(r) = by_name(n) else {
                out.push(format!(
                    "{DIM}     · {}  {}  {:>7}  pending{RESET}",
                    pad(n, name_w),
                    pad("-", rt_w),
                    ""
                ));
                continue;
            };
            let dur = r.elapsed.map(fmt_dur).unwrap_or_default();
            let plain = format!(
                "{}  {}  {:>7}  {}",
                pad(&r.name, name_w),
                pad(&r.runtime, rt_w),
                dur,
                match r.state {
                    NodeState::Pending => "pending",
                    NodeState::Spawned => "spawned",
                    NodeState::Working => "working",
                    NodeState::Done => "done",
                    NodeState::Gone => "gone",
                }
            );
            let plain = clip(&plain, cols.saturating_sub(7));
            let line = match r.state {
                NodeState::Done => format!("     {DIM}✔{RESET} {DIM}{plain}{RESET}"),
                NodeState::Gone => format!("     {RED}✖{RESET} {RED}{plain}{RESET}"),
                NodeState::Working => {
                    let mark = if tick % 2 == 0 {
                        format!("{YELLOW}●{RESET}")
                    } else {
                        format!("{DIM}{YELLOW}●{RESET}")
                    };
                    format!("     {mark} {plain}")
                }
                NodeState::Spawned => format!("     ○ {plain}"),
                NodeState::Pending => format!("     {DIM}· {plain}{RESET}"),
            };
            out.push(line);
        }
    }
    if !mail.is_empty() {
        out.push(format!("{BOLD} mailbox{RESET}"));
        for (from, to, body) in mail {
            let body = body.split_whitespace().collect::<Vec<_>>().join(" ");
            let body = match body.strip_prefix("flow-mailbox dispatch: ") {
                Some(rest) => format!(
                    "[dispatch] {}",
                    rest.split_whitespace().next().unwrap_or(rest)
                ),
                None => body,
            };
            out.push(format!(
                "{DIM}   {from} → {to}  {}{RESET}",
                clip(&body, cols.saturating_sub(10 + wcw(from) + wcw(to)))
            ));
        }
    }
    if out.len() > lines_budget {
        let extra = out.len() - lines_budget + 1;
        out.truncate(lines_budget.saturating_sub(1).max(1));
        out.push(format!("{DIM}   … +{extra} more{RESET}"));
    }
    format!("\x1b[H\x1b[J{}", out.join("\n"))
}

/// `hive flow board` body: resolve the team, tag this pane as a dock, paint
/// until interrupted. Returns the process exit code.
pub fn board_cmd(team: Option<&str>) -> i32 {
    let (team_name, team) =
        match crate::cli::resolve_scoped_team(team, true) {
            Ok((Some(name), Some(team))) => (name, team),
            Ok(_) | Err(_) => {
                eprintln!("Error: no Hive team in scope — pass --team <team> (see `hive ls`)");
                return 1;
            }
        };
    let workspace = crate::cli::resolve_workspace(Some(&team), true).unwrap_or_default();
    if workspace.is_empty() {
        eprintln!("Error: team '{team_name}' has no workspace");
        return 1;
    }

    if let Ok(pane) = std::env::var("TMUX_PANE") {
        // A dock pane: the adaptive layout must not fold this strip into a
        // member preset.
        let _ = std::process::Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane, "@hive-role", "dock"])
            .status();
        let _ = std::process::Command::new("tmux")
            .args(["select-pane", "-t", &pane, "-T", "⬡ flow board"])
            .status();
    }

    print!("\x1b[?1049h\x1b[?25l");
    let _ = std::io::stdout().flush();
    unsafe {
        libc::signal(libc::SIGINT, _restore_and_exit as libc::sighandler_t);
        libc::signal(libc::SIGTERM, _restore_and_exit as libc::sighandler_t);
    }

    let mut tick: u64 = 0;
    loop {
        let spec = load_spec(&workspace);
        let roster: Vec<RosterRow> = match crate::team::Team::load(&team_name, "") {
            Ok(t) => t
                .agents
                .iter()
                .map(|a| RosterRow {
                    name: a.name.clone(),
                    cli: a.cli.clone(),
                    model: a.model.clone(),
                    has_pane: !a.pane_id.is_empty(),
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        let events = crate::bus::read_all_events(&workspace).unwrap_or_default();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let planned: Vec<String> = spec
            .phases
            .iter()
            .flat_map(|(_, ns)| ns.iter().cloned())
            .collect();
        let rows = derive_rows(&roster, &events, &planned, now);
        let mail: Vec<(String, String, String)> = events
            .iter()
            .rev()
            .filter(|e| e.from == crate::flow::FLOW_SENDER || e.to == crate::flow::FLOW_SENDER)
            .take(3)
            .map(|e| (e.from.clone(), e.to.clone(), e.body.clone()))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        let (cols, lines) = crossterm::terminal::size()
            .map(|(c, l)| (c as usize, l as usize))
            .unwrap_or((120, 14));
        print!("{}", render(&spec, &team_name, &rows, &mail, cols, lines, tick));
        let _ = std::io::stdout().flush();
        tick += 1;
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Event;
    use serde_json::Map;

    fn ev(from: &str, to: &str, created_at: &str) -> Event {
        Event {
            from: from.to_string(),
            to: to.to_string(),
            intent: "send".to_string(),
            metadata: Map::new(),
            created_at: created_at.to_string(),
            msg_id: String::new(),
            in_reply_to: String::new(),
            body: String::new(),
            artifact: String::new(),
        }
    }

    fn roster(name: &str, has_pane: bool) -> RosterRow {
        RosterRow {
            name: name.to_string(),
            cli: "claude".to_string(),
            model: "opus".to_string(),
            has_pane,
        }
    }

    #[test]
    fn test_iso_to_epoch_matches_known_points() {
        assert_eq!(iso_to_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_to_epoch("2026-09-02T00:00:00Z"), Some(1_788_307_200));
        assert_eq!(iso_to_epoch("garbage"), None);
    }

    #[test]
    fn test_derive_rows_full_state_machine() {
        let t0 = "2026-09-02T00:00:00Z";
        let t1 = "2026-09-02T00:05:00Z";
        let now = iso_to_epoch("2026-09-02T00:10:00Z").unwrap();
        let events = vec![
            ev("flow.run", "done-node", t0),
            ev("done-node", "flow.run", t1),
            ev("flow.run", "working-node", t0),
            ev("flow.run", "gone-node", t0),
        ];
        let roster = vec![
            roster("done-node", false), // retired after replying: still done
            roster("working-node", true),
            roster("gone-node", false),
            roster("spawned-node", true),
        ];
        let planned = vec!["pending-node".to_string()];
        let rows = derive_rows(&roster, &events, &planned, now);
        let get = |n: &str| rows.iter().find(|r| r.name == n).unwrap();

        assert_eq!(get("done-node").state, NodeState::Done);
        assert_eq!(get("done-node").elapsed, Some(300));
        assert_eq!(get("working-node").state, NodeState::Working);
        assert_eq!(get("working-node").elapsed, Some(600));
        assert_eq!(get("gone-node").state, NodeState::Gone);
        assert_eq!(get("spawned-node").state, NodeState::Spawned);
        assert_eq!(get("pending-node").state, NodeState::Pending);
        assert_eq!(get("pending-node").runtime, "-");
        // planned nodes come first, roster-only nodes after
        assert_eq!(rows[0].name, "pending-node");
    }

    #[test]
    fn test_render_groups_phases_and_folds_dispatch_bodies() {
        let spec = BoardSpec {
            workflow: "review".to_string(),
            phases: vec![
                ("Review".to_string(), vec!["a".to_string(), "b".to_string()]),
                ("Verify".to_string(), vec!["v".to_string()]),
            ],
        };
        let rows = vec![
            NodeRow { name: "a".into(), runtime: "claude · opus".into(), state: NodeState::Done, elapsed: Some(65) },
            NodeRow { name: "b".into(), runtime: "grok · 4.6".into(), state: NodeState::Working, elapsed: Some(10) },
        ];
        let mail = vec![(
            "flow.run".to_string(),
            "v".to_string(),
            "flow-mailbox dispatch: v.md (not a member; hive send flow.run, then stop)".to_string(),
        )];
        let s = render(&spec, "t", &rows, &mail, 120, 40, 0);
        assert!(s.contains("∥ Review  1/2"), "{s}");
        assert!(s.contains("→ Verify  0/1"), "{s}");
        assert!(s.contains("1m05s"), "{s}");
        assert!(s.contains("pending"), "{s}"); // v planned but absent
        assert!(s.contains("[dispatch] v.md"), "{s}");
        assert!(!s.contains("not a member"), "{s}");
    }
}
