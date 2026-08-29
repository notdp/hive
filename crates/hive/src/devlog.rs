//! Developer-facing log paths and verbosity policy.

use std::env;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

pub const RUN_DIR_NAME: &str = "run";
pub const NOTIFY_LOG_NAME: &str = "notify.jsonl";
pub const HIVED_STDERR_NAME: &str = "hived.stderr";
pub const CVIM_DIR_NAME: &str = "cvim";

const VERBOSITY_ENV: &str = "HIVE_LOG_VERBOSITY";
const DEV_ONLY_EVENTS: [&str; 3] = ["active.changed", "tick.summary", "windows.changed"];

/// Python `GLOBAL_HIVE_DIR` module constant; computed per call here.
pub fn global_hive_dir() -> PathBuf {
    let base = match env::var("XDG_CACHE_HOME") {
        Ok(value) => PathBuf::from(value),
        Err(_) => PathBuf::from(env::var("HOME").unwrap_or_default()).join(".cache"),
    };
    base.join("hive")
}

pub fn utc_timestamp_ms() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::gmtime_r(&secs, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        tm.tm_year as i64 + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        dur.subsec_millis()
    )
}

pub fn run_dir(workspace: &Path) -> PathBuf {
    workspace.join(RUN_DIR_NAME)
}

pub fn notify_log_path(workspace: &Path) -> PathBuf {
    run_dir(workspace).join(NOTIFY_LOG_NAME)
}

pub fn global_notify_log_path() -> PathBuf {
    global_hive_dir().join(NOTIFY_LOG_NAME)
}

pub fn hived_stderr_path(workspace: &Path) -> PathBuf {
    run_dir(workspace).join(HIVED_STDERR_NAME)
}

/// Python signature has `workspace: str | Path = ""`; empty/None falls back to
/// the global cache dir.
pub fn cvim_log_dir(workspace: Option<&Path>) -> PathBuf {
    match workspace {
        Some(ws) if !ws.as_os_str().is_empty() => run_dir(ws).join(CVIM_DIR_NAME),
        _ => global_hive_dir().join(CVIM_DIR_NAME),
    }
}

pub fn log_paths(workspace: &Path) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert(
        "notify".to_string(),
        Value::String(notify_log_path(workspace).to_string_lossy().into_owned()),
    );
    map.insert(
        "hived_stderr".to_string(),
        Value::String(hived_stderr_path(workspace).to_string_lossy().into_owned()),
    );
    map.insert(
        "cvim_dir".to_string(),
        Value::String(cvim_log_dir(Some(workspace)).to_string_lossy().into_owned()),
    );
    map
}

pub fn default_verbosity() -> &'static str {
    let exe = env::current_exe().unwrap_or_default();
    verbosity_for_source(&exe)
}

/// Mirrors Python `default_verbosity`, parameterized on the source path the
/// Python code reads from `__file__` (env override first, then install-mode
/// heuristic: a `site-packages`/`dist-packages` ancestor means "installed").
fn verbosity_for_source(source: &Path) -> &'static str {
    let env_value = env::var(VERBOSITY_ENV)
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    match env_value.as_str() {
        "dev" => return "dev",
        "normal" => return "normal",
        _ => {}
    }
    let installed = source.ancestors().any(|parent| {
        matches!(
            parent.file_name().and_then(|name| name.to_str()),
            Some("site-packages") | Some("dist-packages")
        )
    });
    if installed {
        "normal"
    } else {
        "dev"
    }
}

pub fn should_emit(event: &str) -> bool {
    if !DEV_ONLY_EVENTS.contains(&event) {
        return true;
    }
    default_verbosity() == "dev"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    // HIVE_LOG_VERBOSITY is process-global; serialize the tests that touch it.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_default_verbosity_is_normal_from_site_packages() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        env::remove_var("HIVE_LOG_VERBOSITY");

        let source = Path::new("/venv/lib/python3.11/site-packages/hive/devlog.py");
        assert_eq!(verbosity_for_source(source), "normal");
    }

    #[test]
    fn test_default_verbosity_is_dev_from_source_checkout() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        env::remove_var("HIVE_LOG_VERBOSITY");

        let source = Path::new("/repo/src/hive/devlog.py");
        assert_eq!(verbosity_for_source(source), "dev");
    }

    #[test]
    fn test_env_overrides_default_verbosity() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        env::set_var("HIVE_LOG_VERBOSITY", "dev");

        let source = Path::new("/venv/lib/python3.11/site-packages/hive/devlog.py");
        assert_eq!(verbosity_for_source(source), "dev");
        env::remove_var("HIVE_LOG_VERBOSITY");
    }

    #[test]
    fn test_log_paths_are_workspace_run_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");

        assert_eq!(run_dir(&workspace), workspace.join("run"));
        assert_eq!(
            serde_json::to_value(log_paths(&workspace)).unwrap(),
            json!({
                "notify": workspace.join("run").join("notify.jsonl").to_string_lossy(),
                "hived_stderr": workspace.join("run").join("hived.stderr").to_string_lossy(),
                "cvim_dir": workspace.join("run").join("cvim").to_string_lossy(),
            })
        );
    }
}
