//! The team's eager tmux display on top of the registry: the window in the
//! session named after the team (or the caller's), one viewer pane per
//! roster member with an engine identity, a Claude session member's
//! read-only mirror and the `@hive-mirror` choice that shows or withholds
//! it. Built at create/join/spawn/attach, healed when the window is gone,
//! backfilled when the roster outgrew it. tmux is display: nothing here is
//! authority on who is on the team.

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};

use crate::identity;
use crate::json_fields::{is_set, map_str};
use crate::paths::getcwd;
use crate::shell::shlex_quote;
use crate::team::sorted_member_rows;
use crate::tmux;

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
    // `join-pane -b` lands the pane before `first` on tmux 3.4 and after
    // it on 3.7; the mirror belongs at the front either way, so the
    // order is read back and settled here rather than trusted.
    let index =
        |pane: &str| tmux::display_value(pane, "#{pane_index}").and_then(|v| v.parse::<u32>().ok());
    if let (Some(joined), Some(anchor)) = (index(hidden), index(first)) {
        if joined > anchor {
            tmux::swap_pane(hidden, first);
        }
    }
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
) -> Result<()> {
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
    tmux::send_keys(pane, &member_attach_command(member, role == "mirror"), true)
}

/// A member a pane can ride: engine identity recorded, on a CLI
/// `attach_launcher` has a resume form for.
fn attachable(member: &Map<String, Value>) -> bool {
    is_set(member.get("sessionId")) && attach_launcher(&map_str(member, "cli"), "").is_some()
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
fn members_to_backfill(
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
) -> Result<Vec<String>> {
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
        return Ok(Vec::new());
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
        bind_member_viewer(&split, &member, &team, &ws, role)?;
        added.push(name);
        prev_pane = split;
    }
    if !added.is_empty() {
        let _ = crate::layout::ensure(window, false);
    }
    Ok(added)
}

/// Session geometry for a team session hive builds itself.
const TEAM_SESSION_COLS: u32 = 220;

const TEAM_SESSION_ROWS: u32 = 60;

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
    let pane = tmux::new_session(team, TEAM_SESSION_COLS, TEAM_SESSION_ROWS)?;
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
fn team_window_for_caller(team: &str, anchor_cwd: &str) -> Result<(String, String)> {
    if !identity::is_inside_tmux() {
        let (window, pane, _) = new_team_session_window(team)?;
        return Ok((window, pane));
    }
    let session_name = identity::current_session_name()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "hive".to_string());
    if !tmux::has_session(&session_name) {
        let _ = tmux::new_session(&session_name, TEAM_SESSION_COLS, TEAM_SESSION_ROWS);
    }
    let (window, first_pane) =
        tmux::new_window(&session_name, team, Some(anchor_cwd), true).unwrap_or_default();
    if window.is_empty() || first_pane.is_empty() {
        bail!("failed to create a window for the team");
    }
    mark_hive_built(&window);
    Ok((window, first_pane))
}

/// Build a window for the team: one attach pane per member, tiled. A team
/// with no attachable member still gets its window (the first pane stays a
/// shell).
///
/// Returns (window_target, attached_member_names, skipped_member_names).
fn materialize_team_display(
    entry: &Map<String, Value>,
) -> Result<(String, Vec<String>, Vec<String>)> {
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
    let (window, first_pane) = team_window_for_caller(&team, &anchor_cwd)?;

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
            let split = tmux::split_window(
                &prev_pane,
                crate::layout::split_horizontal(&window),
                None,
                true,
                if cwd.is_empty() { None } else { Some(&cwd) },
            )?;
            if split.is_empty() {
                skipped.push(name);
                continue;
            }
            split
        };
        bind_member_viewer(&pane, member, &team, &ws, role)?;
        attached.push(name);
        prev_pane = pane;
    }

    let _ = crate::layout::ensure(&window, false);
    let _ = crate::registry::set_display(&team, &tmux::get_window_id(&window).unwrap_or_default());
    Ok((window, attached, skipped))
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
pub(crate) fn ensure_team_display(entry: &Map<String, Value>) -> Result<(String, bool)> {
    let team = map_str(entry, "team");
    let window = team_window(&team);
    if window.is_empty() {
        let (window, _attached, skipped) = materialize_team_display(entry)?;
        for name in skipped {
            eprintln!("! {name}: no attachable engine identity — no pane");
        }
        return Ok((window, true));
    }
    for name in backfill_missing_member_panes(&window, entry, mirror_preference(&window))? {
        eprintln!("+ {name}: pane added to the existing window");
    }
    Ok((window, false))
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::testkit::{
        claude_session_me, count, display_env, display_env_outside, fake_tmux, fake_tmux_sessions,
        fake_tmux_tagged, has_row, member_row,
    };

    #[test]
    fn test_attach_backfills_only_missing_attachable_members() {
        let rendered: std::collections::HashSet<String> =
            ["orch".to_string(), "scout".to_string()].into();
        let picked = members_to_backfill(
            &rendered,
            vec![
                member_row("orch", "claude", "sid-1"),  // already rendered
                member_row("scout", "claude", "sid-2"), // already rendered
                member_row("sage", "grok", "sid-3"),    // missing -> backfill
                member_row("ghost", "grok", ""),        // no engine identity
                member_row("shelly", "bash", "sid-4"),  // not an agent CLI
            ],
        );
        let names: Vec<String> = picked.iter().map(|m| map_str(m, "name")).collect();
        assert_eq!(names, vec!["sage".to_string()]);
    }

    #[test]
    fn test_attach_names_the_missing_team_before_looking_at_tmux() {
        let _env = display_env();
        let argv = fake_tmux("", &[]);

        let message = team_entry("ghost").unwrap_err();

        assert!(message.contains("hive ls"), "{message}");
        assert!(argv.borrow().is_empty());
    }

    #[test]
    fn test_attach_heal_outside_tmux_builds_the_team_session() {
        let _env = display_env_outside();
        crate::registry::record_team(
            "honey",
            "",
            "100.0",
            &[
                member_row("orch", "grok", "sid-orch"),
                member_row("sage", "grok", "sid-sage"),
            ],
            "",
        )
        .unwrap();
        let argv = fake_tmux_sessions("", &[], &[], &[]);

        // Not `attach_cmd`: outside tmux it would exec `tmux attach`.
        let (window, built) =
            ensure_team_display(&crate::registry::load("honey").unwrap()).unwrap();

        assert!(built);
        // The window's index is read back from tmux, never assumed to be 0.
        assert_eq!(window, "honey:1");
        assert!(has_row(
            &argv,
            &[
                "new-session",
                "-d",
                "-s",
                "honey",
                "-x",
                "220",
                "-y",
                "60",
                "-P",
                "-F",
                "#{pane_id}",
            ]
        ));
        assert!(has_row(&argv, &["rename-window", "-t", "honey:1", "honey"]));
        assert_eq!(count(&argv, "new-window"), 0);
        // The one split hangs off the pane `new-session` handed back, so the
        // second member lands in the team session and nowhere else.
        let splits: Vec<Vec<String>> = argv
            .borrow()
            .iter()
            .filter(|a| a[0] == "split-window")
            .cloned()
            .collect();
        assert_eq!(splits.len(), 1, "{splits:?}");
        assert_eq!(&splits[0][..3], ["split-window", "-t", "%1"]);
        assert!(has_row(
            &argv,
            &["set-window-option", "-t", "honey:1", "@hive-team", "honey"]
        ));
        assert!(has_row(
            &argv,
            &["set-window-option", "-t", "honey:1", "@hive-built", "1"]
        ));
        assert_eq!(
            crate::registry::load("honey").unwrap()["display"],
            Value::from("@7")
        );
    }

    #[test]
    fn test_attach_heal_outside_tmux_reuses_a_session_named_after_the_team() {
        let _env = display_env_outside();
        crate::registry::record_team(
            "honey",
            "",
            "100.0",
            &[
                member_row("orch", "grok", "sid-orch"),
                member_row("sage", "grok", "sid-sage"),
            ],
            "",
        )
        .unwrap();
        let argv = fake_tmux_sessions("", &[], &[], &["honey"]);

        let (_window, built) =
            ensure_team_display(&crate::registry::load("honey").unwrap()).unwrap();

        assert!(built);
        assert_eq!(count(&argv, "new-session"), 0);
        assert_eq!(count(&argv, "new-window"), 1);
        let new_window = argv
            .borrow()
            .iter()
            .find(|a| a[0] == "new-window")
            .cloned()
            .unwrap();
        assert_eq!(&new_window[..3], ["new-window", "-t", "=honey:"]);
        assert!(new_window.windows(2).any(|pair| pair == ["-n", "honey"]));
        assert!(has_row(
            &argv,
            &["set-window-option", "-t", "honey:2", "@hive-built", "1"]
        ));
    }

    #[test]
    fn test_team_session_is_matched_by_exact_name_never_by_prefix() {
        let _env = display_env_outside();
        // A stranger's session whose name merely starts with the team name: a
        // bare `-t hornet` would resolve to it and put the team window there.
        let argv = fake_tmux_sessions("", &[], &[], &["hornet-x"]);

        let (window, first_pane, created) = new_team_session_window("hornet").unwrap();

        assert!(created);
        assert_eq!(window, "hornet:1");
        assert_eq!(first_pane, "%1");
        assert!(has_row(&argv, &["has-session", "-t", "=hornet"]));
        assert!(has_row(
            &argv,
            &[
                "new-session",
                "-d",
                "-s",
                "hornet",
                "-x",
                "220",
                "-y",
                "60",
                "-P",
                "-F",
                "#{pane_id}",
            ]
        ));
        assert_eq!(count(&argv, "new-window"), 0);
    }

    #[test]
    fn test_attach_heal_builds_a_window_for_a_team_with_no_attachable_member() {
        let _env = display_env();
        crate::registry::record_team(
            "honey",
            "",
            "100.0",
            &[member_row("orch", "claude", "")],
            "",
        )
        .unwrap();
        let argv = fake_tmux("", &[]);

        let (_window, built) =
            ensure_team_display(&crate::registry::load("honey").unwrap()).unwrap();

        // The window exists for the team, not for its members: nobody rides a
        // pane, so the first pane stays a shell and no viewer is launched.
        assert!(built);
        assert_eq!(count(&argv, "new-window"), 1);
        assert_eq!(count(&argv, "split-window"), 0);
        assert_eq!(count(&argv, "send-keys"), 0);
        assert_eq!(
            crate::registry::load("honey").unwrap()["display"],
            Value::from("@7")
        );
    }

    #[test]
    fn test_pane_role_draws_the_mirror_unless_the_preference_is_off() {
        let mut env = display_env();
        let _claude = claude_session_me(&mut env);
        let argv = fake_tmux_tagged("dev:1\t@7\thoney\t\t\t\n", &[], &[]);
        let orch = member_row("orch", "claude", "s-me");

        assert_eq!(pane_role(None, &orch), Some("mirror"));
        assert_eq!(pane_role(Some(true), &orch), Some("mirror"));
        assert_eq!(pane_role(Some(false), &orch), None);
        // An engine member never mirrors, whatever the window records.
        assert_eq!(
            pane_role(Some(false), &member_row("sage", "grok", "sid")),
            Some("agent")
        );
        assert_eq!(count(&argv, "set-window-option"), 0);
    }
}
