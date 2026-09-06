//! `hive mirror [on|off] [--window TARGET]`: show or hide the team's
//! read-only orch mirror pane. Display state only — `off` parks the pane
//! with break-pane in a hidden window of the team session (the viewer keeps
//! running), `on` joins it back; the choice lands on the window as
//! `@hive-mirror` so heal and backfill keep it.

use super::*;
use crate::tmux;

pub fn mirror_cmd(mode: &str, window: &str) {
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
        tmux::get_current_window_target()
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
        // rebuilt the way an attach heal would; the rig mirror is no
        // roster member.
        backfill_missing_member_panes(&window, &entry, Some(true));
        join_rig_mirror(&window, &team);
        if shown_mirrors(&window).is_empty() {
            return Ok(format!("mirror on ({team}): no session mirror to show"));
        }
        tmux::set_window_option(&window, "@hive-mirror", "on");
        return Ok(format!("mirror on ({team})"));
    }
    if window_arg.is_empty() {
        let me = tmux::get_current_pane_id().unwrap_or_default();
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

/// A flow rig's mirror (`flow_rig.rs`) names no member: its parked pane
/// joins back by team. A parked pane naming a member is that member's
/// (`join_hidden_mirror`), never joined here.
fn join_rig_mirror(window: &str, team: &str) {
    let Some(hidden) = tmux::hidden_mirror_pane(team) else {
        return;
    };
    if tmux::get_pane_option(&hidden, "hive-agent").is_some_and(|a| !a.is_empty()) {
        return;
    }
    let Some(first) = tmux::list_panes(window).into_iter().next() else {
        return;
    };
    join_parked_pane(&hidden, &first);
    let _ = crate::layout::ensure(window, false);
}
