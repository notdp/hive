//! `hive mirror [on|off]`: show or hide the mirror rails of the current
//! team window. Display state only — `hive view` panes come and go, the
//! choice lands on the window as `@hive-mirror` so heal and backfill keep it.

use super::*;
use crate::tmux;

pub fn mirror_cmd(mode: &str) {
    match _mirror(mode) {
        Ok(line) => println!("{line}"),
        Err(message) => fail(&message),
    }
}

/// The verb's one line of output, or its refusal.
pub(crate) fn _mirror(mode: &str) -> Result<String, String> {
    let window = tmux::get_current_window_target()
        .filter(|w| !w.is_empty())
        .ok_or_else(|| "hive mirror runs from a pane in a team window".to_string())?;
    let team = tmux::get_window_option(&window, "hive-team")
        .ok_or_else(|| format!("window {window} is not a team window (see `hive ls`)"))?;
    let entry = _team_entry(&team)?;
    let rails: Vec<String> = tmux::list_panes_full(&window)
        .into_iter()
        .filter(|p| p.role == "mirror")
        .map(|p| p.pane_id)
        .collect();
    let on = match mode {
        "on" => true,
        "off" => false,
        _ => rails.is_empty(),
    };
    // The choice is recorded either way; the window is only touched when
    // there is a rail to add or remove, so a no-op keeps the human's zoom.
    // `split-window` and `kill-pane` both unzoom, so the re-tile lands.
    if on {
        tmux::set_window_option(&window, "@hive-mirror", "on");
        if _backfill_missing_member_panes(&window, &entry).is_empty() {
            return Ok(format!("mirror on ({team}): no session mirror to show"));
        }
        return Ok(format!("mirror on ({team})"));
    }
    let me = tmux::get_current_pane_id().unwrap_or_default();
    if rails.contains(&me) {
        return Err("this pane is the rail; run `hive mirror off` from another pane".to_string());
    }
    tmux::set_window_option(&window, "@hive-mirror", "off");
    if rails.is_empty() {
        return Ok(format!("mirror off ({team}): no rail"));
    }
    for pane in &rails {
        tmux::kill_pane(pane);
    }
    let _ = crate::layout::apply_adaptive(&window);
    Ok(format!("mirror off ({team})"))
}
