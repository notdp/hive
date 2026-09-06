//! Display verbs: `attach` (heal the team's window, then jump to it),
//! `mirror` (show or park the session mirror pane), `layout`.

use serde_json::json;

use super::util::{fail, ok_or_fail};
use crate::identity;
use crate::json_fields::map_str;
use crate::team::{resolve_scoped_team, start_team_hived_or_warn, Team};
use crate::team_display::{backfill_missing_member_panes, ensure_team_display, team_entry};
use crate::tmux;

/// `hive layout <preset|auto> [--on-change] [--window TARGET]`. `auto`
/// from a human forces the plan (the "布局拖乱了" repair); `--on-change`
/// is the window hooks' form, which applies only when the plan's key
/// differs from `@hive-layout` and prints nothing (a run-shell job's
/// output would land in a tmux view). `--window` names the window when
/// there is no caller pane (a run-shell job has no TMUX_PANE).
pub(crate) fn layout_cmd(preset: &str, on_change: bool, window: &str) {
    let window_target = if !window.is_empty() {
        window.to_string()
    } else {
        let (_, t) = ok_or_fail(resolve_scoped_team(None, true));
        let t = t.expect("required resolve returned no team");
        if !t.tmux_window.is_empty() {
            t.tmux_window.clone()
        } else {
            identity::current_window_target().unwrap_or_default()
        }
    };
    if window_target.is_empty() {
        fail("Cannot determine tmux window target");
    }
    if preset == "auto" {
        if on_change {
            crate::layout::ensure_hook(&window_target);
            return;
        }
        let outcome = crate::layout::ensure(&window_target, true);
        let plan = outcome.plan();
        println!(
            "{}",
            json!({
                "layout": plan.map(|p| p.key.as_str()).unwrap_or_default(),
                "orientation": plan.map(|p| p.orientation).unwrap_or_default(),
                "window": window_target,
                "applied": outcome.applied(),
                "reason": outcome.reason(),
            })
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
    println!("{}", json!({"layout": preset, "window": window_target}));
}

// ---------------------------------------------------------------------------
// attach
// ---------------------------------------------------------------------------
/// The jump attach ends on. Inside tmux, `switch-client` moves *this*
/// client — `select-window` would only retarget the window's own session and
/// leave a client attached elsewhere untouched.
fn jump_to_window(window: &str, verdict: &str) {
    if identity::is_inside_tmux() {
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

pub(crate) fn attach_cmd(team_name: &str) {
    let entry = match team_entry(team_name) {
        Ok(entry) => entry,
        Err(message) => fail(&message),
    };
    let (window, built) = ok_or_fail(ensure_team_display(&entry));
    let ws = map_str(&entry, "workspace");
    if !ws.is_empty() {
        if let Ok(mut t) = Team::load(team_name, "") {
            start_team_hived_or_warn(&mut t, &ws);
        }
    }
    jump_to_window(&window, if built { "built" } else { "found" });
}

/// `hive mirror [on|off] [--window TARGET]`: show or hide the team's
/// read-only orch mirror pane. Display state only — `off` parks the pane
/// with break-pane in a hidden window of the team session (the viewer keeps
/// running), `on` joins it back; the choice lands on the window as
/// `@hive-mirror` so heal and backfill keep it.
pub(crate) fn mirror_cmd(mode: &str, window: &str) {
    match mirror(mode, window) {
        Ok(line) => println!("{line}"),
        Err(message) => fail(&message),
    }
}

/// The verb's one line of output, or its refusal. *window_arg* names the
/// team window when there is no caller pane (the status-bar click and
/// prefix+m run from a run-shell job, which has no TMUX_PANE).
pub(crate) fn mirror(mode: &str, window_arg: &str) -> Result<String, String> {
    let window = if window_arg.is_empty() {
        identity::current_window_target()
            .filter(|w| !w.is_empty())
            .ok_or_else(|| "hive mirror runs from a pane in a team window".to_string())?
    } else {
        window_arg.to_string()
    };
    let team = tmux::get_window_option(&window, "hive-team")
        .ok_or_else(|| format!("window {window} is not a team window (see `hive ls`)"))?;
    let entry = team_entry(&team)?;
    let shown = shown_mirrors(&window);
    let on = match mode {
        "on" => true,
        "off" => false,
        _ => shown.is_empty(),
    };
    // The choice is recorded once it holds — never for a refusal, never for
    // an `on` with nothing to show (the chip would toggle nothing). The
    // window is only touched when a pane has to move, so a no-op keeps the
    // human's zoom; break-pane and join-pane both unzoom, so the re-tile
    // after them lands.
    if on {
        if !shown.is_empty() {
            tmux::set_window_option(&window, "@hive-mirror", "on");
            return Ok(format!("mirror on ({team}): already shown"));
        }
        // A roster member's parked pane joins back, a missing one is
        // rebuilt the way an attach heal would.
        backfill_missing_member_panes(&window, &entry, Some(true)).map_err(|e| e.to_string())?;
        if shown_mirrors(&window).is_empty() {
            return Ok(format!("mirror on ({team}): no session mirror to show"));
        }
        tmux::set_window_option(&window, "@hive-mirror", "on");
        return Ok(format!("mirror on ({team})"));
    }
    if window_arg.is_empty() {
        let me = identity::current_pane_id().unwrap_or_default();
        if shown.contains(&me) {
            return Err(
                "this pane is the mirror; run `hive mirror off` from another pane".to_string(),
            );
        }
    }
    if shown.is_empty() {
        tmux::set_window_option(&window, "@hive-mirror", "off");
        return Ok(format!("mirror off ({team}): no mirror"));
    }
    if tmux::list_panes(&window).len() == 1 {
        return Err(
            "the mirror is the window's only pane (break-pane would only rename the window); \
             spawn a member first"
                .to_string(),
        );
    }
    tmux::set_window_option(&window, "@hive-mirror", "off");
    // The hidden window goes into the team session when there is one; a
    // team built inside the caller's session parks it there.
    let session = format!("={team}");
    let target = tmux::has_session(&session).then(|| format!("{session}:"));
    for pane in &shown {
        let (hidden, _) =
            tmux::break_pane(pane, &format!("{team}·mirror"), true, target.as_deref())
                .map_err(|e| format!("break-pane {pane}: {e}"))?;
        tmux::set_window_option(&hidden, &format!("@{}", tmux::HIDDEN_WINDOW_KEY), &team);
    }
    let _ = crate::layout::ensure(&window, false);
    Ok(format!("mirror off ({team})"))
}

fn shown_mirrors(window: &str) -> Vec<String> {
    tmux::list_panes_full(window)
        .into_iter()
        .filter(|p| p.role == "mirror")
        .map(|p| p.pane_id)
        .collect()
}

#[cfg(test)]
mod tests;
