//! Developer-facing log paths and verbosity policy.

use std::env;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

pub const RUN_DIR_NAME: &str = "run";
pub const NOTIFY_LOG_NAME: &str = "notify.jsonl";
pub const HIVED_STDERR_NAME: &str = "hived.stderr";
pub const HIVED_SOCKET_NAME: &str = "hived.sock";
pub const CVIM_DIR_NAME: &str = "cvim";

const VERBOSITY_ENV: &str = "HIVE_LOG_VERBOSITY";
const DEV_ONLY_EVENTS: [&str; 3] = ["active.changed", "tick.summary", "windows.changed"];

/// `${XDG_CACHE_HOME:-~/.cache}/hive`: where logs land when no workspace
/// resolves. Computed per call so a changed env is seen.
pub fn global_hive_dir() -> PathBuf {
    let base = match env::var("XDG_CACHE_HOME") {
        Ok(value) => PathBuf::from(value),
        Err(_) => PathBuf::from(env::var("HOME").unwrap_or_default()).join(".cache"),
    };
    base.join("hive")
}

/// `YYYY-MM-DDTHH:MM:SS` of *secs* in UTC, no zone suffix — callers add
/// the `Z` or fractional tail their own record format carries.
fn utc_iso_seconds(secs: u64) -> String {
    let secs = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::gmtime_r(&secs, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year as i64 + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Now as `YYYY-MM-DDTHH:MM:SS` UTC, no zone suffix.
pub fn utc_now_iso_seconds() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    utc_iso_seconds(dur.as_secs())
}

pub fn utc_timestamp_ms() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{}.{:03}Z",
        utc_iso_seconds(dur.as_secs()),
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

/// Where the hived socket is expected by convention: `<run dir>/hived.sock`.
/// This is also the symlink hived leaves behind when the real socket had to
/// move (see `hived_socket_path_in`).
pub fn hived_socket_link_path(run_dir: &Path) -> PathBuf {
    run_dir.join(HIVED_SOCKET_NAME)
}

/// The path hived actually binds and clients actually connect to.
///
/// `sun_path` caps a unix socket path at ~103 bytes on macOS, so a deep
/// workspace (a Claude scratchpad, a nested worktree) cannot host its own
/// socket. Rather than refusing the workspace, the socket relocates to a
/// short, deterministic spot — `/tmp/hive-<uid>/<sha256(run dir)[..12]>/hived.sock`
/// — that every hive process derives from the same run dir, so no lookup
/// table and nothing to persist. Short workspaces keep the in-tree path.
pub fn hived_socket_path_in(run_dir: &Path) -> PathBuf {
    let in_tree = hived_socket_link_path(run_dir);
    if in_tree.as_os_str().len() <= max_socket_path_len() {
        return in_tree;
    }
    relocated_socket_dir(run_dir).join(HIVED_SOCKET_NAME)
}

pub fn hived_socket_path(workspace: &Path) -> PathBuf {
    hived_socket_path_in(&run_dir(workspace))
}

/// True when the socket for this run dir lives outside it.
pub fn hived_socket_is_relocated(run_dir: &Path) -> bool {
    hived_socket_path_in(run_dir) != hived_socket_link_path(run_dir)
}

fn relocated_socket_dir(run_dir: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(run_dir.as_os_str().as_encoded_bytes());
    let digest = format!("{:x}", hasher.finalize());
    PathBuf::from(format!("/tmp/hive-{}", unsafe { libc::getuid() })).join(&digest[..12])
}

/// Longest unix socket path the kernel accepts (`sun_path` minus the NUL):
/// 103 bytes on macOS, 107 on Linux.
pub fn max_socket_path_len() -> usize {
    let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_path.len() - 1
}

/// Refuse a workspace whose hived socket cannot be bound even after
/// relocation — only when `/tmp` itself is out of reach, in practice.
///
/// Before relocation existed, a long workspace made `bind` fail and hived
/// exit silently; every `hive spawn` / `hive send` then reported only
/// "hived unavailable". Checked at workspace init so the failure names the
/// path while the human can still pick another one.
pub fn check_socket_path_len(workspace: &Path) -> Result<(), String> {
    let sock = hived_socket_path(workspace);
    let len = sock.as_os_str().len();
    let max = max_socket_path_len();
    if len <= max {
        return Ok(());
    }
    Err(format!(
        "hived socket path too long for a unix socket: {} is {len} bytes, \
         the limit is {max}; pick a shorter workspace path (e.g. under /tmp)",
        sock.display()
    ))
}

/// The last line hived wrote to its stderr log, for "hived unavailable"
/// errors to carry a reason instead of a bare verdict.
pub fn hived_last_stderr_line(workspace: &Path) -> String {
    std::fs::read_to_string(hived_stderr_path(workspace))
        .ok()
        .and_then(|s| {
            s.lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// `hived unavailable`, plus the socket-path verdict or hived's last stderr
/// line when either explains it.
pub fn hived_unavailable_message(workspace: &Path) -> String {
    if let Err(reason) = check_socket_path_len(workspace) {
        return format!("hived unavailable ({reason})");
    }
    let last = hived_last_stderr_line(workspace);
    if last.is_empty() {
        "hived unavailable".to_string()
    } else {
        format!("hived unavailable (hived.stderr: {last})")
    }
}

/// A missing or empty workspace falls back to the global cache dir.
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

/// Verbosity for the binary at *source*: the env override wins, otherwise a
/// binary running from a cargo `target/` dir beside a `Cargo.toml` is a dev
/// checkout and everything else is an install.
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
    let dev_checkout = source.ancestors().any(|parent| {
        matches!(
            parent.file_name().and_then(|name| name.to_str()),
            Some("target")
        ) && parent.join("../Cargo.toml").exists()
    });
    if dev_checkout {
        "dev"
    } else {
        "normal"
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
    use crate::testenv::EnvGuard;
    use serde_json::json;

    #[test]
    fn test_hived_socket_stays_in_tree_for_a_short_workspace() {
        let ws = Path::new("/tmp/ws");
        assert_eq!(
            hived_socket_path(ws),
            PathBuf::from("/tmp/ws/run/hived.sock")
        );
        assert!(!hived_socket_is_relocated(&run_dir(ws)));
        // exactly at the limit still binds in tree
        let room = max_socket_path_len() - "/run/hived.sock".len();
        let edge = PathBuf::from("/".to_string() + &"y".repeat(room - 1));
        assert_eq!(
            hived_socket_path(&edge).as_os_str().len(),
            max_socket_path_len()
        );
        assert!(!hived_socket_is_relocated(&run_dir(&edge)));
        assert!(check_socket_path_len(&edge).is_ok());
    }

    #[test]
    fn test_hived_socket_relocates_deterministically_for_a_long_workspace() {
        let long = PathBuf::from(format!("/tmp/{}", "x".repeat(max_socket_path_len())));
        let sock = hived_socket_path(&long);
        assert!(
            sock.as_os_str().len() <= max_socket_path_len(),
            "{}",
            sock.display()
        );
        assert!(hived_socket_is_relocated(&run_dir(&long)));
        assert!(
            sock.starts_with(format!("/tmp/hive-{}", unsafe { libc::getuid() })),
            "{}",
            sock.display()
        );
        assert_eq!(sock.file_name().unwrap(), HIVED_SOCKET_NAME);
        // same input, same answer — server and clients never consult each other
        assert_eq!(sock, hived_socket_path(&long));
        // a different workspace lands elsewhere
        let other = PathBuf::from(format!("/tmp/{}", "z".repeat(max_socket_path_len())));
        assert_ne!(sock, hived_socket_path(&other));
        // the in-tree name survives as the symlink location
        assert_eq!(
            hived_socket_link_path(&run_dir(&long)),
            long.join("run/hived.sock")
        );
        // and the workspace is accepted
        assert!(check_socket_path_len(&long).is_ok());
    }

    #[test]
    fn test_hived_unavailable_message_carries_a_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        assert_eq!(hived_unavailable_message(&ws), "hived unavailable");
        std::fs::create_dir_all(run_dir(&ws)).unwrap();
        std::fs::write(
            hived_stderr_path(&ws),
            "boot\nsocket bind failed: EINVAL\n\n",
        )
        .unwrap();
        assert_eq!(
            hived_unavailable_message(&ws),
            "hived unavailable (hived.stderr: socket bind failed: EINVAL)"
        );
    }

    #[test]
    fn test_default_verbosity_is_normal_from_installed_binary() {
        let _env = EnvGuard::cleared(&["HIVE_LOG_VERBOSITY"]);

        let source = Path::new("/usr/local/bin/hive");
        assert_eq!(verbosity_for_source(source), "normal");
    }

    #[test]
    fn test_default_verbosity_is_dev_from_source_checkout() {
        let _env = EnvGuard::cleared(&["HIVE_LOG_VERBOSITY"]);

        assert_eq!(
            verbosity_for_source(&env::current_exe().expect("test binary path")),
            "dev"
        );
    }

    #[test]
    fn test_env_overrides_default_verbosity() {
        let mut env = EnvGuard::new();
        env.set("HIVE_LOG_VERBOSITY", "dev");

        let source = Path::new("/usr/local/bin/hive");
        assert_eq!(verbosity_for_source(source), "dev");
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
