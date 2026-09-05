// --------------------------------------------------------------------------
// busy / transcript machinery
// --------------------------------------------------------------------------

use std::fs;

use serde_json::{Map, Value};

use super::*;

pub fn _fresh_snapshot_session_id_impl(pane_id: &str, now: Option<f64>) -> String {
    let store = runtime_snapshots()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(snapshot) = store.get(pane_id) {
        if !snapshot.sessionId.value.is_empty() && snapshot.sessionId.is_fresh(now) {
            return snapshot.sessionId.value.clone();
        }
    }
    String::new()
}

/// Resolve the agent transcript jsonl path for a pane, with TTL cache.
///
/// Returns the absolute path string, or None when the pane has no
/// associated transcript (non-agent pane, no resolved session, etc.).
/// The cache is keyed by pane_id with a coarse TTL so the underlying
/// rglob in ``adapter.find_session_file`` does not fire on every tick.
///
/// When ``force=true`` the cache is bypassed and re-populated. Callers use
/// this to recover from a session switch (e.g. ``/new``) where the cached
/// path points at the previous session's jsonl that no longer advances.
pub fn _resolve_transcript_path_cached_impl(pane_id: &str, force: bool) -> Option<String> {
    let now = monotonic();
    let snapshot_exists = runtime_snapshots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(pane_id)
        .is_some();
    let fresh_snapshot_session_id = hooked_fresh_snapshot_session_id(pane_id, Some(now));
    if !force {
        let cache = transcript_path_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = cache.get(pane_id) {
            if now < cached.1
                && (!snapshot_exists
                    || (!fresh_snapshot_session_id.is_empty()
                        && cached.2 == fresh_snapshot_session_id))
            {
                return if cached.0.is_empty() {
                    None
                } else {
                    Some(cached.0.clone())
                };
            }
        }
    }

    let mut path_str = String::new();
    let mut sid = String::new();
    if !pane_id.is_empty() && hooked_is_pane_alive(pane_id) {
        if let Some(profile) = hooked_detect_profile_for_pane(pane_id) {
            if let Some(adapter) = hooked_adapters_get(profile.name) {
                sid = fresh_snapshot_session_id;
                if sid.is_empty() {
                    sid = adapter
                        .resolve_current_session_id(pane_id)
                        .unwrap_or_default();
                }
                if !sid.is_empty() {
                    let cwd_hint = hooked_display_value(pane_id, "#{pane_current_path}");
                    if let Some(transcript) = adapter.find_session_file(&sid, cwd_hint.as_deref()) {
                        path_str = transcript.to_string_lossy().to_string();
                    }
                }
            }
        }
    }

    transcript_path_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            pane_id.to_string(),
            (path_str.clone(), now + _TRANSCRIPT_PATH_CACHE_TTL, sid),
        );
    if path_str.is_empty() {
        None
    } else {
        Some(path_str)
    }
}

pub fn _check_mtime_within(path: &str, threshold_seconds: f64) -> Option<bool> {
    let metadata = fs::metadata(path).ok()?;
    let mtime = metadata.modified().ok()?;
    let age = std::time::SystemTime::now()
        .duration_since(mtime)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    Some(age <= threshold_seconds)
}

/// Three-state phantom-redraw gate based on transcript jsonl mtime.
///
/// Returns:
///     Some(true)  — jsonl mtime advanced within threshold (real activity)
///     Some(false) — jsonl mtime is older than threshold (phantom redraw)
///     None        — path could not be determined or stat failed; caller
///                   falls back to the underlying control-mode signal.
pub fn _transcript_progressed_recently_impl(pane_id: &str, threshold_seconds: f64) -> Option<bool> {
    let path = hooked_resolve_transcript_path_cached(pane_id, false)?;
    let progressed = _check_mtime_within(&path, threshold_seconds);
    if progressed != Some(false) {
        return progressed;
    }
    // Stale: cached path may be from a previous session. Re-resolve once.
    let fresh = hooked_resolve_transcript_path_cached(pane_id, true);
    match fresh {
        None => Some(false),
        Some(fresh) if fresh == path => Some(false),
        Some(fresh) => _check_mtime_within(&fresh, threshold_seconds),
    }
}

/// Busy flag from claude's own session registry, or None.
///
/// A bg member pane answers from its job's engine entry; an interactive
/// claude on the pane tty answers from its own registry entry (real TUI
/// sessions report ``status``; headless/desktop ones do not and stay None).
pub fn _claude_registry_busy(pane_id: &str) -> Option<bool> {
    if let Some(job_id) = hooked_cb_job_id_for_pane(pane_id) {
        let engine = hooked_cb_engine_session_for_job(&job_id)?;
        return Some(engine.status == "busy");
    }
    let reported = hooked_cs_session_status(hooked_claude_pid_for_pane(pane_id))?;
    Some(reported.0 == "busy")
}

/// Busy flag from the pane's native runtime source (codex shared
/// app-server via the pane's thread record, grok per-pane leader, claude's
/// own session registry).
///
/// None when no native source holds live state for the pane, which is the
/// signal to fall back to the heuristic monitor source.
pub fn _native_daemon_busy_impl(pane_id: &str) -> Option<bool> {
    if pane_id.is_empty() {
        return None;
    }
    if let Some(rt) = hooked_cas_runtime_for_pane(pane_id) {
        return Some(rt.busy);
    }
    if let Some(rt) = hooked_gl_runtime_for_pane(pane_id) {
        return Some(rt.busy);
    }
    _claude_registry_busy(pane_id)
}

/// Public ``busy`` signal: true when the agent is in mid-turn.
pub fn _pane_is_truly_busy(pane_id: &str, monitor: Option<&dyn OutputMonitor>) -> bool {
    _is_output_busy(pane_id, monitor, None)
}

pub fn _busy_output_payload_impl(pane_id: &str) -> Map<String, Value> {
    let monitor = _get_output_busy_monitor();
    let mut map = Map::new();
    map.insert(
        "busy".to_string(),
        Value::Bool(_pane_is_truly_busy(pane_id, monitor.as_deref())),
    );
    map
}

/// Busy verdict: the native daemon/registry answer when one exists, else
/// the output monitor gated by transcript progress. With an `inactive_age`
/// (idle-notify's window-inactive boundary) output the user already saw
/// while the window was active does not count.
pub fn _is_output_busy(
    pane_id: &str,
    monitor: Option<&dyn OutputMonitor>,
    inactive_age: Option<f64>,
) -> bool {
    if pane_id.is_empty() {
        return false;
    }

    if let Some(app_busy) = hooked_native_daemon_busy(pane_id) {
        return app_busy;
    }

    if let Some(m) = monitor {
        if m.is_busy(pane_id, BUSY_OUTPUT_THRESHOLD_SECONDS) {
            let progressed =
                hooked_transcript_progressed_recently(pane_id, BUSY_OUTPUT_THRESHOLD_SECONDS);
            if progressed != Some(false) {
                let Some(inactive_age) = inactive_age else {
                    return true;
                };
                if let Some(output_age) = m.last_output_age(pane_id) {
                    if output_age < inactive_age {
                        return true;
                    }
                }
            }
        }
    }

    false
}

pub fn _most_recent_output_pane(panes: &[String], monitor: Option<&dyn OutputMonitor>) -> String {
    let Some(monitor) = monitor else {
        return String::new();
    };
    let mut candidates: Vec<(f64, String)> = Vec::new();
    for pane_id in panes {
        if let Some(age) = monitor.last_output_age(pane_id) {
            candidates.push((age, pane_id.clone()));
        }
    }
    candidates
        .into_iter()
        .min_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        })
        .map(|(_, pane)| pane)
        .unwrap_or_default()
}

pub(super) fn _idle_notify_target_pane(
    panes: &[String],
    record: &IdleRecord,
    busy_monitor: Option<&dyn OutputMonitor>,
) -> String {
    if let Some(recorded) = record.last_busy_pane.as_ref() {
        if !recorded.is_empty() && panes.iter().any(|p| p == recorded) {
            return recorded.clone();
        }
    }
    let recent = _most_recent_output_pane(panes, busy_monitor);
    if !recent.is_empty() {
        return recent;
    }
    panes.first().cloned().unwrap_or_default()
}
