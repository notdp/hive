use anyhow::{anyhow, Result};
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
    let result = ok_or_fail(_inject_report(&t, agent_name, text));
    println!("{}", json_pretty(&Value::Object(result)));
}

/// Type *text* into the member's composer and describe the delivery.
///
/// Documented low-level bypass: raw composer keystrokes for every CLI, so
/// delivery paths (channel/RPC) can be debugged from outside themselves.
pub(crate) fn _inject_report(t: &Team, agent_name: &str, text: &str) -> Result<Map<String, Value>> {
    let agent = t
        .get(agent_name)
        .map_err(|_| anyhow!("member '{agent_name}' not found in team '{}'", t.name))?;
    crate::agent::_submit_interactive_text(&agent.pane_id, text, &agent.cli)?;
    let mut result = Map::new();
    result.insert("member".to_string(), Value::String(agent_name.to_string()));
    result.insert("action".to_string(), Value::String("inject".to_string()));
    result.insert("pane".to_string(), Value::String(agent.pane_id.clone()));
    result.insert("success".to_string(), Value::Bool(true));
    Ok(result)
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

/// Title + tags + context + viewer launcher for one member's display pane.
pub(crate) fn _bind_member_viewer(pane: &str, member: &Map<String, Value>, team: &str, ws: &str) {
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

/// A member a pane can ride: engine identity recorded, on a CLI
/// `_attach_launcher` has a resume form for.
fn _attachable(member: &Map<String, Value>) -> bool {
    truthy(member.get("sessionId")) && _attach_launcher(&map_str(member, "cli"), "").is_some()
}

fn _entry_members(entry: &Map<String, Value>) -> Vec<Map<String, Value>> {
    entry
        .get("members")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.as_object().cloned())
                .collect()
        })
        .unwrap_or_default()
}

/// Roster members an existing window should gain panes for: not rendered
/// yet and attachable — in roster order.
pub(super) fn _members_to_backfill(
    rendered: &std::collections::HashSet<String>,
    members: Vec<Map<String, Value>>,
) -> Vec<Map<String, Value>> {
    _sorted_member_rows(members)
        .into_iter()
        .filter(|member| !rendered.contains(&map_str(member, "name")) && _attachable(member))
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
    let mut added = Vec::new();
    for member in _members_to_backfill(&rendered, _entry_members(entry)) {
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

/// Session geometry for a team session hive builds itself.
const _TEAM_SESSION_COLS: u32 = 220;
const _TEAM_SESSION_ROWS: u32 = 60;

/// Marks a window hive built itself (as opposed to one a human's session
/// lent the team): `hive delete` closes only these.
fn _mark_hive_built(window: &str) {
    tmux::set_window_option(window, "@hive-built", "1");
}

/// The team's window in the session named after it: a fresh detached
/// session when none exists, a new window in it otherwise. Returns
/// (window target, first pane id, created_session).
pub fn _new_team_session_window(team: &str) -> Result<(String, String, bool)> {
    // `=` pins the exact name: a bare `-t <team>` falls back to prefix
    // matching and would put the window into a stranger's `<team>-x`.
    let exact = format!("={team}");
    if tmux::has_session(&exact) {
        // new_window forces "<team>:" so a numeric name is a session, not an index
        let (window, pane) = tmux::new_window(&exact, team, None, true)?;
        _mark_hive_built(&window);
        return Ok((window, pane, false));
    }
    let pane = tmux::new_session(team, _TEAM_SESSION_COLS, _TEAM_SESSION_ROWS)?;
    // Never fall back to "<team>:" here — that is a session target, not a
    // window, and the first window's index follows the user's base-index.
    let window = tmux::get_pane_window_target(&pane)
        .filter(|w| !w.is_empty())
        .ok_or_else(|| anyhow!("tmux did not report the window of pane {pane}"))?;
    tmux::rename_window(&window, team);
    _mark_hive_built(&window);
    Ok((window, pane, true))
}

/// Where a team window goes for the caller: inside tmux the caller's own
/// session, outside tmux the team session. Returns (window target, first
/// pane id).
fn _team_window_for_caller(team: &str, anchor_cwd: &str) -> (String, String) {
    if !tmux::is_inside_tmux() {
        let (window, pane, _) = ok_or_fail(_new_team_session_window(team));
        return (window, pane);
    }
    let session_name = tmux::get_current_session_name()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "hive".to_string());
    if !tmux::has_session(&session_name) {
        let _ = tmux::new_session(&session_name, _TEAM_SESSION_COLS, _TEAM_SESSION_ROWS);
    }
    let (window, first_pane) =
        tmux::new_window(&session_name, team, Some(anchor_cwd), true).unwrap_or_default();
    if window.is_empty() || first_pane.is_empty() {
        fail("failed to create a window for the team");
    }
    _mark_hive_built(&window);
    (window, first_pane)
}

/// Build a window for the team: one attach pane per member, tiled. A team
/// with no attachable member still gets its window (the first pane stays a
/// shell).
///
/// Returns (window_target, attached_member_names, skipped_member_names).
fn _materialize_team_display(entry: &Map<String, Value>) -> (String, Vec<String>, Vec<String>) {
    let team = map_str(entry, "team");
    let ws = map_str(entry, "workspace");
    let members = _sorted_member_rows(_entry_members(entry));
    let attachable_idx: Vec<usize> = members
        .iter()
        .enumerate()
        .filter(|(_, member)| _attachable(member))
        .map(|(index, _)| index)
        .collect();
    let mut skipped: Vec<String> = members
        .iter()
        .enumerate()
        .filter(|(index, _)| !attachable_idx.contains(index))
        .map(|(_, member)| map_str(member, "name"))
        .collect();

    let anchor_cwd = attachable_idx
        .first()
        .map(|index| map_str(&members[*index], "cwd"))
        .filter(|cwd| !cwd.is_empty())
        .unwrap_or_else(getcwd);
    let (window, first_pane) = _team_window_for_caller(&team, &anchor_cwd);

    tmux::configure_hive_window(&window);
    tmux::set_window_option(&window, "@hive-team", &team);
    tmux::set_window_option(&window, "@hive-workspace", &ws);
    tmux::set_window_option(&window, "@hive-created", &map_str(entry, "createdAt"));

    let mut attached: Vec<String> = Vec::new();
    let mut prev_pane = first_pane.clone();
    for (i, index) in attachable_idx.iter().enumerate() {
        let member = &members[*index];
        let name = map_str(member, "name");
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

/// The registry entry for *team_name*, or the `hive ls` refusal.
pub(super) fn _team_entry(team_name: &str) -> Result<Map<String, Value>, String> {
    crate::registry::load(team_name)
        .ok_or_else(|| format!("team '{team_name}' not found (see `hive ls`)"))
}

fn _team_window(team_name: &str) -> String {
    crate::team::_find_team_window(team_name, "")
        .map(|(window, _)| window)
        .unwrap_or_default()
}

/// The team's display, made whole: rebuilt when the window is gone,
/// backfilled with a pane per roster member it does not show yet. Returns
/// (window, built).
pub(crate) fn _ensure_team_display(entry: &Map<String, Value>) -> (String, bool) {
    let team = map_str(entry, "team");
    let window = _team_window(&team);
    if window.is_empty() {
        let (window, _attached, skipped) = _materialize_team_display(entry);
        for name in skipped {
            eprintln!("! {name}: no attachable engine identity — no pane");
        }
        return (window, true);
    }
    for name in _backfill_missing_member_panes(&window, entry) {
        eprintln!("+ {name}: pane added to the existing window");
    }
    (window, false)
}

/// The jump attach ends on. Inside tmux, `switch-client` moves *this*
/// client — `select-window` would only retarget the window's own session and
/// leave a client attached elsewhere untouched.
fn _jump_to_window(window: &str, verdict: &str) {
    if tmux::is_inside_tmux() {
        tmux::switch_client(window);
        println!("{verdict} {window}");
        return;
    }
    let session = match window.split_once(':') {
        Some((session, _)) => session.to_string(),
        None => window.to_string(),
    };
    ok_or_fail(tmux::exec_attach(&session, window));
}

pub fn attach_cmd(team_name: &str) {
    let entry = match _team_entry(team_name) {
        Ok(entry) => entry,
        Err(message) => fail(&message),
    };
    let (window, built) = _ensure_team_display(&entry);
    let ws = map_str(&entry, "workspace");
    if !ws.is_empty() {
        if let Ok(mut t) = Team::load(team_name, "") {
            let _ = _ensure_team_hived(&mut t, &ws);
        }
    }
    _jump_to_window(&window, if built { "built" } else { "found" });
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
    println!("{}", ok_or_fail(_capture_text(&t, member_name, lines)));
}

/// The last *lines* of the member's own pane (the pane its roster row
/// resolved to), or the not-found refusal.
pub(crate) fn _capture_text(t: &Team, member_name: &str, lines: i64) -> Result<String> {
    let agent = t
        .get(member_name)
        .map_err(|_| anyhow!("member '{member_name}' not found in team '{}'", t.name))?;
    agent.capture(lines.max(0) as u32)
}
