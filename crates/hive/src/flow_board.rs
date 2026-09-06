//! hive::flow_board — `hive flow board`: a live progress board for a flow
//! team, made to sit in a dock pane next to the members it describes.
//!
//! Everything it shows is truth the flow machinery already writes: the
//! roster (`Team::load`, whose pane groups carry each node's phase), the
//! hived's liveness answer, and the `flow.run` mailbox on the team bus —
//! a dispatch row and the reply anchored to it by `in_reply_to` give each
//! node its state and elapsed time. Phases come from the pane groups the
//! spawns set, so serial/parallel structure needs no sidecar.
//!
//! The pane tags itself `@hive-role dock`; `layout::ensure` keeps
//! the strip at the bottom and tiles the members above it.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;

use unicode_width::UnicodeWidthChar;

use crate::adapters::base::parse_iso_timestamp;
use crate::bus::Event;
use crate::flow::{DISPATCH_BODY_PREFIX, FLOW_SENDER};

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";

#[derive(Debug, Clone, PartialEq)]
pub enum NodeState {
    /// On the roster, no task dispatched yet.
    Spawned,
    /// Dispatched, alive, no reply yet.
    Working,
    /// Replied to the dispatch — delivered, whatever happened to it since.
    Done,
    /// Dispatched, dead, no reply: will never resolve.
    Gone,
}

#[derive(Debug, Clone)]
pub struct NodeRow {
    pub name: String,
    pub runtime: String,
    pub phase: String,
    pub state: NodeState,
    pub elapsed: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RosterRow {
    pub name: String,
    pub cli: String,
    pub model: String,
    pub phase: String,
    pub alive: bool,
}

/// `YYYY-MM-DDTHH:MM:SS…` (the bus's `now_iso` shape) to epoch seconds.
fn iso_to_epoch(s: &str) -> Option<u64> {
    let dt = parse_iso_timestamp(Some(&serde_json::Value::String(s.to_string())))?;
    u64::try_from(dt.timestamp() as i64).ok()
}

/// Join the roster with the flow.run mailbox into per-node rows, in roster
/// order. Pure — the state machine lives here and under test.
pub fn derive_rows(roster: &[RosterRow], events: &[Event], now_epoch: u64) -> Vec<NodeRow> {
    // latest dispatch per member, and the reply anchored to that dispatch
    let mut dispatch: HashMap<&str, (&str, u64)> = HashMap::new();
    let mut replied: HashMap<&str, u64> = HashMap::new(); // by dispatch msg_id
    for ev in events {
        let ts = iso_to_epoch(&ev.created_at).unwrap_or(now_epoch);
        if ev.from == FLOW_SENDER {
            dispatch.insert(ev.to.as_str(), (ev.msg_id.as_str(), ts));
        } else if ev.to == FLOW_SENDER && !ev.in_reply_to.is_empty() {
            replied.entry(ev.in_reply_to.as_str()).or_insert(ts);
        }
    }
    roster
        .iter()
        .map(|r| {
            let runtime = if r.model.is_empty() {
                r.cli.clone()
            } else {
                format!("{} · {}", r.cli, r.model)
            };
            let (state, elapsed) = match dispatch.get(r.name.as_str()) {
                Some((msg_id, sent)) => match replied.get(msg_id) {
                    Some(got) => (NodeState::Done, Some(got.saturating_sub(*sent))),
                    None if r.alive => (NodeState::Working, Some(now_epoch.saturating_sub(*sent))),
                    None => (NodeState::Gone, Some(now_epoch.saturating_sub(*sent))),
                },
                None => (NodeState::Spawned, None),
            };
            NodeRow {
                name: r.name.clone(),
                runtime,
                phase: r.phase.clone(),
                state,
                elapsed,
            }
        })
        .collect()
}

/// Phases in first-seen order; a node without a group falls under "nodes".
pub fn group_phases(rows: &[NodeRow]) -> Vec<(String, Vec<&NodeRow>)> {
    let mut phases: Vec<(String, Vec<&NodeRow>)> = Vec::new();
    for r in rows {
        let title = if r.phase.is_empty() {
            "nodes"
        } else {
            &r.phase
        };
        match phases.iter_mut().find(|(t, _)| t == title) {
            Some((_, nodes)) => nodes.push(r),
            None => phases.push((title.to_string(), vec![r])),
        }
    }
    phases
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
    format!("{s}{}", " ".repeat(width.saturating_sub(wcw(s))))
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

pub fn render(
    team: &str,
    rows: &[NodeRow],
    mail: &[&Event],
    cols: usize,
    lines_budget: usize,
    tick: u64,
) -> String {
    let phases = group_phases(rows);
    let name_w = rows.iter().map(|r| wcw(&r.name)).max().unwrap_or(8).max(8);
    let rt_w = rows
        .iter()
        .map(|r| wcw(&r.runtime))
        .max()
        .unwrap_or(10)
        .max(10);
    let clock = {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!(
            "{:02}:{:02}:{:02}Z",
            secs / 3600 % 24,
            secs / 60 % 60,
            secs % 60
        )
    };
    let mut out: Vec<String> = vec![format!("{DIM}team={team}  {clock}{RESET}")];
    if rows.is_empty() {
        out.push(format!("{DIM}   no nodes yet{RESET}"));
    }
    for (title, nodes) in &phases {
        let done = nodes.iter().filter(|r| r.state == NodeState::Done).count();
        let tag = if nodes.len() > 1 { "∥" } else { "→" };
        out.push(format!(
            "{BOLD} {tag} {title}  {done}/{}{RESET}",
            nodes.len()
        ));
        for r in nodes {
            let dur = r.elapsed.map(fmt_dur).unwrap_or_default();
            let plain = clip(
                &format!(
                    "{}  {}  {:>7}  {}",
                    pad(&r.name, name_w),
                    pad(&r.runtime, rt_w),
                    dur,
                    match r.state {
                        NodeState::Spawned => "spawned",
                        NodeState::Working => "working",
                        NodeState::Done => "done",
                        NodeState::Gone => "gone",
                    }
                ),
                cols.saturating_sub(7),
            );
            out.push(match r.state {
                NodeState::Done => format!("     {DIM}✔{RESET} {DIM}{plain}{RESET}"),
                NodeState::Gone => format!("     {RED}✖{RESET} {RED}{plain}{RESET}"),
                NodeState::Working => {
                    let mark = if tick.is_multiple_of(2) {
                        YELLOW
                    } else {
                        "\x1b[2;33m"
                    };
                    format!("     {mark}●{RESET} {plain}")
                }
                NodeState::Spawned => format!("     ○ {plain}"),
            });
        }
    }
    if !mail.is_empty() {
        out.push(format!("{BOLD} mailbox{RESET}"));
        for e in mail {
            let (from, to) = (&e.from, &e.to);
            let body = e.body.split_whitespace().collect::<Vec<_>>().join(" ");
            let body = match body.strip_prefix(DISPATCH_BODY_PREFIX) {
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

extern "C" fn _restore_and_exit(_: libc::c_int) {
    const RESTORE: &[u8] = b"\x1b[?1049l\x1b[?25h";
    unsafe {
        libc::write(1, RESTORE.as_ptr() as *const libc::c_void, RESTORE.len());
        libc::_exit(0);
    }
}

/// Roster + liveness in one hived round-trip (pane-bound fallback).
fn roster_snapshot(team_name: &str) -> Vec<RosterRow> {
    let Ok(team) = crate::team::Team::load(team_name, "") else {
        return Vec::new();
    };
    let runtime = if team.workspace.is_empty() {
        None
    } else {
        crate::hived::request_team_runtime(&team.workspace, &team.name)
    };
    roster_rows(&team, runtime_alive_set(runtime))
}

/// The members the hived reports `cliAlive`, or None when its answer is not
/// usable (`team::usable_runtime`) and pane liveness has to stand in.
fn runtime_alive_set(
    runtime: Option<serde_json::Map<String, serde_json::Value>>,
) -> Option<HashSet<String>> {
    crate::team::usable_runtime(runtime).map(|rt| {
        rt.get("members")
            .and_then(serde_json::Value::as_object)
            .map(|m| {
                m.iter()
                    .filter(|(_, v)| {
                        v.get("cliAlive").and_then(serde_json::Value::as_bool) == Some(true)
                    })
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default()
    })
}

fn roster_rows(team: &crate::team::Team, runtime_alive: Option<HashSet<String>>) -> Vec<RosterRow> {
    team.agents
        .iter()
        .map(|a| RosterRow {
            name: a.name.clone(),
            cli: a.cli.clone(),
            model: a.model.clone(),
            phase: team.member_groups.get(&a.name).cloned().unwrap_or_default(),
            alive: match &runtime_alive {
                Some(set) => set.contains(&a.name),
                None => !a.pane_id.is_empty(),
            },
        })
        .collect()
}

/// `hive flow board` body: resolve the team, dock this pane, paint until
/// interrupted. Returns the process exit code.
pub fn board_cmd(team: Option<&str>) -> i32 {
    let (team_name, team) = match crate::cli::resolve_scoped_team(team, true) {
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
        crate::tmux::set_pane_option(&pane, "hive-role", "dock");
        crate::tmux::set_pane_title(&pane, "⬡ flow board");
        if !team.tmux_window.is_empty() {
            let _ = crate::layout::ensure(&team.tmux_window, false);
        }
    }

    print!("\x1b[?1049h\x1b[?25l");
    let _ = std::io::stdout().flush();
    unsafe {
        let handler = _restore_and_exit as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }

    let mut tick: u64 = 0;
    loop {
        let roster = roster_snapshot(&team_name);
        let events = crate::bus::read_all_events(&workspace).unwrap_or_default();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let rows = derive_rows(&roster, &events, now);
        let mail: Vec<&Event> = events
            .iter()
            .rev()
            .filter(|e| e.from == FLOW_SENDER || e.to == FLOW_SENDER)
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let (cols, lines) = crossterm::terminal::size()
            .map(|(c, l)| (c as usize, l as usize))
            .unwrap_or((120, 14));
        print!("{}", render(&team_name, &rows, &mail, cols, lines, tick));
        let _ = std::io::stdout().flush();
        tick += 1;
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    fn ev(from: &str, to: &str, msg_id: &str, in_reply_to: &str, created_at: &str) -> Event {
        Event {
            from: from.to_string(),
            to: to.to_string(),
            intent: "send".to_string(),
            metadata: Map::new(),
            created_at: created_at.to_string(),
            msg_id: msg_id.to_string(),
            in_reply_to: in_reply_to.to_string(),
            body: String::new(),
            artifact: String::new(),
        }
    }

    fn roster(name: &str, phase: &str, alive: bool) -> RosterRow {
        RosterRow {
            name: name.to_string(),
            cli: "claude".to_string(),
            model: "opus".to_string(),
            phase: phase.to_string(),
            alive,
        }
    }

    fn team_with(members: &[(&str, &str)]) -> crate::team::Team {
        let mut team = crate::team::Team {
            name: "t".to_string(),
            workspace: "/ws".to_string(),
            ..Default::default()
        };
        for (name, pane) in members {
            team.agents.push(crate::agent::Agent {
                model: "opus".to_string(),
                cwd: String::new(),
                ..crate::agent::testhook::fake_agent(name, "t", pane, "claude")
            });
        }
        team
    }

    fn alive_by_name(rows: &[RosterRow]) -> Vec<(String, bool)> {
        rows.iter().map(|r| (r.name.clone(), r.alive)).collect()
    }

    #[test]
    fn test_runtime_alive_set_error_envelope_is_unknown_not_offline() {
        let err = serde_json::json!({"ok": false, "error": "load failed"});
        assert_eq!(runtime_alive_set(err.as_object().cloned()), None);
        assert_eq!(runtime_alive_set(None), None);
        assert_eq!(runtime_alive_set(Some(Map::new())), None);

        let ok = serde_json::json!({"ok": true, "members": {"a": {"cliAlive": true}, "b": {"cliAlive": false}}});
        let set = runtime_alive_set(ok.as_object().cloned()).unwrap();
        assert_eq!(set, HashSet::from(["a".to_string()]));
    }

    #[test]
    fn test_roster_rows_hived_error_keeps_pane_liveness() {
        let team = team_with(&[("a", "%1"), ("b", "")]);
        let err = serde_json::json!({"ok": false, "error": "load failed"});
        let rows = roster_rows(&team, runtime_alive_set(err.as_object().cloned()));
        assert_eq!(
            alive_by_name(&rows),
            vec![("a".to_string(), true), ("b".to_string(), false)]
        );
    }

    #[test]
    fn test_roster_rows_hived_answer_overrides_pane() {
        let team = team_with(&[("a", "%1"), ("b", "")]);
        let ok = serde_json::json!({"ok": true, "members": {"b": {"cliAlive": true}}});
        let rows = roster_rows(&team, runtime_alive_set(ok.as_object().cloned()));
        assert_eq!(
            alive_by_name(&rows),
            vec![("a".to_string(), false), ("b".to_string(), true)]
        );
    }

    #[test]
    fn test_iso_to_epoch_matches_known_points() {
        assert_eq!(iso_to_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_to_epoch("2026-09-02T00:00:00Z"), Some(1_788_307_200));
        assert_eq!(iso_to_epoch("garbage"), None);
    }

    #[test]
    fn test_derive_rows_uses_in_reply_to_and_liveness() {
        let t0 = "2026-09-02T00:00:00Z";
        let t1 = "2026-09-02T00:05:00Z";
        let now = iso_to_epoch("2026-09-02T00:10:00Z").unwrap();
        let events = vec![
            ev("flow.run", "done-node", "d1", "", t0),
            ev("done-node", "flow.run", "r1", "d1", t1),
            ev("flow.run", "working-node", "d2", "", t0),
            ev("flow.run", "gone-node", "d3", "", t0),
            // a bystander row to flow.run not anchored to a dispatch counts for nothing
            ev("working-node", "flow.run", "x", "", t1),
        ];
        let roster = vec![
            roster("done-node", "Review", false), // retired after replying: still done
            roster("working-node", "Review", true),
            roster("gone-node", "Verify", false),
            roster("spawned-node", "", true),
        ];
        let rows = derive_rows(&roster, &events, now);
        let get = |n: &str| rows.iter().find(|r| r.name == n).unwrap();
        assert_eq!(get("done-node").state, NodeState::Done);
        assert_eq!(get("done-node").elapsed, Some(300));
        assert_eq!(get("working-node").state, NodeState::Working);
        assert_eq!(get("working-node").elapsed, Some(600));
        assert_eq!(get("gone-node").state, NodeState::Gone);
        assert_eq!(get("spawned-node").state, NodeState::Spawned);

        let phases = group_phases(&rows);
        let titles: Vec<&str> = phases.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(titles, vec!["Review", "Verify", "nodes"]);
        assert_eq!(phases[0].1.len(), 2);
    }

    #[test]
    fn test_render_marks_parallel_phases_and_folds_dispatch_bodies() {
        let rows = vec![
            NodeRow {
                name: "a".into(),
                runtime: "claude · opus".into(),
                phase: "Review".into(),
                state: NodeState::Done,
                elapsed: Some(65),
            },
            NodeRow {
                name: "b".into(),
                runtime: "grok · 4.6".into(),
                phase: "Review".into(),
                state: NodeState::Working,
                elapsed: Some(10),
            },
            NodeRow {
                name: "v".into(),
                runtime: "claude · opus".into(),
                phase: "Verify".into(),
                state: NodeState::Spawned,
                elapsed: None,
            },
        ];
        let mut dispatch = ev("flow.run", "v", "d1", "", "2026-09-02T00:00:00Z");
        dispatch.body =
            format!("{DISPATCH_BODY_PREFIX}v.md (not a member; hive send flow.run, then stop)");
        let s = render("t", &rows, &[&dispatch], 120, 40, 0);
        assert!(s.contains("∥ Review  1/2"), "{s}");
        assert!(s.contains("→ Verify  0/1"), "{s}");
        assert!(s.contains("1m05s"), "{s}");
        assert!(s.contains("[dispatch] v.md"), "{s}");
        assert!(!s.contains("not a member"), "{s}");
    }
}
