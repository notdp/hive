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
    let result = ok_or_fail(inject_report(&t, agent_name, text));
    println!("{}", json_pretty(&Value::Object(result)));
}

/// Type *text* into the member's composer and describe the delivery.
///
/// Documented low-level bypass: raw composer keystrokes for every CLI, so
/// delivery paths (channel/RPC) can be debugged from outside themselves.
pub(crate) fn inject_report(t: &Team, agent_name: &str, text: &str) -> Result<Map<String, Value>> {
    let agent = t
        .get(agent_name)
        .map_err(|_| anyhow!("member '{agent_name}' not found in team '{}'", t.name))?;
    crate::agent::submit_interactive_text(&agent.pane_id, text, &agent.cli)?;
    let mut result = Map::new();
    result.insert("member".to_string(), Value::String(agent_name.to_string()));
    result.insert("action".to_string(), Value::String("inject".to_string()));
    result.insert("pane".to_string(), Value::String(agent.pane_id.clone()));
    result.insert("success".to_string(), Value::Bool(true));
    Ok(result)
}

/// Run `/compact` on the literal pane. Returns the compaction status.
fn compact_target(target: &PaneTarget) -> String {
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
            ok_or_fail(crate::agent::submit_interactive_text(
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
        crate::agent::submit_interactive_text(&target.pane_id, "/compact", &target.cli)
    {
        fail(&exc.to_string());
    }
    "compacted".to_string()
}

pub fn compact_cmd(pane_id: &str) {
    // Resolve the pane straight from its tmux options — never re-resolve
    // through Team state (the cross-window same-name bug PR #8 fixed).
    let target = resolve_pane_target(pane_id);
    let status = compact_target(&target);
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

/// `hive layout <preset|auto> [--on-change] [--window TARGET]`. `auto`
/// from a human forces the plan (the "布局拖乱了" repair); `--on-change`
/// is the window hooks' form, which applies only when the plan's key
/// differs from `@hive-layout` and prints nothing (a run-shell job's
/// output would land in a tmux view). `--window` names the window when
/// there is no caller pane (a run-shell job has no TMUX_PANE).
pub fn layout_cmd(preset: &str, on_change: bool, window: &str) {
    let window_target = if !window.is_empty() {
        window.to_string()
    } else {
        let (_, t) = ok_or_fail(resolve_scoped_team(None, true));
        let t = t.expect("required resolve returned no team");
        if !t.tmux_window.is_empty() {
            t.tmux_window.clone()
        } else {
            tmux::get_current_window_target().unwrap_or_default()
        }
    };
    if window_target.is_empty() {
        fail("Cannot determine tmux window target");
    }
    if preset == "auto" {
        let outcome = crate::layout::ensure(&window_target, !on_change);
        if on_change {
            return;
        }
        let plan = outcome.plan();
        println!(
            "{}",
            py_dumps(
                &json!({
                    "layout": plan.map(|p| p.key.as_str()).unwrap_or_default(),
                    "orientation": plan.map(|p| p.orientation).unwrap_or_default(),
                    "window": window_target,
                    "applied": outcome.applied(),
                    "reason": outcome.reason(),
                }),
                true,
                None,
                false
            )
        );
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

fn attach_launcher(cli_name: &str, quoted_sid: &str) -> Option<String> {
    match cli_name {
        "claude" => Some(format!("hive claude --resume {quoted_sid}")),
        "codex" => Some(format!("hive codex resume {quoted_sid}")),
        "grok" => Some(format!("hive grok --resume {quoted_sid}")),
        _ => None,
    }
}

fn member_attach_command(member: &Map<String, Value>, mirrors: bool) -> String {
    let cli_name = map_str(member, "cli");
    let quoted_sid = shlex_quote(&map_str(member, "sessionId"));
    let launch = attach_launcher(&cli_name, &quoted_sid).expect("attachable cli");
    let cwd = map_str(member, "cwd");
    let cwd = if cwd.is_empty() { getcwd() } else { cwd };
    if mirrors {
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

/// A claude member whose sessionId is an interactive session (a creating
/// or joined desktop/ccd session), not a bg job: its pane can only mirror
/// it, never resume it.
fn mirrors_a_session(member: &Map<String, Value>) -> bool {
    map_str(member, "cli") == "claude"
        && crate::adapters::claude_bg::job_row(&map_str(member, "sessionId"), "claude").is_none()
}

/// The window's recorded mirror choice: `@hive-mirror` `on` / `off`
/// (`hive mirror`, or `on` written when a session mirror is built), None
/// when nothing is recorded yet — which reads as open.
fn mirror_preference(window: &str) -> Option<bool> {
    match tmux::get_window_option(window, "hive-mirror").as_deref() {
        Some("on") => Some(true),
        Some("off") => Some(false),
        _ => None,
    }
}

/// The `@hive-role` of the pane *member* gets — `agent` riding its engine,
/// `mirror` for a session mirror — or None when no pane is drawn: a session
/// mirror is withheld while *mirror_pref* (the window's `mirror_preference`,
/// or the `on` a `hive mirror on` is enforcing) is `off`. Decided once per
/// member: the job-ledger probe behind `mirrors_a_session` is a CLI call.
pub(crate) fn pane_role(
    mirror_pref: Option<bool>,
    member: &Map<String, Value>,
) -> Option<&'static str> {
    if !mirrors_a_session(member) {
        return Some("agent");
    }
    if mirror_pref == Some(false) {
        return None;
    }
    Some("mirror")
}

/// A session mirror on screen makes the orch chip appear: the window
/// records `on` unless a choice is already recorded.
fn record_mirror_shown(window: &str) {
    if !window.is_empty() && mirror_preference(window).is_none() {
        tmux::set_window_option(window, "@hive-mirror", "on");
    }
}

/// *member*'s parked mirror pane (`hive mirror off` broke it into a hidden
/// window) joined back as *window*'s first pane, tags and viewer intact —
/// so a rebuilt or healed window never starts a second viewer of one
/// session. None when nothing of *member*'s is parked.
fn join_hidden_mirror(window: &str, team: &str, member: &str) -> Option<String> {
    let hidden = tmux::hidden_mirror_pane(team)?;
    if tmux::get_pane_option(&hidden, "hive-agent").as_deref() != Some(member) {
        return None;
    }
    let first = tmux::list_panes(window).into_iter().next()?;
    join_parked_pane(&hidden, &first);
    record_mirror_shown(window);
    Some(hidden)
}

/// The parked pane *hidden* joined back before *first*. A notify mark it
/// carries is stale: the select hook reconciles only the panes of the
/// window it fires on, and the parked pane sat outside it.
pub(crate) fn join_parked_pane(hidden: &str, first: &str) {
    tmux::join_pane_before(hidden, first);
    tmux::clear_pane_option(hidden, crate::notify_ui::PANE_NOTIFY_ACTIVE_KEY);
}

/// Title + tags + context + viewer launcher for one member's display pane,
/// *role* being what `pane_role` decided for it.
pub(crate) fn bind_member_viewer(
    pane: &str,
    member: &Map<String, Value>,
    team: &str,
    ws: &str,
    role: &str,
) {
    let name = map_str(member, "name");
    let cli_name = map_str(member, "cli");
    tmux::set_pane_title(pane, &format!("[{name}]"));
    tmux::tag_pane(pane, role, &name, team, &cli_name, "");
    if role == "mirror" {
        record_mirror_shown(&tmux::get_pane_window_target(pane).unwrap_or_default());
    }
    if !ws.is_empty() {
        let _ = crate::context::save_context_for_pane(pane, team, ws, &name);
    }
    ok_or_fail(tmux::send_keys(
        pane,
        &member_attach_command(member, role == "mirror"),
        true,
    ));
}

/// A member a pane can ride: engine identity recorded, on a CLI
/// `attach_launcher` has a resume form for.
fn attachable(member: &Map<String, Value>) -> bool {
    truthy(member.get("sessionId")) && attach_launcher(&map_str(member, "cli"), "").is_some()
}

fn entry_members(entry: &Map<String, Value>) -> Vec<Map<String, Value>> {
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
pub(crate) fn members_to_backfill(
    rendered: &std::collections::HashSet<String>,
    members: Vec<Map<String, Value>>,
) -> Vec<Map<String, Value>> {
    sorted_member_rows(members)
        .into_iter()
        .filter(|member| !rendered.contains(&map_str(member, "name")) && attachable(member))
        .collect()
}

/// Split panes into an existing team window for roster members it does not
/// render yet (a member spawned after the window was built, a session
/// mirror `hive mirror on` asks back — *mirror_pref* is what decides a
/// session member's pane, see `pane_role`). Re-tiles when it added any.
pub(crate) fn backfill_missing_member_panes(
    window: &str,
    entry: &Map<String, Value>,
    mirror_pref: Option<bool>,
) -> Vec<String> {
    let team = map_str(entry, "team");
    let ws = map_str(entry, "workspace");
    let rendered: std::collections::HashSet<String> = tmux::list_panes_full(window)
        .into_iter()
        .filter(|p| p.is_member_pane() && !p.agent.is_empty())
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
    for member in members_to_backfill(&rendered, entry_members(entry)) {
        let Some(role) = pane_role(mirror_pref, &member) else {
            continue;
        };
        let name = map_str(&member, "name");
        if role == "mirror" && join_hidden_mirror(window, &team, &name).is_some() {
            added.push(name);
            continue;
        }
        let cwd = map_str(&member, "cwd");
        let split = tmux::split_window(
            &prev_pane,
            crate::layout::split_horizontal(window),
            None,
            true,
            if cwd.is_empty() { None } else { Some(&cwd) },
        )
        .unwrap_or_default();
        if split.is_empty() {
            continue;
        }
        bind_member_viewer(&split, &member, &team, &ws, role);
        added.push(name);
        prev_pane = split;
    }
    if !added.is_empty() {
        let _ = crate::layout::ensure(window, false);
    }
    added
}

/// Session geometry for a team session hive builds itself.
const _TEAM_SESSION_COLS: u32 = 220;
const _TEAM_SESSION_ROWS: u32 = 60;

/// Marks a window hive built itself (as opposed to one a human's session
/// lent the team): `hive delete` closes only these.
fn mark_hive_built(window: &str) {
    tmux::set_window_option(window, "@hive-built", "1");
}

/// The team's window in the session named after it: a fresh detached
/// session when none exists, a new window in it otherwise. Returns
/// (window target, first pane id, created_session).
pub(crate) fn new_team_session_window(team: &str) -> Result<(String, String, bool)> {
    // `=` pins the exact name: a bare `-t <team>` falls back to prefix
    // matching and would put the window into a stranger's `<team>-x`.
    let exact = format!("={team}");
    if tmux::has_session(&exact) {
        // new_window forces "<team>:" so a numeric name is a session, not an index
        let (window, pane) = tmux::new_window(&exact, team, None, true)?;
        mark_hive_built(&window);
        install_team_status(&pane);
        return Ok((window, pane, false));
    }
    let pane = tmux::new_session(team, _TEAM_SESSION_COLS, _TEAM_SESSION_ROWS)?;
    // Never fall back to "<team>:" here — that is a session target, not a
    // window, and the first window's index follows the user's base-index.
    let window = tmux::get_pane_window_target(&pane)
        .filter(|w| !w.is_empty())
        .ok_or_else(|| anyhow!("tmux did not report the window of pane {pane}"))?;
    tmux::rename_window(&window, team);
    mark_hive_built(&window);
    install_team_status(&pane);
    Ok((window, pane, true))
}

/// hive's status bar on the session *pane* belongs to — the team session
/// only; a window built inside the caller's own session leaves their
/// status line alone.
fn install_team_status(pane: &str) {
    let sid = tmux::display_value(pane, "#{session_id}").unwrap_or_default();
    if !sid.is_empty() {
        tmux::install_team_status(&sid);
    }
}

/// Where a team window goes for the caller: inside tmux the caller's own
/// session, outside tmux the team session. Returns (window target, first
/// pane id).
fn team_window_for_caller(team: &str, anchor_cwd: &str) -> (String, String) {
    if !tmux::is_inside_tmux() {
        let (window, pane, _) = ok_or_fail(new_team_session_window(team));
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
    mark_hive_built(&window);
    (window, first_pane)
}

/// Build a window for the team: one attach pane per member, tiled. A team
/// with no attachable member still gets its window (the first pane stays a
/// shell).
///
/// Returns (window_target, attached_member_names, skipped_member_names).
fn materialize_team_display(entry: &Map<String, Value>) -> (String, Vec<String>, Vec<String>) {
    let team = map_str(entry, "team");
    let ws = map_str(entry, "workspace");
    let members = sorted_member_rows(entry_members(entry));
    let attachable_idx: Vec<usize> = members
        .iter()
        .enumerate()
        .filter(|(_, member)| attachable(member))
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
    let (window, first_pane) = team_window_for_caller(&team, &anchor_cwd);

    tmux::configure_hive_window(&window);
    tmux::set_window_option(&window, "@hive-team", &team);
    tmux::set_window_option(&window, "@hive-workspace", &ws);
    tmux::set_window_option(&window, "@hive-created", &map_str(entry, "createdAt"));

    let mut attached: Vec<String> = Vec::new();
    let mut prev_pane = first_pane.clone();
    // The first member to take a pane gets the window's own; a withheld
    // mirror leaves it a shell, as a window with nobody attachable is.
    let mut first_free = true;
    let mirror_pref = mirror_preference(&window);
    for index in &attachable_idx {
        let member = &members[*index];
        let Some(role) = pane_role(mirror_pref, member) else {
            continue;
        };
        let name = map_str(member, "name");
        if role == "mirror" && join_hidden_mirror(&window, &team, &name).is_some() {
            attached.push(name);
            continue;
        }
        let cwd = map_str(member, "cwd");
        let pane = if first_free {
            first_free = false;
            first_pane.clone()
        } else {
            let split = ok_or_fail(tmux::split_window(
                &prev_pane,
                crate::layout::split_horizontal(&window),
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
        bind_member_viewer(&pane, member, &team, &ws, role);
        attached.push(name);
        prev_pane = pane;
    }

    let _ = crate::layout::ensure(&window, false);
    let _ = crate::registry::set_display(&team, &tmux::get_window_id(&window).unwrap_or_default());
    (window, attached, skipped)
}

/// The registry entry for *team_name*, or the `hive ls` refusal.
pub(crate) fn team_entry(team_name: &str) -> Result<Map<String, Value>, String> {
    crate::registry::load(team_name)
        .ok_or_else(|| format!("team '{team_name}' not found (see `hive ls`)"))
}

fn team_window(team_name: &str) -> String {
    crate::team::find_team_window(team_name, "")
        .map(|(window, _)| window)
        .unwrap_or_default()
}

/// The team's display, made whole: rebuilt when the window is gone,
/// backfilled with a pane per roster member it does not show yet. Returns
/// (window, built).
pub(crate) fn ensure_team_display(entry: &Map<String, Value>) -> (String, bool) {
    let team = map_str(entry, "team");
    let window = team_window(&team);
    if window.is_empty() {
        let (window, _attached, skipped) = materialize_team_display(entry);
        for name in skipped {
            eprintln!("! {name}: no attachable engine identity — no pane");
        }
        return (window, true);
    }
    for name in backfill_missing_member_panes(&window, entry, mirror_preference(&window)) {
        eprintln!("+ {name}: pane added to the existing window");
    }
    (window, false)
}

/// The jump attach ends on. Inside tmux, `switch-client` moves *this*
/// client — `select-window` would only retarget the window's own session and
/// leave a client attached elsewhere untouched.
fn jump_to_window(window: &str, verdict: &str) {
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
    let entry = match team_entry(team_name) {
        Ok(entry) => entry,
        Err(message) => fail(&message),
    };
    let (window, built) = ensure_team_display(&entry);
    let ws = map_str(&entry, "workspace");
    if !ws.is_empty() {
        if let Ok(mut t) = Team::load(team_name, "") {
            let _ = start_team_hived(&mut t, &ws);
        }
    }
    jump_to_window(&window, if built { "built" } else { "found" });
}

// ---------------------------------------------------------------------------
// thread / capture
// ---------------------------------------------------------------------------

pub fn thread(message_id: &str) {
    let (_, t) = ok_or_fail(resolve_scoped_team(None, true));
    let mut t = t.expect("required resolve returned no team");
    let ws = ok_or_fail(resolve_workspace(Some(&t), true));
    let _ = start_team_hived(&mut t, &ws);
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
    println!("{}", ok_or_fail(capture_text(&t, member_name, lines)));
}

/// The last *lines* of the member's own pane (the pane its roster row
/// resolved to), or the not-found refusal.
pub(crate) fn capture_text(t: &Team, member_name: &str, lines: i64) -> Result<String> {
    let agent = t
        .get(member_name)
        .map_err(|_| anyhow!("member '{member_name}' not found in team '{}'", t.name))?;
    agent.capture(lines.max(0) as u32)
}
