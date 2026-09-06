//! Notify debug tracing.
//!
//! Always-on JSONL log of notify state-machine transitions, covering both the
//! hived idle watcher and the notify_ui delivery path. Business-path events
//! are always recorded; low-information hived heartbeat events are only
//! recorded when Hive is running from a source checkout or
//! `HIVE_LOG_VERBOSITY=dev`.
//!
//! Logs go to `<workspace>/run/notify.jsonl` when the workspace is known
//! (hived paths, select-hook cleanup with `@hive-workspace`) and fall back to
//! `~/.cache/hive/notify.jsonl` (or `$XDG_CACHE_HOME/hive/...`) when no
//! workspace can be resolved.
//!
//! Hived callers already know their workspace and call `emit(workspace, ..)`
//! directly; notify_ui helpers go through `emit_for_window`, which resolves
//! `@hive-workspace` on the target window only when the hint is empty.
//! `workspace_for_window` failures fall back to the global log silently.
//!
//! Multiple processes (hived loop, select-hook cleanup) write to the same log
//! via a single append write on an `O_APPEND` fd.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use serde_json::Value;

#[cfg(test)]
use self::tests::fake_tmux as tmux;
#[cfg(not(test))]
use crate::tmux;

pub fn log_path(workspace: &str) -> PathBuf {
    crate::devlog::notify_log_path(std::path::Path::new(workspace))
}

pub fn workspace_for_window(window_target: &str) -> String {
    if window_target.is_empty() {
        return String::new();
    }
    tmux::get_window_option(window_target, "hive-workspace")
        .map(|value| value.trim().to_string())
        .unwrap_or_default()
}

/// Emit by window. Pass a non-empty `workspace` to skip the tmux lookup.
pub fn emit_for_window(
    window_target: &str,
    event: &str,
    workspace: &str,
    fields: &[(&str, Value)],
) {
    let workspace = if workspace.is_empty() {
        workspace_for_window(window_target)
    } else {
        workspace.to_string()
    };
    emit(&workspace, event, fields);
}

pub fn emit(workspace: &str, event: &str, fields: &[(&str, Value)]) {
    if !crate::devlog::should_emit(event) {
        return;
    }
    let mut record: Vec<(String, Value)> = vec![
        (
            "ts".to_string(),
            Value::String(crate::devlog::utc_timestamp_ms()),
        ),
        ("pid".to_string(), Value::from(std::process::id())),
        ("component".to_string(), Value::String("notify".to_string())),
        (
            "workspace".to_string(),
            Value::String(if workspace.is_empty() {
                "<global>".to_string()
            } else {
                workspace.to_string()
            }),
        ),
        ("event".to_string(), Value::String(event.to_string())),
    ];
    for (key, value) in fields {
        if value.is_null() {
            continue;
        }
        record.push((key.to_string(), value.clone()));
    }
    let record: serde_json::Map<String, Value> = record.into_iter().collect();
    let mut payload = Value::Object(record).to_string();
    payload.push('\n');

    let path = if workspace.is_empty() {
        crate::devlog::global_notify_log_path()
    } else {
        log_path(workspace)
    };
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let mut handle = match fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(handle) => handle,
        Err(_) => return,
    };
    let _ = handle.write_all(payload.as_bytes());
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    /// Test stand-in for `crate::tmux`. Also used by
    /// `notify_ui` tests to route debug logs into a temp workspace.
    pub mod fake_tmux {
        use std::cell::RefCell;

        thread_local! {
            static WORKSPACE_VALUE: RefCell<Option<String>> = const { RefCell::new(None) };
            static LOOKUPS: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
        }

        pub fn get_window_option(target: &str, key: &str) -> Option<String> {
            LOOKUPS.with(|lookups| {
                lookups
                    .borrow_mut()
                    .push((target.to_string(), key.to_string()))
            });
            WORKSPACE_VALUE.with(|value| value.borrow().clone())
        }

        pub fn set_workspace_value(value: Option<String>) {
            WORKSPACE_VALUE.with(|slot| *slot.borrow_mut() = value);
        }

        pub fn take_lookups() -> Vec<(String, String)> {
            LOOKUPS.with(|lookups| std::mem::take(&mut *lookups.borrow_mut()))
        }

        pub fn reset() {
            set_workspace_value(None);
            let _ = take_lookups();
        }
    }

    fn first_record(path: &std::path::Path) -> Value {
        let text = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(text.lines().next().unwrap()).unwrap()
    }

    #[test]
    fn test_emit_falls_back_to_global_log_when_no_workspace() {
        let mut env = crate::testenv::EnvGuard::new();
        let tmp = TempDir::new().unwrap();
        env.set("XDG_CACHE_HOME", tmp.path());

        emit("", "global.event", &[("payload", json!("x"))]);

        let log = tmp.path().join("hive").join("notify.jsonl");
        let record = first_record(&log);
        assert_eq!(record["event"], "global.event");
        assert_eq!(record["component"], "notify");
        assert_eq!(record["workspace"], "<global>");
        assert_eq!(record["payload"], "x");
        assert_eq!(record["pid"], std::process::id());
    }

    #[test]
    fn test_emit_filters_heartbeat_events_in_normal_mode() {
        let mut env = crate::testenv::EnvGuard::new();
        let tmp = TempDir::new().unwrap();
        env.set("XDG_CACHE_HOME", tmp.path());
        env.set("HIVE_LOG_VERBOSITY", "normal");

        emit("", "tick.summary", &[("team", json!("team-a"))]);

        assert!(!tmp.path().join("hive").join("notify.jsonl").exists());
    }

    #[test]
    fn test_emit_writes_workspace_log_when_workspace_known() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");

        emit(workspace.to_str().unwrap(), "ws.event", &[("a", json!(1))]);

        let record = first_record(&workspace.join("run").join("notify.jsonl"));
        assert_eq!(record["event"], "ws.event");
        assert_eq!(record["component"], "notify");
        assert_eq!(record["workspace"], workspace.to_str().unwrap());
        assert_eq!(record["a"], 1);
    }

    #[test]
    fn test_emit_for_window_uses_passed_workspace() {
        fake_tmux::reset();
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");

        emit_for_window(
            "dev:1",
            "ui.event",
            workspace.to_str().unwrap(),
            &[("payload", json!("x"))],
        );

        let record = first_record(&workspace.join("run").join("notify.jsonl"));
        assert_eq!(record["event"], "ui.event");
        assert_eq!(record["payload"], "x");
        // passed workspace skips lookup
        assert!(fake_tmux::take_lookups().is_empty());
    }

    #[test]
    fn test_emit_for_window_resolves_workspace_when_not_passed() {
        fake_tmux::reset();
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");
        fake_tmux::set_workspace_value(Some(workspace.to_str().unwrap().to_string()));

        emit_for_window("dev:1", "ui.event", "", &[("payload", json!("resolved"))]);

        let record = first_record(&workspace.join("run").join("notify.jsonl"));
        assert_eq!(record["event"], "ui.event");
        assert_eq!(record["payload"], "resolved");
    }
}
