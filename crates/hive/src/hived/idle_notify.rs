// --------------------------------------------------------------------------
// idle notify state machine
// --------------------------------------------------------------------------

use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value};

use super::*;

/// One window's idle-notify record.
#[derive(Debug, Clone, PartialEq)]
pub struct IdleRecord {
    pub last_busy_ts: f64,
    pub notified: bool,
    pub seen_since_fire: bool,
    pub missing_ticks: i64,
    pub last_busy_pane: Option<String>,
}

impl IdleRecord {
    pub fn new(last_busy_ts: f64, notified: bool, seen_since_fire: bool) -> IdleRecord {
        IdleRecord {
            last_busy_ts,
            notified,
            seen_since_fire,
            missing_ticks: 0,
            last_busy_pane: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WinDebug {
    pub busy_observed: bool,
    pub observed_token: Option<String>,
}

/// Cross-tick debug bookkeeping for the idle-notify log.
#[derive(Debug)]
pub struct NotifyDebugState {
    pub tick_seq: u64,
    pub windows: HashMap<String, WinDebug>,
    pub active_window: Option<String>,
    pub inactive_at: HashMap<String, f64>,
    pub windows_keys: Option<Vec<String>>,
    pub last_heartbeat: f64,
}

impl Default for NotifyDebugState {
    fn default() -> Self {
        NotifyDebugState {
            tick_seq: 0,
            windows: HashMap::new(),
            active_window: None,
            inactive_at: HashMap::new(),
            windows_keys: None,
            // `monotonic()` starts near zero: negative infinity makes the
            // first tick emit a heartbeat.
            last_heartbeat: f64::NEG_INFINITY,
        }
    }
}

fn record_state_value(record: &IdleRecord) -> Value {
    let mut map = Map::new();
    map.insert("notified".to_string(), Value::Bool(record.notified));
    map.insert(
        "seen_since_fire".to_string(),
        Value::Bool(record.seen_since_fire),
    );
    map.insert("last_busy_ts".to_string(), Value::from(record.last_busy_ts));
    Value::Object(map)
}

fn notify_state_value(notified: bool, seen_since_fire: bool) -> Value {
    let mut map = Map::new();
    map.insert("notified".to_string(), Value::Bool(notified));
    map.insert("seen_since_fire".to_string(), Value::Bool(seen_since_fire));
    Value::Object(map)
}

/// What every phase after `collect_windows` shares within one tick.
struct TickCtx<'a> {
    team_name: &'a str,
    workspace: &'a str,
    now: f64,
    active_window: String,
    token_key: &'a str,
    busy_monitor: Option<&'a dyn OutputMonitor>,
}

#[allow(clippy::too_many_arguments)]
pub fn _idle_notify_tick(
    team_name: &str,
    session_name: &str,
    idle_notify: &mut HashMap<String, IdleRecord>,
    busy_monitor: Option<&dyn OutputMonitor>,
    now: f64,
    workspace: &str,
    debug_state: Option<&mut NotifyDebugState>,
    members: Option<&[(String, Map<String, Value>)]>,
) {
    let mut local_debug = NotifyDebugState::default();
    let debug_state = match debug_state {
        Some(state) => state,
        None => &mut local_debug,
    };
    debug_state.tick_seq += 1;

    let ctx = TickCtx {
        team_name,
        workspace,
        now,
        active_window: hooked_get_most_recent_client_window(session_name).unwrap_or_default(),
        token_key: crate::notify_ui::NOTIFY_TOKEN_OPTION.trim_start_matches('@'),
        busy_monitor,
    };
    let windows = collect_windows(team_name, members);
    emit_topology_changes(&ctx, debug_state, &windows);
    clear_active_window_token(&ctx, &windows);

    if !hooked_is_plugin_enabled("notify") {
        if !idle_notify.is_empty() {
            hooked_notify_debug_emit(
                workspace,
                "plugin.disabled",
                &[
                    ("team", Value::from(team_name)),
                    ("records_cleared", Value::from(idle_notify.len())),
                ],
            );
        }
        idle_notify.clear();
        return;
    }

    prune_missing_windows(&ctx, idle_notify, debug_state, &windows);

    for (window_target, window_panes) in &windows {
        let mut panes = window_panes.clone();
        panes.sort();
        let record_existed = idle_notify.contains_key(window_target);
        let record = idle_notify
            .entry(window_target.clone())
            .or_insert_with(|| IdleRecord::new(now, true, true));
        if !record_existed {
            hooked_notify_debug_emit(
                workspace,
                "record.create",
                &[
                    ("team", Value::from(team_name)),
                    ("window", Value::from(window_target.clone())),
                    (
                        "panes",
                        Value::Array(panes.iter().cloned().map(Value::from).collect()),
                    ),
                    ("initial", record_state_value(record)),
                ],
            );
        }
        record.missing_ticks = 0;
        let inactive_age = debug_state
            .inactive_at
            .get(window_target)
            .map(|inactive_at_ts| now - inactive_at_ts);
        let win_dbg = debug_state
            .windows
            .entry(window_target.clone())
            .or_default();
        tick_window(&ctx, window_target, &panes, record, win_dbg, inactive_age);
    }

    emit_heartbeat(&ctx, debug_state, &windows, idle_notify.len());
}

/// The team's live agent panes grouped by tmux window.
fn collect_windows(
    team_name: &str,
    members: Option<&[(String, Map<String, Value>)]>,
) -> BTreeMap<String, Vec<String>> {
    let agent_panes: Vec<String> = match members {
        Some(members) => agent_panes_from_bindings(members),
        None => hooked_idle_notify_agent_panes(team_name),
    };
    let mut windows: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pane_id in &agent_panes {
        let window_target = hooked_get_pane_window_target(pane_id).unwrap_or_default();
        if window_target.is_empty() {
            continue;
        }
        windows
            .entry(window_target)
            .or_default()
            .push(pane_id.clone());
    }
    windows
}

/// Log active-window and window-set changes; stamp the moment the previous
/// active window became inactive so the busy check can ignore output the
/// user already saw while it was active (the newly-active window has no
/// inactive boundary).
fn emit_topology_changes(
    ctx: &TickCtx,
    debug_state: &mut NotifyDebugState,
    windows: &BTreeMap<String, Vec<String>>,
) {
    let active_window = &ctx.active_window;
    let prev_active_initialized = debug_state.active_window.is_some();
    let prev_active = debug_state.active_window.clone().unwrap_or_default();
    if !prev_active_initialized || prev_active != *active_window {
        hooked_notify_debug_emit(
            ctx.workspace,
            "active.changed",
            &[
                ("team", Value::from(ctx.team_name)),
                (
                    "old",
                    if prev_active_initialized {
                        Value::from(prev_active.clone())
                    } else {
                        Value::Null
                    },
                ),
                (
                    "new",
                    if active_window.is_empty() {
                        Value::Null
                    } else {
                        Value::from(active_window.clone())
                    },
                ),
            ],
        );
        if prev_active_initialized && !prev_active.is_empty() {
            debug_state.inactive_at.insert(prev_active.clone(), ctx.now);
        }
        if !active_window.is_empty() {
            debug_state.inactive_at.remove(active_window);
        }
        debug_state.active_window = Some(active_window.clone());
    }

    let new_keys: Vec<String> = windows.keys().cloned().collect();
    if debug_state.windows_keys.as_ref() != Some(&new_keys) {
        hooked_notify_debug_emit(
            ctx.workspace,
            "windows.changed",
            &[
                ("team", Value::from(ctx.team_name)),
                (
                    "old",
                    match debug_state.windows_keys.as_ref() {
                        Some(keys) => Value::Array(keys.iter().cloned().map(Value::from).collect()),
                        None => Value::Null,
                    },
                ),
                (
                    "new",
                    Value::Array(new_keys.iter().cloned().map(Value::from).collect()),
                ),
            ],
        );
        debug_state.windows_keys = Some(new_keys);
    }
}

/// The user is looking at the active window: drop its pending notify token.
fn clear_active_window_token(ctx: &TickCtx, windows: &BTreeMap<String, Vec<String>>) {
    let Some(window_panes) = windows.get(&ctx.active_window) else {
        return;
    };
    let token = hooked_get_window_option(&ctx.active_window, ctx.token_key).unwrap_or_default();
    if token.is_empty() {
        return;
    }
    let mut sorted_panes = window_panes.clone();
    sorted_panes.sort();
    hooked_notify_debug_emit(
        ctx.workspace,
        "active.clear_attempt",
        &[
            ("team", Value::from(ctx.team_name)),
            ("window", Value::from(ctx.active_window.clone())),
            ("token", Value::from(token.clone())),
            (
                "panes",
                Value::Array(sorted_panes.iter().cloned().map(Value::from).collect()),
            ),
        ],
    );
    hooked_clear_stale_notify(
        &ctx.active_window,
        &sorted_panes,
        &token,
        false,
        "hived.active_window",
        ctx.workspace,
    );
}

/// Forget windows that have not been seen for a few ticks.
fn prune_missing_windows(
    ctx: &TickCtx,
    idle_notify: &mut HashMap<String, IdleRecord>,
    debug_state: &mut NotifyDebugState,
    windows: &BTreeMap<String, Vec<String>>,
) {
    let known_windows: Vec<String> = idle_notify.keys().cloned().collect();
    for window_target in known_windows {
        let Some(record) = idle_notify.get_mut(&window_target) else {
            continue;
        };
        if windows.contains_key(&window_target) {
            record.missing_ticks = 0;
            continue;
        }
        record.missing_ticks += 1;
        if record.missing_ticks < IDLE_NOTIFY_MISSING_PRUNE_TICKS {
            continue;
        }
        hooked_notify_debug_emit(
            ctx.workspace,
            "record.prune",
            &[
                ("team", Value::from(ctx.team_name)),
                ("window", Value::from(window_target.clone())),
                ("missing_ticks", Value::from(record.missing_ticks)),
                ("last_state", record_state_value(record)),
            ],
        );
        idle_notify.remove(&window_target);
        debug_state.windows.remove(&window_target);
        debug_state.inactive_at.remove(&window_target);
    }
}

/// One window's step of the state machine: seen while active, parked on a
/// pending token, re-armed by output, or fired once it has been idle long
/// enough.
fn tick_window(
    ctx: &TickCtx,
    window_target: &str,
    panes: &[String],
    record: &mut IdleRecord,
    win_dbg: &mut WinDebug,
    inactive_age: Option<f64>,
) {
    let (team_name, workspace, now) = (ctx.team_name, ctx.workspace, ctx.now);
    if window_target == ctx.active_window {
        let state_before = record_state_value(record);
        let was_seen = record.seen_since_fire;
        let was_notified = record.notified;
        record.last_busy_ts = now;
        record.notified = true;
        record.seen_since_fire = true;
        if !was_seen || !was_notified {
            hooked_notify_debug_emit(
                workspace,
                "active.block",
                &[
                    ("team", Value::from(team_name)),
                    ("window", Value::from(window_target)),
                    ("state_before", state_before),
                ],
            );
        }
        return;
    }

    let token = hooked_get_window_option(window_target, ctx.token_key).unwrap_or_default();
    if !token.is_empty() {
        if win_dbg.observed_token.as_deref() != Some(token.as_str()) {
            hooked_notify_debug_emit(
                workspace,
                "token.present",
                &[
                    ("team", Value::from(team_name)),
                    ("window", Value::from(window_target)),
                    ("token", Value::from(token.clone())),
                    (
                        "state_before",
                        notify_state_value(record.notified, record.seen_since_fire),
                    ),
                ],
            );
            win_dbg.observed_token = Some(token.clone());
        }
        record.notified = true;
        record.seen_since_fire = false;
        return;
    }

    if let Some(prev_token) = win_dbg.observed_token.take() {
        hooked_notify_debug_emit(
            workspace,
            "token.cleared_externally",
            &[
                ("team", Value::from(team_name)),
                ("window", Value::from(window_target)),
                ("prev_token", Value::from(prev_token)),
                ("state_before", record_state_value(record)),
            ],
        );
    }

    let busy_panes: Vec<String> = panes
        .iter()
        .filter(|p| _is_output_busy(p, ctx.busy_monitor, inactive_age))
        .cloned()
        .collect();
    let prev_busy = win_dbg.busy_observed;
    let is_busy = !busy_panes.is_empty();
    if is_busy {
        record.last_busy_ts = now;
        let recent = _most_recent_output_pane(&busy_panes, ctx.busy_monitor);
        record.last_busy_pane = Some(if recent.is_empty() {
            busy_panes[busy_panes.len() - 1].clone()
        } else {
            recent
        });
        if prev_busy != is_busy {
            hooked_notify_debug_emit(
                workspace,
                "busy.transition",
                &[
                    ("team", Value::from(team_name)),
                    ("window", Value::from(window_target)),
                    ("busy", Value::Bool(true)),
                    (
                        "busy_panes",
                        Value::Array(busy_panes.iter().cloned().map(Value::from).collect()),
                    ),
                    (
                        "last_busy_pane",
                        record
                            .last_busy_pane
                            .clone()
                            .map(Value::from)
                            .unwrap_or(Value::Null),
                    ),
                ],
            );
        }
        if record.seen_since_fire {
            if record.notified {
                hooked_notify_debug_emit(
                    workspace,
                    "busy.rearm",
                    &[
                        ("team", Value::from(team_name)),
                        ("window", Value::from(window_target)),
                        ("seen_since_fire", Value::Bool(true)),
                    ],
                );
            }
            record.notified = false;
        }
        win_dbg.busy_observed = true;
        return;
    }

    if prev_busy != is_busy {
        hooked_notify_debug_emit(
            workspace,
            "busy.transition",
            &[
                ("team", Value::from(team_name)),
                ("window", Value::from(window_target)),
                ("busy", Value::Bool(false)),
                ("last_busy_ts", Value::from(record.last_busy_ts)),
            ],
        );
    }
    win_dbg.busy_observed = false;

    if now - record.last_busy_ts < IDLE_NOTIFY_THRESHOLD_SECONDS || record.notified {
        return;
    }
    let target_pane = _idle_notify_target_pane(panes, record, ctx.busy_monitor);
    hooked_notify_debug_emit(
        workspace,
        "fire.attempt",
        &[
            ("team", Value::from(team_name)),
            ("window", Value::from(window_target)),
            ("target_pane", Value::from(target_pane.clone())),
            ("idle_seconds", Value::from(now - record.last_busy_ts)),
            (
                "state_before",
                notify_state_value(record.notified, record.seen_since_fire),
            ),
        ],
    );
    let (suppressed, surface) =
        hooked_notify_ui_notify(IDLE_NOTIFY_MESSAGE, &target_pane, workspace);
    record.notified = true;
    record.seen_since_fire = suppressed;
    let new_token = hooked_get_window_option(window_target, ctx.token_key).unwrap_or_default();
    win_dbg.observed_token = if new_token.is_empty() {
        None
    } else {
        Some(new_token.clone())
    };
    hooked_notify_debug_emit(
        workspace,
        "fire.result",
        &[
            ("team", Value::from(team_name)),
            ("window", Value::from(window_target)),
            ("target_pane", Value::from(target_pane)),
            ("surface", surface.map(Value::from).unwrap_or(Value::Null)),
            ("suppressed", Value::Bool(suppressed)),
            (
                "token_after",
                if new_token.is_empty() {
                    Value::Null
                } else {
                    Value::from(new_token)
                },
            ),
            (
                "state_after",
                notify_state_value(record.notified, record.seen_since_fire),
            ),
        ],
    );
}

fn emit_heartbeat(
    ctx: &TickCtx,
    debug_state: &mut NotifyDebugState,
    windows: &BTreeMap<String, Vec<String>>,
    records: usize,
) {
    if ctx.now - debug_state.last_heartbeat < NOTIFY_DEBUG_HEARTBEAT_SECONDS {
        return;
    }
    hooked_notify_debug_emit(
        ctx.workspace,
        "tick.summary",
        &[
            ("team", Value::from(ctx.team_name)),
            ("tick_seq", Value::from(debug_state.tick_seq)),
            (
                "active_window",
                if ctx.active_window.is_empty() {
                    Value::Null
                } else {
                    Value::from(ctx.active_window.clone())
                },
            ),
            (
                "windows",
                Value::Array(windows.keys().cloned().map(Value::from).collect()),
            ),
            ("records", Value::from(records)),
        ],
    );
    debug_state.last_heartbeat = ctx.now;
}
