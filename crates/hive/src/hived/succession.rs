//! Follow a desktop conversation across the CLI sessions it restarts as.
//!
//! A Claude session member enrolled from the desktop app carries the
//! conversation's stable id (`hostSessionId`, `claude_desktop`). When the
//! human rewinds and resends, clears, or returns to the pre-clear session,
//! the desktop restarts the CLI under a new session id and its record moves
//! `cliSessionId` on, keeping the old id under `priorCliSessionIds`; the
//! roster row still names the old one, so the member reads as gone and its
//! own `hive` calls find no team. This tick moves the row to the
//! conversation's current session — the hived's recalibration, never a
//! member's own action — and only for that exact shape: the desktop's
//! record for the row's own conversation names a different current session
//! *and* lists the row's session among its priors. A conversation the human
//! forked has a stable id of its own, so it never matches; a target session
//! that is not live, or that any member anywhere already holds, is refused;
//! two rows resolving to one target are both refused; and the write itself
//! is a compare-and-set under the store lock (`registry::commit_succession`),
//! so a row rebound or recreated between the observation and the write is
//! left alone. An event is emitted only for a write that landed.
//!
//! ponytail: bg job members are out of scope — their roster session id is
//! also their job address, so following a `/clear` there needs a job id on
//! the row first.

use serde_json::Value;

use super::seams::{
    hooked_commit_succession, hooked_cs_list_sessions, hooked_desktop_record,
    hooked_notify_debug_emit,
};
use crate::adapters::claude_desktop::DesktopRecord;
use crate::adapters::claude_sessions::ClaudeSession;

pub(super) const EVENT_SUCCEEDED: &str = "member.session_succeeded";
pub(super) const EVENT_REFUSED: &str = "member.session_refused";

/// A roster row this tick considers: a claude member with a host session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Row {
    pub name: String,
    pub session_id: String,
    pub host_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Plan {
    Move {
        name: String,
        from: String,
        to: String,
    },
    Refused {
        name: String,
        to: String,
        reason: &'static str,
    },
}

/// What each row's desktop record says, against the live sessions and the
/// sessions members already hold. Rows whose conversation has not moved,
/// or whose record is unknown, produce nothing.
pub(super) fn plan_successions(
    rows: &[Row],
    record: impl Fn(&str) -> Option<DesktopRecord>,
    live: &[ClaudeSession],
    taken: impl Fn(&str) -> bool,
) -> Vec<Plan> {
    let mut plans: Vec<Plan> = Vec::new();
    for row in rows {
        let Some(rec) = record(&row.host_session_id) else {
            continue;
        };
        let to = rec.cli_session_id;
        if to == row.session_id || !rec.prior_cli_session_ids.contains(&row.session_id) {
            continue;
        }
        let name = row.name.clone();
        if !live.iter().any(|s| s.session_id == to) {
            plans.push(Plan::Refused {
                name,
                to,
                reason: "target_not_live",
            });
        } else if taken(&to) {
            plans.push(Plan::Refused {
                name,
                to,
                reason: "target_taken",
            });
        } else {
            plans.push(Plan::Move {
                name,
                from: row.session_id.clone(),
                to,
            });
        }
    }
    // two rows converging on one session: neither can be right
    let targets: Vec<String> = plans
        .iter()
        .filter_map(|p| match p {
            Plan::Move { to, .. } => Some(to.clone()),
            Plan::Refused { .. } => None,
        })
        .collect();
    plans
        .into_iter()
        .map(|p| match p {
            Plan::Move { name, from: _, to }
                if targets.iter().filter(|t| **t == to).count() > 1 =>
            {
                Plan::Refused {
                    name,
                    to,
                    reason: "converge",
                }
            }
            other => other,
        })
        .collect()
}

fn created_at_key(entry: &serde_json::Map<String, Value>) -> String {
    match entry.get("createdAt") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// The tick: plan against *team*'s registry entry, commit each move, emit.
pub(super) fn reconcile_successions(workspace: &str, team: &str) {
    let Some(entry) = crate::registry::load(team) else {
        return;
    };
    let rows: Vec<Row> = entry
        .get("members")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_object)
                .filter(|m| m.get("cli").and_then(Value::as_str) == Some("claude"))
                .filter_map(|m| {
                    let host = m
                        .get(crate::registry::HOST_SESSION_FIELD)
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let sid = m
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let name = m.get("name").and_then(Value::as_str).unwrap_or_default();
                    (!host.is_empty() && !sid.is_empty() && !name.is_empty()).then(|| Row {
                        name: name.to_string(),
                        session_id: sid.to_string(),
                        host_session_id: host.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    if rows.is_empty() {
        return;
    }
    let live = hooked_cs_list_sessions();
    let created_at = created_at_key(&entry);
    let plans = plan_successions(&rows, hooked_desktop_record, &live, |sid| {
        crate::registry::member_for_session(sid, None).is_some()
    });
    for plan in plans {
        match plan {
            Plan::Move { name, from, to } => {
                let outcome = hooked_commit_succession(team, &name, &from, &to, &created_at)
                    .unwrap_or("error");
                let event = if outcome == "written" {
                    EVENT_SUCCEEDED
                } else {
                    EVENT_REFUSED
                };
                hooked_notify_debug_emit(
                    workspace,
                    event,
                    &[
                        ("team", Value::from(team)),
                        ("member", Value::from(name.as_str())),
                        ("from", Value::from(from.as_str())),
                        ("to", Value::from(to.as_str())),
                        ("reason", Value::from(outcome)),
                    ],
                );
            }
            Plan::Refused { name, to, reason } => hooked_notify_debug_emit(
                workspace,
                EVENT_REFUSED,
                &[
                    ("team", Value::from(team)),
                    ("member", Value::from(name.as_str())),
                    ("to", Value::from(to.as_str())),
                    ("reason", Value::from(reason)),
                ],
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hived::testhook;
    use crate::registry::{record_team, HOST_SESSION_FIELD};
    use serde_json::{json, Map};
    use std::sync::{Arc, Mutex};

    fn rec(current: &str, prior: &[&str]) -> DesktopRecord {
        DesktopRecord {
            cli_session_id: current.to_string(),
            prior_cli_session_ids: prior.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn live(sid: &str) -> ClaudeSession {
        ClaudeSession {
            name: "desk".to_string(),
            pid: 7,
            cwd: "/w".to_string(),
            kind: "interactive".to_string(),
            entrypoint: "claude-desktop".to_string(),
            socket_path: "/tmp/d.sock".to_string(),
            session_id: sid.to_string(),
            title: String::new(),
        }
    }

    fn row(name: &str, sid: &str, host: &str) -> Row {
        Row {
            name: name.to_string(),
            session_id: sid.to_string(),
            host_session_id: host.to_string(),
        }
    }

    #[test]
    fn test_plan_moves_only_a_conversation_that_moved_on_from_the_row() {
        let rows = [
            row("orch", "A", "local_h"),
            row("same", "S", "local_s"),
            row("fork", "F", "local_f"),
            row("gone", "G", "local_g"),
        ];
        let record = |h: &str| match h {
            "local_h" => Some(rec("C", &["A"])),
            "local_s" => Some(rec("S", &[])),
            // a record that moved on from a session that was never this row's
            "local_f" => Some(rec("C2", &["X"])),
            _ => None,
        };
        let plans = plan_successions(&rows, record, &[live("C"), live("C2")], |_| false);
        assert_eq!(
            plans,
            vec![Plan::Move {
                name: "orch".to_string(),
                from: "A".to_string(),
                to: "C".to_string(),
            }]
        );
    }

    #[test]
    fn test_plan_refuses_a_dead_taken_or_shared_target() {
        let rows = [row("m1", "A", "local_1"), row("m2", "B", "local_2")];
        let record = |h: &str| match h {
            "local_1" => Some(rec("C", &["A"])),
            "local_2" => Some(rec("C", &["B"])),
            _ => None,
        };
        // both rows resolve to C: converge
        let plans = plan_successions(&rows, record, &[live("C")], |_| false);
        assert!(plans.iter().all(|p| matches!(
            p,
            Plan::Refused {
                reason: "converge",
                ..
            }
        )));
        assert_eq!(plans.len(), 2);
        // C not live
        let plans = plan_successions(&rows[..1], record, &[], |_| false);
        assert_eq!(
            plans,
            vec![Plan::Refused {
                name: "m1".to_string(),
                to: "C".to_string(),
                reason: "target_not_live",
            }]
        );
        // C held by someone already
        let plans = plan_successions(&rows[..1], record, &[live("C")], |sid| sid == "C");
        assert_eq!(
            plans,
            vec![Plan::Refused {
                name: "m1".to_string(),
                to: "C".to_string(),
                reason: "target_taken",
            }]
        );
    }

    fn m(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), Value::String((*v).to_string())))
            .collect()
    }

    #[test]
    fn test_reconcile_commits_a_move_and_emits_only_for_the_write_that_landed() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::testenv::iso(tmp.path());
        record_team(
            "honey",
            "/ws",
            "1.0",
            &[
                m(&[
                    ("name", "orch"),
                    ("cli", "claude"),
                    ("sessionId", "A"),
                    (HOST_SESSION_FIELD, "local_h"),
                ]),
                m(&[("name", "rex"), ("cli", "codex"), ("sessionId", "t1")]),
            ],
            "",
        )
        .unwrap();
        type Events = Arc<Mutex<Vec<(String, Map<String, Value>)>>>;
        let events: Events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let hook = testhook::Hook {
            cs_list_sessions: Some(Arc::new(|| vec![live("C")])),
            desktop_record: Some(Arc::new(|h: &str| {
                (h == "local_h").then(|| rec("C", &["A"]))
            })),
            notify_debug_emit: Some(Arc::new(move |_ws, event, fields| {
                let map: Map<String, Value> = fields
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), v.clone()))
                    .collect();
                sink.lock().unwrap().push((event.to_string(), map));
            })),
            ..testhook::Hook::default()
        };
        let _guard = testhook::install(hook);

        reconcile_successions("/ws", "honey");
        let entry = crate::registry::load("honey").unwrap();
        assert_eq!(entry["members"][0]["sessionId"], "C");
        assert_eq!(entry["members"][0][HOST_SESSION_FIELD], "local_h");
        assert_eq!(entry["members"][1]["sessionId"], "t1");
        {
            let got = events.lock().unwrap();
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].0, EVENT_SUCCEEDED);
            assert_eq!(got[0].1["member"], "orch");
            assert_eq!(got[0].1["from"], "A");
            assert_eq!(got[0].1["to"], "C");
        }

        // the next tick sees a row already on C: nothing to do, no event
        reconcile_successions("/ws", "honey");
        assert_eq!(events.lock().unwrap().len(), 1);

        // a target another team's member holds is refused, no move, no
        // success event
        record_team(
            "comb",
            "/ws2",
            "2.0",
            &[m(&[("name", "ant"), ("cli", "claude"), ("sessionId", "D")])],
            "",
        )
        .unwrap();
        testhook::update(|h| {
            h.cs_list_sessions = Some(Arc::new(|| vec![live("D")]));
            h.desktop_record = Some(Arc::new(|h: &str| {
                (h == "local_h").then(|| rec("D", &["A", "C"]))
            }));
        });
        reconcile_successions("/ws", "honey");
        assert_eq!(
            crate::registry::load("honey").unwrap()["members"][0]["sessionId"],
            "C"
        );
        let got = events.lock().unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].0, EVENT_REFUSED);
        assert_eq!(got[1].1["reason"], "target_taken");
        assert_eq!(json!(got[1].1["to"]), json!("D"));
    }
}
