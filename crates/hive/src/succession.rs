//! Shared desktop-session succession decisions for the hived and CLI identity.
//! Observations enter as values; only the callers commit roster changes.

use serde_json::Value;

use crate::adapters::claude_desktop::DesktopRecord;
use crate::adapters::claude_sessions::ClaudeSession;

pub(crate) const EVENT_SUCCEEDED: &str = "member.session_succeeded";
pub(crate) const EVENT_REFUSED: &str = "member.session_refused";

/// A roster row this tick considers: a claude member with a host session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub team: String,
    pub name: String,
    pub session_id: String,
    pub host_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    Move {
        team: String,
        name: String,
        from: String,
        host: String,
        to: String,
    },
    Refused {
        team: String,
        name: String,
        to: String,
        reason: &'static str,
    },
}

/// What each row's desktop record says, against the live sessions and the
/// sessions members already hold. *rows* is every team's, so two rows of
/// different teams resolving to one session are seen converging. Rows
/// whose conversation has not moved, or whose record is unknown, produce
/// nothing.
pub(crate) fn plan_successions(
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
        let (team, name) = (row.team.clone(), row.name.clone());
        if live.iter().any(|s| s.session_id == row.session_id) {
            plans.push(Plan::Refused {
                team,
                name,
                to,
                reason: "old_still_live",
            });
        } else if !live.iter().any(|s| s.session_id == to) {
            plans.push(Plan::Refused {
                team,
                name,
                to,
                reason: "target_not_live",
            });
        } else if taken(&to) {
            plans.push(Plan::Refused {
                team,
                name,
                to,
                reason: "target_taken",
            });
        } else {
            plans.push(Plan::Move {
                team,
                name,
                from: row.session_id.clone(),
                host: row.host_session_id.clone(),
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
            Plan::Move { team, name, to, .. }
                if targets.iter().filter(|t| **t == to).count() > 1 =>
            {
                Plan::Refused {
                    team,
                    name,
                    to,
                    reason: "converge",
                }
            }
            other => other,
        })
        .collect()
}

pub(crate) fn created_at_key(entry: &serde_json::Map<String, Value>) -> String {
    match entry.get("createdAt") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

pub(crate) fn rows_of(entry: &serde_json::Map<String, Value>) -> Vec<Row> {
    let team = entry
        .get("team")
        .and_then(Value::as_str)
        .unwrap_or_default();
    entry
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
                    (!team.is_empty() && !host.is_empty() && !sid.is_empty() && !name.is_empty())
                        .then(|| Row {
                            team: team.to_string(),
                            name: name.to_string(),
                            session_id: sid.to_string(),
                            host_session_id: host.to_string(),
                        })
                })
                .collect()
        })
        .unwrap_or_default()
}
