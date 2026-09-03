use serde_json::{json, Map, Value};

use super::*;
use crate::team::Team;
use crate::tmux;

// ---------------------------------------------------------------------------
// inject / compact
// ---------------------------------------------------------------------------

pub fn inject_cmd(agent_name: &str, text: &str) {
    let (_, t) = ok_or_fail(resolve_scoped_team(None, true));
    let t = t.expect("required resolve returned no team");
    let agent = match t.get(agent_name) {
        Ok(agent) => agent,
        Err(_) => fail(&format!(
            "member '{agent_name}' not found in team '{}'",
            t.name
        )),
    };
    // Documented low-level bypass: raw composer keystrokes for every CLI, so
    // delivery paths (channel/RPC) can be debugged from outside themselves.
    if let Err(exc) = crate::agent::_submit_interactive_text(&agent.pane_id, text, &agent.cli) {
        fail(&exc.to_string());
    }
    let mut result = Map::new();
    result.insert("member".to_string(), Value::String(agent_name.to_string()));
    result.insert("action".to_string(), Value::String("inject".to_string()));
    result.insert("pane".to_string(), Value::String(agent.pane_id.clone()));
    result.insert("success".to_string(), Value::Bool(true));
    println!("{}", json_pretty(&Value::Object(result)));
}

/// Run `/compact` on the literal pane. Returns the compaction status.
fn _compact_target(target: &PaneTarget) -> String {
    if target.cli == "codex" || target.cli == "grok" {
        // Daemon-backed CLIs: an idle agent compacts via the dedicated RPC;
        // when busy we keystroke `/compact` into the CLI's own TUI so it can
        // refuse visibly instead of a silent background compaction.
        let status = if target.cli == "codex" {
            crate::adapters::codex_app_server::compact_pane(&target.pane_id)
        } else {
            crate::adapters::grok_leader::compact_pane(&target.pane_id)
        };
        if status != "compacted" {
            ok_or_fail(crate::agent::_submit_interactive_text(
                &target.pane_id,
                "/compact",
                &target.cli,
            ));
        }
        return status.to_string();
    }
    // claude (and embedded codex without a daemon): `/compact` is a TUI
    // slash command, so it must go through the composer.
    if let Err(exc) =
        crate::agent::_submit_interactive_text(&target.pane_id, "/compact", &target.cli)
    {
        fail(&exc.to_string());
    }
    "compacted".to_string()
}

pub fn compact_cmd(pane_id: &str) {
    // Resolve the pane straight from its tmux options — never re-resolve
    // through Team state (the cross-window same-name bug PR #8 fixed).
    let target = _resolve_pane_target(pane_id);
    let status = _compact_target(&target);
    let mut result = Map::new();
    result.insert(
        "member".to_string(),
        Value::String(target.member_label.clone()),
    );
    result.insert("action".to_string(), Value::String("compact".to_string()));
    result.insert("pane".to_string(), Value::String(target.pane_id.clone()));
    result.insert("status".to_string(), Value::String(status.clone()));
    result.insert("success".to_string(), Value::Bool(status == "compacted"));
    if !target.is_team_bound {
        // Pane-only compact has no team identity; `member` is the pane id.
        result.insert("team".to_string(), Value::Null);
    }
    println!("{}", json_pretty(&Value::Object(result)));
}

// ---------------------------------------------------------------------------
// layout
// ---------------------------------------------------------------------------

pub fn layout_cmd(preset: &str) {
    let (_, t) = ok_or_fail(resolve_scoped_team(None, true));
    let t = t.expect("required resolve returned no team");
    let window_target = if !t.tmux_window.is_empty() {
        t.tmux_window.clone()
    } else {
        tmux::get_current_window_target().unwrap_or_default()
    };
    if window_target.is_empty() {
        fail("Cannot determine tmux window target");
    }
    if preset == "auto" {
        match crate::layout::apply_adaptive(&window_target) {
            None => println!(
                "{}",
                py_dumps(
                    &json!({"layout": "", "window": window_target, "reason": "no-op"}),
                    true,
                    None,
                    false
                )
            ),
            Some(choice) => println!(
                "{}",
                py_dumps(
                    &json!({
                        "layout": choice.preset,
                        "orientation": choice.orientation,
                        "window": window_target,
                    }),
                    true,
                    None,
                    false
                )
            ),
        }
        return;
    }
    if preset == "main-vertical" || preset == "main-horizontal" {
        let dim = if preset == "main-vertical" {
            "main-pane-width"
        } else {
            "main-pane-height"
        };
        tmux::set_window_option(&window_target, dim, "50%");
    }
    tmux::select_layout(&window_target, preset);
    println!(
        "{}",
        py_dumps(
            &json!({"layout": preset, "window": window_target}),
            true,
            None,
            false
        )
    );
}

// ---------------------------------------------------------------------------
// attach
// ---------------------------------------------------------------------------

fn _attach_launcher(cli_name: &str, quoted_sid: &str) -> Option<String> {
    match cli_name {
        "claude" => Some(format!("hive claude --resume {quoted_sid}")),
        "codex" => Some(format!("hive codex resume {quoted_sid}")),
        "grok" => Some(format!("hive grok --resume {quoted_sid}")),
        _ => None,
    }
}

fn _member_attach_command(cli_name: &str, session_id: &str, cwd: &str) -> String {
    let quoted_sid = shlex_quote(session_id);
    let launch = _attach_launcher(cli_name, &quoted_sid).expect("attachable cli");
    let cwd = if cwd.is_empty() {
        getcwd()
    } else {
        cwd.to_string()
    };
    if cli_name == "claude" && crate::adapters::claude_bg::job_row(session_id, "claude").is_none() {
        // An interactive session (desktop ccd, joined session) must NOT be
        // resumed — the launcher's resume lane would mint a forked bg job
        // that steals the member's deliveries. Render the transcript
        // read-only instead — and without the resume-hint tail, which would
        // otherwise re-adopt any same-named job on viewer exit.
        return format!("cd {} && hive view {quoted_sid}", shlex_quote(&cwd));
    }
    format!(
        "cd {} && {launch}; hive resume-hint {cli_name} 2>/dev/null || true",
        shlex_quote(&cwd)
    )
}

/// Build a window for the team: one attach pane per member, tiled.
///
/// Returns (window_target, attached_member_names, skipped_member_names).
/// Title + tags + context + viewer launcher for one member's display pane.
fn _bind_member_viewer(pane: &str, member: &Map<String, Value>, team: &str, ws: &str) {
    let name = map_str(member, "name");
    let cli_name = map_str(member, "cli");
    let cwd = map_str(member, "cwd");
    tmux::set_pane_title(pane, &format!("[{name}]"));
    tmux::tag_pane(pane, "agent", &name, team, &cli_name, "");
    if !ws.is_empty() {
        let _ = crate::context::save_context_for_pane(pane, team, ws, &name);
    }
    ok_or_fail(tmux::send_keys(
        pane,
        &_member_attach_command(&cli_name, &map_str(member, "sessionId"), &cwd),
        true,
    ));
}

/// Roster members an existing window should gain panes for: not rendered
/// yet, engine identity recorded, and an attachable CLI — in roster order.
pub(super) fn _members_to_backfill(
    rendered: &std::collections::HashSet<String>,
    members: Vec<Map<String, Value>>,
) -> Vec<Map<String, Value>> {
    _sorted_member_rows(members)
        .into_iter()
        .filter(|member| {
            let name = map_str(member, "name");
            !rendered.contains(&name)
                && truthy(member.get("sessionId"))
                && matches!(map_str(member, "cli").as_str(), "claude" | "codex" | "grok")
        })
        .collect()
}

/// Split panes into an existing team window for roster members it does not
/// render yet (a member spawned after the window was built).
fn _backfill_missing_member_panes(window: &str, entry: &Map<String, Value>) -> Vec<String> {
    let team = map_str(entry, "team");
    let ws = map_str(entry, "workspace");
    let rendered: std::collections::HashSet<String> = tmux::list_panes_full(window)
        .into_iter()
        .filter(|p| p.role == "agent" && !p.agent.is_empty())
        .map(|p| p.agent)
        .collect();
    let mut prev_pane = tmux::list_panes(window)
        .into_iter()
        .last()
        .unwrap_or_default();
    if prev_pane.is_empty() {
        return Vec::new();
    }
    let members: Vec<Map<String, Value>> = entry
        .get("members")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.as_object().cloned())
                .collect()
        })
        .unwrap_or_default();
    let mut added = Vec::new();
    for member in _members_to_backfill(&rendered, members) {
        let name = map_str(&member, "name");
        let cwd = map_str(&member, "cwd");
        let count = tmux::list_panes(window).len();
        let split = tmux::split_window(
            &prev_pane,
            crate::layout::split_horizontal(window, count + 1),
            None,
            true,
            if cwd.is_empty() { None } else { Some(&cwd) },
        )
        .unwrap_or_default();
        if split.is_empty() {
            continue;
        }
        _bind_member_viewer(&split, &member, &team, &ws);
        added.push(name);
        prev_pane = split;
    }
    if !added.is_empty() {
        let _ = crate::layout::apply_adaptive(window);
    }
    added
}

fn _materialize_team_display(entry: &Map<String, Value>) -> (String, Vec<String>, Vec<String>) {
    let team = map_str(entry, "team");
    let ws = map_str(entry, "workspace");
    let members: Vec<Map<String, Value>> = entry
        .get("members")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.as_object().cloned())
                .collect()
        })
        .unwrap_or_default();
    let members = _sorted_member_rows(members);
    let attachable_idx: Vec<usize> = members
        .iter()
        .enumerate()
        .filter(|(_, member)| {
            truthy(member.get("sessionId"))
                && matches!(map_str(member, "cli").as_str(), "claude" | "codex" | "grok")
        })
        .map(|(index, _)| index)
        .collect();
    let mut skipped: Vec<String> = members
        .iter()
        .enumerate()
        .filter(|(index, _)| !attachable_idx.contains(index))
        .map(|(_, member)| map_str(member, "name"))
        .collect();
    if attachable_idx.is_empty() {
        fail(&format!(
            "team '{team}' has no attachable members (no recorded engine identity)"
        ));
    }

    let session_name = tmux::get_current_session_name()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "hive".to_string());
    if !tmux::has_session(&session_name) {
        let _ = tmux::new_session(&session_name, 200, 50);
    }
    let anchor_cwd = {
        let first = &members[attachable_idx[0]];
        let cwd = map_str(first, "cwd");
        if cwd.is_empty() {
            getcwd()
        } else {
            cwd
        }
    };
    let (window, first_pane) =
        tmux::new_window(&session_name, &team, Some(&anchor_cwd), true).unwrap_or_default();
    if window.is_empty() || first_pane.is_empty() {
        fail("failed to create a window for the team");
    }

    tmux::configure_hive_window(&window);
    tmux::set_window_option(&window, "@hive-team", &team);
    tmux::set_window_option(&window, "@hive-workspace", &ws);
    tmux::set_window_option(&window, "@hive-created", &map_str(entry, "createdAt"));

    let mut attached: Vec<String> = Vec::new();
    let mut prev_pane = first_pane.clone();
    for (i, index) in attachable_idx.iter().enumerate() {
        let member = &members[*index];
        let name = map_str(member, "name");
        let cli_name = map_str(member, "cli");
        let cwd = map_str(member, "cwd");
        let pane = if i == 0 {
            first_pane.clone()
        } else {
            let split = ok_or_fail(tmux::split_window(
                &prev_pane,
                crate::layout::split_horizontal(&window, i + 1),
                None,
                true,
                if cwd.is_empty() { None } else { Some(&cwd) },
            ));
            if split.is_empty() {
                skipped.push(name);
                continue;
            }
            split
        };
        _bind_member_viewer(&pane, member, &team, &ws);
        attached.push(name);
        prev_pane = pane;
    }

    let _ = crate::layout::apply_adaptive(&window);
    let _ = crate::registry::set_display(&team, &tmux::get_window_id(&window).unwrap_or_default());
    (window, attached, skipped)
}

pub fn attach_cmd(team_name: &str) {
    let entry = match crate::registry::load(team_name) {
        Some(entry) => entry,
        None => fail(&format!("team '{team_name}' not found (see `hive ls`)")),
    };

    let mut window = crate::team::_find_team_window(team_name, "")
        .map(|(window, _)| window)
        .unwrap_or_default();
    let mut built = false;
    if window.is_empty() {
        let (materialized, _attached, skipped) = _materialize_team_display(&entry);
        window = materialized;
        built = true;
        for name in skipped {
            eprintln!("! {name}: no attachable engine identity — not rendered");
        }
    } else {
        // A member spawned after the window was built has no pane yet —
        // fold it into the existing display instead of leaving it headless.
        for name in _backfill_missing_member_panes(&window, &entry) {
            eprintln!("+ {name}: rendered into the existing window");
        }
    }
    let ws = map_str(&entry, "workspace");
    if !ws.is_empty() {
        if let Ok(mut t) = Team::load(team_name, "") {
            let _ = _ensure_team_hived(&mut t, &ws);
        }
    }

    if tmux::is_inside_tmux() {
        tmux::select_window(&window);
        println!("{} {window}", if built { "built" } else { "found" });
        return;
    }
    let session = match window.split_once(':') {
        Some((session, _)) => session.to_string(),
        None => window.clone(),
    };
    ok_or_fail(tmux::exec_attach(&session, &window));
}

// ---------------------------------------------------------------------------
// thread / capture
// ---------------------------------------------------------------------------

pub fn thread(message_id: &str) {
    let (_, t) = ok_or_fail(resolve_scoped_team(None, true));
    let mut t = t.expect("required resolve returned no team");
    let ws = ok_or_fail(resolve_workspace(Some(&t), true));
    let _ = _ensure_team_hived(&mut t, &ws);
    let payload = crate::hived::request_thread(&ws, message_id);
    let mut payload = match payload {
        Some(payload) if !payload.is_empty() => payload,
        _ => fail(&crate::devlog::hived_unavailable_message(
            std::path::Path::new(&ws),
        )),
    };
    if payload.get("ok") == Some(&Value::Bool(false)) {
        let error = match payload.get("error") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => "thread lookup failed".to_string(),
        };
        fail(&error);
    }
    payload.shift_remove("ok");
    println!("{}", json_pretty(&Value::Object(payload)));
}

pub fn capture(member_name: &str, lines: i64) {
    let (_, t) = ok_or_fail(resolve_scoped_team(None, true));
    let t = t.expect("required resolve returned no team");
    match t.get(member_name) {
        Ok(agent) => {
            let text = ok_or_fail(agent.capture(lines.max(0) as u32));
            println!("{text}");
        }
        Err(_) => fail(&format!(
            "member '{member_name}' not found in team '{}'",
            t.name
        )),
    }
}
