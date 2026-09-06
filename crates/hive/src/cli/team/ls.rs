//! `hive ls`: every registry entry with its display state (live, detached,
//! unknown when tmux does not answer), plus the teams alive only in tmux.

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::super::util::json_pretty;
use crate::json_fields::{is_set, map_str};
use crate::team::sorted_member_rows;
use crate::tmux;

/// Registry entries with their current display state.
fn build_ls_payload() -> Map<String, Value> {
    let (panes, pane_status) = tmux::list_panes_all_status();
    let (windows, win_status) = tmux::list_team_windows_status();
    let tmux_status = if pane_status == "ok" && win_status == "ok" {
        "ok"
    } else if pane_status == "unknown" || win_status == "unknown" {
        "unknown"
    } else {
        "no-server"
    };

    // team -> agent -> pane, insertion ordered.
    let mut live_members: Vec<(String, Vec<(String, tmux::PaneInfo)>)> = Vec::new();
    let mut win_by_team: HashMap<String, tmux::TeamWindow> = HashMap::new();
    if tmux_status == "ok" {
        for p in panes.unwrap_or_default() {
            if !p.team.is_empty() && !p.agent.is_empty() {
                let team_idx = match live_members.iter().position(|(team, _)| *team == p.team) {
                    Some(idx) => idx,
                    None => {
                        live_members.push((p.team.clone(), Vec::new()));
                        live_members.len() - 1
                    }
                };
                let team_slot = &mut live_members[team_idx].1;
                match team_slot.iter().position(|(agent, _)| *agent == p.agent) {
                    Some(idx) => team_slot[idx].1 = p.clone(),
                    None => team_slot.push((p.agent.clone(), p.clone())),
                }
            }
        }
        for w in windows.unwrap_or_default() {
            win_by_team.entry(w.team.clone()).or_insert(w);
        }
    }

    let live_for = |team: &str| -> Option<&Vec<(String, tmux::PaneInfo)>> {
        live_members
            .iter()
            .find(|(name, _)| name == team)
            .map(|(_, members)| members)
    };

    let mut teams: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for entry in crate::registry::list_entries() {
        let team_name = map_str(&entry, "team");
        seen.insert(team_name.clone());
        if is_set(entry.get("corrupt")) {
            let mut row = Map::new();
            row.insert("team".to_string(), Value::String(team_name));
            row.insert("state".to_string(), Value::String("corrupt".to_string()));
            teams.push(Value::Object(row));
            continue;
        }
        let member_rows: Vec<Map<String, Value>> = entry
            .get("members")
            .and_then(Value::as_array)
            .map(|members| {
                members
                    .iter()
                    .filter_map(Value::as_object)
                    .map(|m| {
                        let mut row = Map::new();
                        row.insert(
                            "name".to_string(),
                            m.get("name")
                                .cloned()
                                .unwrap_or(Value::String(String::new())),
                        );
                        row.insert(
                            "cli".to_string(),
                            m.get("cli")
                                .cloned()
                                .unwrap_or(Value::String(String::new())),
                        );
                        row.insert(
                            "model".to_string(),
                            m.get("model")
                                .cloned()
                                .unwrap_or(Value::String(String::new())),
                        );
                        row.insert(
                            "session".to_string(),
                            Value::Bool(is_set(m.get("sessionId"))),
                        );
                        row
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut row = Map::new();
        row.insert("team".to_string(), Value::String(team_name.clone()));
        row.insert(
            "workspace".to_string(),
            entry
                .get("workspace")
                .cloned()
                .unwrap_or(Value::String(String::new())),
        );
        let mut sorted_rows = sorted_member_rows(member_rows);
        if tmux_status == "unknown" {
            row.insert(
                "members".to_string(),
                Value::Array(sorted_rows.into_iter().map(Value::Object).collect()),
            );
            row.insert("state".to_string(), Value::String("unknown".to_string()));
        } else {
            let live = live_for(&team_name);
            match live {
                Some(live) if !live.is_empty() => {
                    for m in sorted_rows.iter_mut() {
                        let name = map_str(m, "name");
                        m.insert(
                            "live".to_string(),
                            Value::Bool(live.iter().any(|(agent, _)| *agent == name)),
                        );
                    }
                    let missing: Vec<String> = entry
                        .get("members")
                        .and_then(Value::as_array)
                        .map(|members| {
                            members
                                .iter()
                                .filter_map(Value::as_object)
                                .map(|m| map_str(m, "name"))
                                .filter(|name| !live.iter().any(|(agent, _)| agent == name))
                                .collect()
                        })
                        .unwrap_or_default();
                    row.insert(
                        "members".to_string(),
                        Value::Array(sorted_rows.into_iter().map(Value::Object).collect()),
                    );
                    row.insert(
                        "window".to_string(),
                        Value::String(
                            win_by_team
                                .get(&team_name)
                                .map(|w| w.window.clone())
                                .unwrap_or_default(),
                        ),
                    );
                    row.insert(
                        "state".to_string(),
                        Value::String(
                            if missing.is_empty() {
                                "live-complete"
                            } else {
                                "live-incomplete"
                            }
                            .to_string(),
                        ),
                    );
                }
                _ => {
                    // The display is gone; the engines may still be running.
                    row.insert(
                        "members".to_string(),
                        Value::Array(sorted_rows.into_iter().map(Value::Object).collect()),
                    );
                    row.insert("state".to_string(), Value::String("detached".to_string()));
                }
            }
        }
        teams.push(Value::Object(row));
    }

    if tmux_status == "ok" {
        // Teams alive only in tmux (predating the registry writers).
        let mut tmux_only: Vec<&(String, Vec<(String, tmux::PaneInfo)>)> = live_members
            .iter()
            .filter(|(team, _)| !seen.contains(team))
            .collect();
        tmux_only.sort_by(|a, b| a.0.cmp(&b.0));
        for (team_name, members) in tmux_only {
            let win = win_by_team.get(team_name);
            let member_rows: Vec<Map<String, Value>> = members
                .iter()
                .map(|(n, p)| {
                    let mut row = Map::new();
                    row.insert("name".to_string(), Value::String(n.clone()));
                    row.insert("cli".to_string(), Value::String(p.cli.clone()));
                    row.insert("live".to_string(), Value::Bool(true));
                    row
                })
                .collect();
            let mut row = Map::new();
            row.insert("team".to_string(), Value::String(team_name.clone()));
            row.insert(
                "state".to_string(),
                Value::String("live-complete".to_string()),
            );
            row.insert(
                "window".to_string(),
                Value::String(win.map(|w| w.window.clone()).unwrap_or_default()),
            );
            row.insert(
                "workspace".to_string(),
                Value::String(win.map(|w| w.workspace.clone()).unwrap_or_default()),
            );
            row.insert(
                "members".to_string(),
                Value::Array(
                    sorted_member_rows(member_rows)
                        .into_iter()
                        .map(Value::Object)
                        .collect(),
                ),
            );
            teams.push(Value::Object(row));
        }
    }

    let mut payload = Map::new();
    payload.insert("tmux".to_string(), Value::String(tmux_status.to_string()));
    payload.insert("teams".to_string(), Value::Array(teams));
    payload
}

/// List hive teams from the registry, with their display state.
pub(crate) fn ls_cmd(plain: bool) {
    let payload = build_ls_payload();
    if !plain {
        println!("{}", json_pretty(&Value::Object(payload)));
        return;
    }
    for line in format_ls_human(&payload) {
        println!("{line}");
    }
}

fn ls_roster(entry: &Map<String, Value>) -> String {
    entry
        .get("members")
        .and_then(Value::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(Value::as_object)
                .map(|m| {
                    let cli = map_str(m, "cli");
                    if !cli.is_empty() {
                        return cli;
                    }
                    let name = map_str(m, "name");
                    if !name.is_empty() {
                        return name;
                    }
                    "?".to_string()
                })
                .collect::<Vec<_>>()
                .join("+")
        })
        .unwrap_or_default()
}

fn format_ls_human(payload: &Map<String, Value>) -> Vec<String> {
    let teams: Vec<&Map<String, Value>> = payload
        .get("teams")
        .and_then(Value::as_array)
        .map(|teams| teams.iter().filter_map(Value::as_object).collect())
        .unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    if teams.is_empty() {
        lines.push("no hive teams".to_string());
        return lines;
    }
    if map_str(payload, "tmux") == "unknown" {
        lines.push("! tmux did not answer — live/detached state unknown this pass".to_string());
    }

    let state_of = |e: &Map<String, Value>| map_str(e, "state");
    let live: Vec<&Map<String, Value>> = teams
        .iter()
        .copied()
        .filter(|e| {
            let s = state_of(e);
            s == "live-complete" || s == "live-incomplete"
        })
        .collect();
    let detached: Vec<&Map<String, Value>> = teams
        .iter()
        .copied()
        .filter(|e| state_of(e) == "detached")
        .collect();
    let other: Vec<&Map<String, Value>> = teams
        .iter()
        .copied()
        .filter(|e| {
            let s = state_of(e);
            s != "live-complete" && s != "live-incomplete" && s != "detached"
        })
        .collect();

    if !live.is_empty() {
        lines.push("LIVE".to_string());
        let mut live_sorted = live;
        live_sorted.sort_by_key(|e| map_str(e, "window"));
        for e in live_sorted {
            let window = map_str(e, "window");
            let mut row = format!(
                "  {}  {} · {}",
                if window.is_empty() { "?" } else { &window },
                map_str(e, "team"),
                ls_roster(e)
            );
            if state_of(e) == "live-incomplete" {
                let missing: Vec<String> = e
                    .get("members")
                    .and_then(Value::as_array)
                    .map(|members| {
                        members
                            .iter()
                            .filter_map(Value::as_object)
                            .filter(|m| !is_set(m.get("live")))
                            .map(|m| map_str(m, "name"))
                            .collect()
                    })
                    .unwrap_or_default();
                row.push_str(&format!("  ! missing {}", missing.join("+")));
            }
            lines.push(row);
        }
    }
    if !detached.is_empty() {
        if !lines.is_empty() && lines.last().map(String::as_str) != Some("") {
            lines.push(String::new());
        }
        lines.push("DETACHED  — no tmux display".to_string());
        for e in detached {
            lines.push(format!("  {} · {}", map_str(e, "team"), ls_roster(e)));
        }
    }
    if !other.is_empty() {
        if !lines.is_empty() && lines.last().map(String::as_str) != Some("") {
            lines.push(String::new());
        }
        lines.push("OTHER".to_string());
        for e in other {
            let what = if state_of(e) == "corrupt" {
                "unreadable registry entry".to_string()
            } else {
                state_of(e)
            };
            lines.push(format!(
                "  {}  {}  {}",
                map_str(e, "team"),
                what,
                ls_roster(e)
            ));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn as_map(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => panic!("expected object"),
        }
    }

    #[test]
    fn test_format_ls_human_empty_payload() {
        let payload = as_map(json!({"tmux": "ok", "teams": []}));
        assert_eq!(format_ls_human(&payload), vec!["no hive teams"]);
    }

    #[test]
    fn test_format_ls_human_groups_live_detached_and_other() {
        let payload = as_map(json!({
            "tmux": "ok",
            "teams": [
                {
                    "team": "honey",
                    "state": "live-incomplete",
                    "window": "dev:1",
                    "members": [
                        {"name": "orch", "cli": "claude", "live": true},
                        {"name": "dodo", "cli": "codex", "live": false},
                    ],
                },
                {
                    "team": "comb",
                    "state": "detached",
                    "members": [{"name": "orch", "cli": "claude"}],
                },
                {"team": "wasp", "state": "corrupt"},
            ],
        }));
        let lines = format_ls_human(&payload);
        assert_eq!(
            lines,
            vec![
                "LIVE".to_string(),
                "  dev:1  honey · claude+codex  ! missing dodo".to_string(),
                String::new(),
                "DETACHED  — no tmux display".to_string(),
                "  comb · claude".to_string(),
                String::new(),
                "OTHER".to_string(),
                "  wasp  unreadable registry entry  ".to_string(),
            ]
        );
    }

    #[test]
    fn test_format_ls_human_flags_unknown_tmux() {
        let payload = as_map(json!({
            "tmux": "unknown",
            "teams": [{"team": "honey", "state": "unknown", "members": []}],
        }));
        let lines = format_ls_human(&payload);
        assert_eq!(
            lines[0],
            "! tmux did not answer — live/detached state unknown this pass"
        );
        assert!(lines.contains(&"OTHER".to_string()));
        assert!(lines.contains(&"  honey  unknown  ".to_string()));
    }

    #[test]
    fn test_ls_roster_prefers_cli_then_name() {
        let entry = as_map(json!({
            "members": [
                {"name": "orch", "cli": "claude"},
                {"name": "dodo", "cli": ""},
                {"name": "", "cli": ""},
            ]
        }));
        assert_eq!(ls_roster(&entry), "claude+dodo+?");
    }
}
