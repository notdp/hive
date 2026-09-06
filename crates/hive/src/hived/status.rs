// --------------------------------------------------------------------------
// status tick: the display writes behind the team session's status bar
// (`tmux/status.rs`) — `@hive-busy` / `@hive-unread` per member pane and
// `@hive-ticker` per team window. Display only: the registry stays the
// truth, and a tick writes edges, not state.
// --------------------------------------------------------------------------

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::*;

/// What the status tick last wrote, so a tick writes only edges.
#[derive(Debug, Default)]
pub struct StatusTickState {
    pub busy: HashMap<String, bool>,
    pub unread: HashMap<String, bool>,
    pub ticker: HashMap<String, String>,
}

pub const TICKER_BODY_CHARS: usize = 80;
pub const TICKER_ROWS: usize = 2;

/// `from → to · age · "body head"` per row, newest first, joined by
/// `   │   `. `#` is doubled: the value is drawn by the status line, where
/// `#[` opens a style.
pub fn ticker_text(events: &[crate::bus::Event], now_epoch: i64) -> String {
    events
        .iter()
        .map(|e| {
            format!(
                "{} → {} · {} · \"{}\"",
                e.from.replace('#', "##"),
                e.to.replace('#', "##"),
                ticker_age(&e.created_at, now_epoch),
                ticker_head(&e.body)
            )
        })
        .collect::<Vec<_>>()
        .join("   │   ")
}

/// `now` under a minute, then `Nm`, `Nh`, `Nd`; `?` when *created_at* does
/// not parse (the bus's `now_iso` shape).
pub fn ticker_age(created_at: &str, now_epoch: i64) -> String {
    let Some(stamp) = crate::adapters::base::parse_iso_timestamp(Some(&Value::from(created_at)))
    else {
        return "?".to_string();
    };
    let age = (now_epoch - stamp.timestamp() as i64).max(0);
    match age {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", age / 60),
        3600..=86_399 => format!("{}h", age / 3600),
        _ => format!("{}d", age / 86_400),
    }
}

/// Whitespace-collapsed body, at most `TICKER_BODY_CHARS` chars, `…`
/// appended when clipped, then `#` → `##`.
pub fn ticker_head(body: &str) -> String {
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut head: String = collapsed.chars().take(TICKER_BODY_CHARS).collect();
    if collapsed.chars().count() > TICKER_BODY_CHARS {
        head.push('…');
    }
    head.replace('#', "##")
}

/// One status tick. Only `agent` panes get the two pane options — a
/// mirror's `hive view` repaint would read as output, and a mirror or
/// terminal pane has no chip to mark unread; the ticker lands on the window
/// of the first bound engine pane.
pub fn _status_tick(
    workspace: &str,
    members: &[(String, Map<String, Value>)],
    monitor: Option<&dyn OutputMonitor>,
    state: &mut StatusTickState,
    now_epoch: i64,
) {
    let panes = hooked_list_panes_all();
    if panes.is_empty() {
        return; // an empty listing is a tmux failure, not an empty server
    }
    let roles: HashMap<&str, &str> = panes
        .iter()
        .map(|p| (p.pane_id.as_str(), p.role.as_str()))
        .collect();
    let bound: Vec<String> = members
        .iter()
        .map(|(_, binding)| map_get_str(binding, "pane"))
        .filter(|pane| !pane.is_empty())
        .collect();
    unread_pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|pane| roles.get(pane.as_str()) == Some(&"agent"));
    for pane in bound
        .iter()
        .filter(|p| roles.get(p.as_str()) == Some(&"agent"))
    {
        let busy = _is_output_busy(pane, monitor, None);
        if busy {
            unread_pending()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(pane);
        }
        let unread = !busy
            && unread_pending()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(pane);
        let flag = |on: bool| if on { "1" } else { "0" };
        if state.busy.get(pane) != Some(&busy) {
            state.busy.insert(pane.clone(), busy);
            hooked_set_pane_option(pane, "hive-busy", flag(busy));
        }
        if state.unread.get(pane) != Some(&unread) {
            state.unread.insert(pane.clone(), unread);
            hooked_set_pane_option(pane, "hive-unread", flag(unread));
        }
    }

    // The ticker follows an engine pane, not the hived's own window
    // argument (stale once `hive attach` rebuilt the window) and never a
    // parked mirror (its hidden window has no bar).
    let Some(anchor) = bound
        .iter()
        .find(|p| roles.get(p.as_str()) == Some(&"agent"))
    else {
        return;
    };
    let Some(window) = hooked_get_pane_window_target(anchor).filter(|w| !w.is_empty()) else {
        return;
    };
    let Ok(events) = crate::bus::latest_send_events(workspace, TICKER_ROWS) else {
        return;
    };
    let text = ticker_text(&events, now_epoch);
    if state.ticker.get(&window) != Some(&text) {
        state.ticker.insert(window.clone(), text.clone());
        hooked_set_window_option(&window, "@hive-ticker", &text);
    }
}
