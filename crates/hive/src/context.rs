//! Persisted Hive CLI context for standalone skill usage.
//!
//! Context is stored **per tmux pane** so that multiple agents in the same
//! window don't overwrite each other's identity. When TMUX_PANE is not set
//! (e.g. outside tmux) the file falls back to `default.json`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{json, Value};

// Python module constants HIVE_HOME / CONTEXT_DIR / CURRENT_CONTEXT_FILE are
// env-derived, so they are functions here (read per call, not frozen at import).

pub fn hive_home() -> PathBuf {
    let home = std::env::var("HIVE_HOME")
        .unwrap_or_else(|_| format!("{}/.hive", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(home)
}

pub fn context_dir() -> PathBuf {
    hive_home().join("contexts")
}

/// Legacy single-file path kept for `clear_current_context` cleanup.
pub fn current_context_file() -> PathBuf {
    hive_home().join("current.json")
}

fn pane_slug(pane_id: &str) -> String {
    if pane_id.is_empty() {
        "default".to_string()
    } else {
        pane_id.replace('%', "pane-")
    }
}

/// The per-pane context file path.
///
/// Pane identity routes through the tmux facade so a member engine's tool
/// subprocess — codex (whose env TMUX_PANE is the shared daemon's frozen
/// value, stripped by hive) or a claude bg engine (which has none at all) —
/// still resolves its own pane via its thread/job record.
pub fn _context_file() -> PathBuf {
    let pane = crate::tmux::get_current_pane_id().unwrap_or_default();
    context_dir().join(format!("{}.json", pane_slug(&pane)))
}

/// Python truthy `str(v)`, or None for a falsy value.
// ponytail: real payloads are all-string; container reprs use JSON, not
// Python repr (never occurs in written context files).
fn truthy_str(v: &Value) -> Option<String> {
    match v {
        Value::Null | Value::Bool(false) => None,
        Value::Bool(true) => Some("True".to_string()),
        Value::String(s) => {
            if s.is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }
        Value::Number(n) => {
            if n.as_f64() == Some(0.0) {
                None
            } else {
                Some(n.to_string())
            }
        }
        Value::Array(a) => {
            if a.is_empty() {
                None
            } else {
                Some(v.to_string())
            }
        }
        Value::Object(o) => {
            if o.is_empty() {
                None
            } else {
                Some(v.to_string())
            }
        }
    }
}

fn read_context_map(path: &Path) -> Option<HashMap<String, String>> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let obj = value.as_object()?;
    let mut out = HashMap::new();
    for (k, v) in obj {
        if let Some(s) = truthy_str(v) {
            out.insert(k.clone(), s);
        }
    }
    Some(out)
}

pub fn load_current_context() -> HashMap<String, String> {
    let path = _context_file();
    if !path.exists() {
        // Migrate from legacy single-file if it exists
        let legacy = current_context_file();
        if legacy.exists() {
            if let Some(map) = read_context_map(&legacy) {
                return map;
            }
        }
        return HashMap::new();
    }
    read_context_map(&path).unwrap_or_default()
}

fn write_context(path: PathBuf, team: &str, workspace: &str, agent: &str) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let payload = json!({
        "team": team,
        "workspace": workspace,
        "agent": agent,
    });
    let mut text = serde_json::to_string_pretty(&payload)?;
    text.push('\n');
    fs::write(&path, text)?;
    Ok(path)
}

pub fn save_current_context(team: &str, workspace: &str, agent: &str) -> Result<PathBuf> {
    write_context(_context_file(), team, workspace, agent)
}

/// Write context for an arbitrary pane (used by hive create to pre-bind agents).
pub fn save_context_for_pane(
    pane_id: &str,
    team: &str,
    workspace: &str,
    agent: &str,
) -> Result<PathBuf> {
    let path = context_dir().join(format!("{}.json", pane_slug(pane_id)));
    let written = write_context(path, team, workspace, agent)?;
    _prune_dead_pane_contexts(&written, _live_pane_ids());
    Ok(written)
}

/// The live pane listing the prune trusts, or None when there is none to
/// trust.
///
/// The one seam of the prune, so a test drives `save_context_for_pane`
/// itself rather than the private prune underneath it. A test that installs
/// no listing gets None — a unit test never queries the real tmux server,
/// and "no listing" is the answer that prunes nothing.
fn _live_pane_ids() -> Option<Vec<String>> {
    #[cfg(test)]
    {
        tests::mocked_live_pane_ids()
    }
    #[cfg(not(test))]
    {
        let (panes, status) = crate::tmux::list_panes_all_status();
        if status != "ok" {
            return None;
        }
        Some(panes?.into_iter().map(|p| p.pane_id).collect())
    }
}

/// Drop `pane-*.json` siblings whose pane tmux no longer lists.
///
/// One write leaves one file per pane forever otherwise — a long-lived
/// `$HIVE_HOME` accumulates thousands. The listing is the only authority:
/// a failed or empty one proves no pane dead and prunes nothing, and the
/// file just written is kept whatever it says.
fn _prune_dead_pane_contexts(keep: &Path, live_panes: Option<Vec<String>>) {
    let Some(live) = live_panes.filter(|panes| !panes.is_empty()) else {
        return;
    };
    let live: std::collections::HashSet<String> = live.into_iter().collect();
    let Ok(entries) = fs::read_dir(context_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(slug) = name
            .strip_suffix(".json")
            .and_then(|s| s.strip_prefix("pane-"))
        else {
            continue; // default.json and anything not a pane context
        };
        if !live.contains(&format!("%{slug}")) {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Remove a pane's saved context (registration rollback).
pub fn clear_context_for_pane(pane_id: &str) {
    let path = context_dir().join(format!("{}.json", pane_slug(pane_id)));
    let _ = fs::remove_file(path);
}

pub fn clear_current_context() -> Result<()> {
    let path = _context_file();
    if path.exists() {
        fs::remove_file(&path)?;
    }
    // Also clean up legacy file
    let legacy = current_context_file();
    if legacy.exists() {
        fs::remove_file(&legacy)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::TEST_ENV_LOCK;
    use std::cell::RefCell;
    use std::sync::MutexGuard;

    /// Pin the env inputs of `crate::tmux::get_current_pane_id` so no member
    /// marker or live tmux server is consulted: with $TMUX set, the pinned
    /// TMUX_PANE probe is skipped and the env var is the answer.
    fn setup(pane: Option<&str>) -> (tempfile::TempDir, MutexGuard<'static, ()>) {
        let guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path());
        std::env::remove_var("CODEX_THREAD_ID");
        std::env::remove_var("CLAUDE_CODE_MESSAGING_SOCKET");
        std::env::set_var("TMUX", "test-isolation");
        match pane {
            Some(p) => std::env::set_var("TMUX_PANE", p),
            None => std::env::remove_var("TMUX_PANE"),
        }
        (tmp, guard)
    }

    thread_local! {
        static LIVE_PANES: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
    }

    /// The listing `_live_pane_ids` answers with under test.
    pub(super) fn mocked_live_pane_ids() -> Option<Vec<String>> {
        LIVE_PANES.with(|p| p.borrow().clone())
    }

    fn set_live_panes(panes: &[&str]) {
        let panes: Vec<String> = panes.iter().map(|p| p.to_string()).collect();
        LIVE_PANES.with(|p| *p.borrow_mut() = Some(panes));
    }

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn test_context_file_uses_tmux_pane_slug() {
        let (tmp, _guard) = setup(Some("%12"));
        assert_eq!(
            _context_file(),
            tmp.path().join("contexts").join("pane-12.json")
        );
    }

    #[test]
    fn test_context_file_falls_back_to_default_without_tmux() {
        let (tmp, _guard) = setup(None);
        assert_eq!(
            _context_file(),
            tmp.path().join("contexts").join("default.json")
        );
    }

    #[test]
    fn test_save_and_load_current_context_round_trip() {
        let (tmp, _guard) = setup(None);

        let path = save_current_context("team-a", "/tmp/ws", "claude").unwrap();

        assert_eq!(path, tmp.path().join("contexts").join("default.json"));
        assert_eq!(
            load_current_context(),
            map(&[
                ("team", "team-a"),
                ("workspace", "/tmp/ws"),
                ("agent", "claude")
            ])
        );
    }

    #[test]
    fn test_load_current_context_filters_empty_values() {
        let (tmp, _guard) = setup(None);
        let dir = tmp.path().join("contexts");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("default.json"),
            r#"{"team": "team-a", "workspace": "", "agent": null}"#,
        )
        .unwrap();

        assert_eq!(load_current_context(), map(&[("team", "team-a")]));
    }

    #[test]
    fn test_load_current_context_uses_legacy_file_when_pane_file_missing() {
        let (tmp, _guard) = setup(None);
        fs::write(
            tmp.path().join("current.json"),
            r#"{"team": "team-a", "workspace": "/tmp/ws", "agent": "gpt"}"#,
        )
        .unwrap();

        assert_eq!(
            load_current_context(),
            map(&[
                ("team", "team-a"),
                ("workspace", "/tmp/ws"),
                ("agent", "gpt")
            ])
        );
    }

    #[test]
    fn test_load_current_context_returns_empty_on_invalid_json() {
        let (tmp, _guard) = setup(None);
        let dir = tmp.path().join("contexts");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("default.json"), "not-json").unwrap();

        assert_eq!(load_current_context(), HashMap::new());
    }

    #[test]
    fn test_save_context_for_pane_writes_named_file() {
        let (tmp, _guard) = setup(None);

        let path = save_context_for_pane("%77", "team-a", "/tmp/ws", "alpha").unwrap();

        assert_eq!(path, tmp.path().join("contexts").join("pane-77.json"));
        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            written,
            json!({"team": "team-a", "workspace": "/tmp/ws", "agent": "alpha"})
        );
    }

    #[test]
    fn test_clear_current_context_removes_pane_and_legacy_files() {
        let (tmp, _guard) = setup(Some("%9"));
        let pane_file = tmp.path().join("contexts").join("pane-9.json");
        fs::create_dir_all(pane_file.parent().unwrap()).unwrap();
        fs::write(&pane_file, "{}").unwrap();
        fs::write(tmp.path().join("current.json"), "{}").unwrap();

        clear_current_context().unwrap();

        assert!(!pane_file.exists());
        assert!(!tmp.path().join("current.json").exists());
    }

    #[test]
    fn test_save_context_for_pane_prunes_the_dead_siblings_it_writes_next_to() {
        // The whole wiring: the write goes through the public entry point
        // and the prune rides along on the listing the seam answers with.
        let (tmp, _guard) = setup(None);
        let dir = tmp.path().join("contexts");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("pane-1.json"), "{}").unwrap();
        fs::write(dir.join("pane-2.json"), "{}").unwrap();
        set_live_panes(&["%1", "%77"]);

        let written = save_context_for_pane("%77", "team-a", "/tmp/ws", "alpha").unwrap();

        assert!(written.exists());
        assert!(dir.join("pane-1.json").exists(), "live sibling pruned");
        assert!(!dir.join("pane-2.json").exists(), "dead sibling kept");
    }

    #[test]
    fn test_prune_dead_pane_contexts_removes_only_panes_tmux_no_longer_lists() {
        let (tmp, _guard) = setup(None);
        let dir = tmp.path().join("contexts");
        fs::create_dir_all(&dir).unwrap();
        for name in ["pane-1.json", "pane-2.json", "pane-99.json", "default.json"] {
            fs::write(dir.join(name), "{}").unwrap();
        }
        let keep = dir.join("pane-99.json");

        _prune_dead_pane_contexts(&keep, Some(vec!["%1".to_string(), "%7".to_string()]));

        assert!(dir.join("pane-1.json").exists()); // live
        assert!(!dir.join("pane-2.json").exists()); // gone
        assert!(keep.exists()); // just written, whatever the listing says
        assert!(dir.join("default.json").exists()); // not a pane context
    }

    #[test]
    fn test_prune_dead_pane_contexts_keeps_everything_without_a_listing() {
        let (tmp, _guard) = setup(None);
        let dir = tmp.path().join("contexts");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("pane-2.json"), "{}").unwrap();
        let keep = dir.join("pane-99.json");

        // failed listing, then a listing of nothing: neither proves a pane dead
        _prune_dead_pane_contexts(&keep, None);
        assert!(dir.join("pane-2.json").exists());
        _prune_dead_pane_contexts(&keep, Some(Vec::new()));
        assert!(dir.join("pane-2.json").exists());
    }
}
