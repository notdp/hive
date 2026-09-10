//! Follow a desktop conversation across the CLI sessions it restarts as.
//!
//! A Claude session member enrolled from the desktop app carries the
//! conversation's stable id (`hostSessionId`, `claude_desktop`). When the
//! human rewinds and resends, clears, or returns to the pre-clear session,
//! the desktop restarts the CLI under a new session id and its record moves
//! `cliSessionId` on, keeping the old id under `priorCliSessionIds`; the
//! roster row still names the old one, so the member reads as gone and its
//! own `hive` calls find no team. This tick and the CLI identity fallback
//! use `crate::succession` to plan the same move, only for that exact shape:
//! the desktop's record for the row's own conversation names a different current session
//! *and* lists the row's session among its priors. A conversation the human
//! forked has a stable id of its own, so it never matches; a target session
//! that is not live, an old session still live, or a target any member
//! anywhere already holds is refused; two rows resolving to one target are both refused; and the write itself
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
#[cfg(test)]
use crate::adapters::claude_desktop::DesktopRecord;
#[cfg(test)]
use crate::adapters::claude_sessions::ClaudeSession;

use crate::succession::{
    created_at_key, plan_successions, rows_of, Plan, Row, EVENT_REFUSED, EVENT_SUCCEEDED,
};

/// The tick: plan over every team's rows (so a target two teams' rows
/// converge on is refused for both), commit and report only *team*'s.
pub(super) fn reconcile_successions(workspace: &str, team: &str) {
    let entries = crate::registry::list_entries();
    let Some(own) = entries
        .iter()
        .find(|e| e.get("team").and_then(Value::as_str) == Some(team))
    else {
        return;
    };
    let created_at = created_at_key(own);
    let rows: Vec<Row> = entries.iter().flat_map(rows_of).collect();
    if !rows.iter().any(|r| r.team == team) {
        return;
    }
    let live = hooked_cs_list_sessions();
    let plans = plan_successions(&rows, hooked_desktop_record, &live, |sid| {
        crate::registry::member_for_session(sid, None).is_some()
    });
    for plan in plans {
        match plan {
            Plan::Move {
                team: t,
                name,
                from,
                host,
                to,
            } if t == team => {
                let outcome = hooked_commit_succession(team, &name, &from, &host, &to, &created_at)
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
                        ("hostSessionId", Value::from(host.as_str())),
                        ("reason", Value::from(outcome)),
                    ],
                );
            }
            Plan::Refused {
                team: t,
                name,
                to,
                reason,
            } if t == team => hooked_notify_debug_emit(
                workspace,
                EVENT_REFUSED,
                &[
                    ("team", Value::from(team)),
                    ("member", Value::from(name.as_str())),
                    ("to", Value::from(to.as_str())),
                    ("reason", Value::from(reason)),
                ],
            ),
            _ => {} // another team's row: its own hived reports it
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
            team: "honey".to_string(),
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
                team: "honey".to_string(),
                name: "orch".to_string(),
                from: "A".to_string(),
                host: "local_h".to_string(),
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
                team: "honey".to_string(),
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
                team: "honey".to_string(),
                name: "m1".to_string(),
                to: "C".to_string(),
                reason: "target_taken",
            }]
        );
    }

    #[test]
    fn test_plan_refuses_the_previous_session_while_it_is_live() {
        let plans = plan_successions(
            &[row("orch", "A", "local_h")],
            |_| Some(rec("C", &["A"])),
            &[live("A"), live("C")],
            |_| false,
        );
        assert!(matches!(
            &plans[..],
            [Plan::Refused {
                reason: "old_still_live",
                ..
            }]
        ));
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
    #[test]
    fn test_reconcile_refuses_a_target_two_teams_rows_converge_on() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::testenv::iso(tmp.path());
        record_team(
            "honey",
            "/ws",
            "1.0",
            &[m(&[
                ("name", "orch"),
                ("cli", "claude"),
                ("sessionId", "A"),
                (HOST_SESSION_FIELD, "local_h"),
            ])],
            "",
        )
        .unwrap();
        record_team(
            "comb",
            "/ws2",
            "2.0",
            &[m(&[
                ("name", "ant"),
                ("cli", "claude"),
                ("sessionId", "B"),
                (HOST_SESSION_FIELD, "local_h"),
            ])],
            "",
        )
        .unwrap();
        type Events = Arc<Mutex<Vec<(String, Map<String, Value>)>>>;
        let events: Events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let hook = testhook::Hook {
            cs_list_sessions: Some(Arc::new(|| vec![live("C")])),
            desktop_record: Some(Arc::new(|h: &str| {
                (h == "local_h").then(|| rec("C", &["A", "B"]))
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
        reconcile_successions("/ws2", "comb");
        assert_eq!(
            crate::registry::load("honey").unwrap()["members"][0]["sessionId"],
            "A"
        );
        assert_eq!(
            crate::registry::load("comb").unwrap()["members"][0]["sessionId"],
            "B"
        );
        let got = events.lock().unwrap();
        assert_eq!(got.len(), 2);
        assert!(got
            .iter()
            .all(|(e, f)| e == EVENT_REFUSED && f["reason"] == "converge"));
        assert_eq!(got[0].1["team"], "honey");
        assert_eq!(got[1].1["team"], "comb");
    }
    #[test]
    fn test_cli_and_hived_succession_share_one_registry_cas() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = crate::testenv::iso(tmp.path());
        env.set("HOME", tmp.path());
        env.set("CLAUDE_HOME", tmp.path().join(".claude"));
        env.set("CLAUDE_CODE_HOST_SESSION_ID", "local_h");
        env.set("CLAUDE_CODE_MESSAGING_SOCKET", tmp.path().join("new.sock"));
        let workspace = tmp.path().join("workspace");
        record_team(
            "honey",
            workspace.to_str().unwrap(),
            "1.0",
            &[m(&[
                ("name", "orch"),
                ("cli", "claude"),
                ("sessionId", "A"),
                (HOST_SESSION_FIELD, "local_h"),
            ])],
            "",
        )
        .unwrap();
        let sessions = tmp.path().join(".claude/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("new.json"),
            json!({
                "name":"new", "pid":std::process::id(), "kind":"interactive",
                "entrypoint":"claude-desktop", "messagingSocketPath":tmp.path().join("new.sock"),
                "sessionId":"C",
            })
            .to_string(),
        )
        .unwrap();
        let records = tmp
            .path()
            .join("Library/Application Support/Claude/claude-code-sessions/account/org");
        std::fs::create_dir_all(&records).unwrap();
        std::fs::write(
            records.join("local_h.json"),
            json!({
                "cliSessionId":"C", "priorCliSessionIds":["A"],
            })
            .to_string(),
        )
        .unwrap();
        let (arrived, ready) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let wait = Mutex::new(wait);
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let captured = outcomes.clone();
        let _guard = testhook::install(testhook::Hook {
            cs_list_sessions: Some(Arc::new(|| vec![live("C")])),
            desktop_record: Some(Arc::new(|_| Some(rec("C", &["A"])))),
            commit_succession: Some(Arc::new(move |team, name, old, host, new, created| {
                arrived.send(()).unwrap();
                wait.lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                let outcome =
                    crate::registry::commit_succession(team, name, old, host, new, created)?;
                captured.lock().unwrap().push(outcome);
                Ok(outcome)
            })),
            notify_debug_emit: Some(Arc::new(crate::notify_debug::emit)),
            ..testhook::Hook::default()
        });
        let ws = workspace.clone();
        let tick = std::thread::spawn(move || reconcile_successions(ws.to_str().unwrap(), "honey"));
        ready
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(crate::identity::default_team().as_deref(), Some("honey"));
        assert_eq!(crate::identity::default_agent().as_deref(), Some("orch"));
        release.send(()).unwrap();
        tick.join().unwrap();
        assert_eq!(*outcomes.lock().unwrap(), vec!["taken"]);
        assert_eq!(
            crate::registry::load("honey").unwrap()["members"][0]["sessionId"],
            "C"
        );
        let log =
            std::fs::read_to_string(crate::notify_debug::log_path(workspace.to_str().unwrap()))
                .unwrap();
        let events: Vec<Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let success: Vec<_> = events
            .iter()
            .filter(|event| event["event"] == EVENT_SUCCEEDED)
            .collect();
        assert_eq!(success.len(), 1);
        assert_eq!(success[0]["via"], "cli");
    }
}
