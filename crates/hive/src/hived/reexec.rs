// --------------------------------------------------------------------------
// build identity
// --------------------------------------------------------------------------

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use super::*;

/// SHA-256 of the binary at `current_exe()` as it sits on disk right now.
/// `hived_build_hash` caches the first result for the process lifetime (the
/// running build); `_stale_disk_build_hash_for_reexec` recomputes it, which
/// is how an install that replaced the file shows up as a different hash.
pub fn _compute_build_hash() -> String {
    let inner = || -> std::io::Result<String> {
        let exe = std::env::current_exe()?;
        let bytes = fs::read(&exe)?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        Ok(hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    };
    inner().unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(not(test))]
pub fn hived_build_hash() -> &'static str {
    static CELL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CELL.get_or_init(_compute_build_hash)
}

/// Hashing the multi-megabyte test binary costs seconds per test process
/// (nextest runs one per test) and no test depends on the real digest: the
/// running build's identity is a constant under test, and
/// `hooked_compute_build_hash` still supplies whatever "disk" hash a reexec
/// test wants to contrast it with.
#[cfg(test)]
pub fn hived_build_hash() -> &'static str {
    "test-build"
}

pub fn _hived_reexec_argv(
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
) -> Vec<String> {
    vec![
        hooked_current_exe(),
        "--hived".to_string(),
        workspace.to_string(),
        team.to_string(),
        tmux_window.to_string(),
        tmux_window_id.to_string(),
    ]
}

/// Per-loop reexec bookkeeping.
#[derive(Debug)]
pub struct ReexecState {
    pub last_code_check_at: f64,
    pub candidate_hash: Option<String>,
}

impl Default for ReexecState {
    fn default() -> Self {
        ReexecState {
            // `monotonic()` starts near zero, so a 0.0 seed would skip the
            // first check; negative infinity makes it run on the first tick.
            last_code_check_at: f64::NEG_INFINITY,
            candidate_hash: None,
        }
    }
}

/// Return a stable changed build hash that should trigger hived reexec.
pub fn _stale_disk_build_hash_for_reexec(state: &mut ReexecState, now: f64) -> Option<String> {
    if now - state.last_code_check_at < HIVED_CODE_CHECK_SECONDS {
        return None;
    }
    state.last_code_check_at = now;

    let disk_hash = hooked_compute_build_hash();
    if disk_hash == "unknown" || disk_hash == hived_build_hash() {
        state.candidate_hash = None;
        return None;
    }

    if state.candidate_hash.as_deref() == Some(disk_hash.as_str()) {
        return Some(disk_hash);
    }
    state.candidate_hash = Some(disk_hash);
    None
}

pub fn _release_reexec_lock_fd_impl(lock_fd: Option<i32>) {
    let Some(fd) = lock_fd else { return };
    unsafe {
        libc::flock(fd, libc::LOCK_UN);
        libc::close(fd);
    }
}

pub fn _try_acquire_reexec_lock_impl(workspace: &str) -> Option<i32> {
    let lock_path = _lock_path(workspace);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).ok()?;
    }
    let cpath = CString::new(lock_path.as_os_str().as_bytes()).ok()?;
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644) };
    if fd < 0 {
        return None;
    }
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        _release_reexec_lock_fd_impl(Some(fd));
        return None;
    }
    // The lock fd rides through execv into the new build: clear FD_CLOEXEC.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        _release_reexec_lock_fd_impl(Some(fd));
        return None;
    }
    Some(fd)
}

pub fn _take_reexec_lock_fd_from_env() -> Option<i32> {
    let raw_fd = std::env::var(_HIVED_REEXEC_LOCK_ENV).unwrap_or_default();
    std::env::remove_var(_HIVED_REEXEC_LOCK_ENV);
    if raw_fd.is_empty() {
        return None;
    }
    raw_fd.parse::<i32>().ok()
}

/// What a (hooked) execv attempt reports back.
#[allow(dead_code)]
pub enum ExecOutcome {
    /// Test-only: the process would have been replaced; unreachable live.
    Replaced,
    Failed(std::io::Error),
}

pub(super) fn _execv_impl(argv: &[String]) -> ExecOutcome {
    let cstrings: Vec<CString> = argv
        .iter()
        .filter_map(|a| CString::new(a.as_str()).ok())
        .collect();
    if cstrings.len() != argv.len() || cstrings.is_empty() {
        return ExecOutcome::Failed(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "bad argv",
        ));
    }
    let mut ptrs: Vec<*const libc::c_char> = cstrings.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    unsafe { libc::execv(cstrings[0].as_ptr(), ptrs.as_ptr()) };
    ExecOutcome::Failed(std::io::Error::last_os_error())
}

/// Replace this process with the on-disk build.
///
/// Returns None when nothing was torn down (another hived holds the reexec
/// lock) — the caller keeps serving on its own socket. When ``execv`` itself
/// fails, the old build has to keep serving rather than leave the window with
/// a dead hived and no socket: the listener is rebound, the output monitor
/// restarted, and the replacement socket returned for the caller to serve on.
pub fn _reexec_hived(
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
    server: &dyn HivedServerApi,
    busy_monitor: Option<&Arc<dyn OutputMonitor>>,
    on_reexec: Option<&dyn Fn()>,
) -> Option<Box<dyn HivedServerApi>> {
    let lock_fd = hooked_try_acquire_reexec_lock(workspace)?;

    let previous_lock_env = std::env::var(_HIVED_REEXEC_LOCK_ENV).ok();
    std::env::set_var(_HIVED_REEXEC_LOCK_ENV, lock_fd.to_string());
    if let Some(monitor) = busy_monitor {
        monitor.stop();
    }
    _set_output_busy_monitor(None);
    server.close();
    hooked_cleanup_socket(workspace);
    if let Some(cb) = on_reexec {
        cb();
    }
    let argv = _hived_reexec_argv(workspace, team, tmux_window, tmux_window_id);
    let outcome = hooked_execv(&argv);
    // Only reached when execv came back (live: it failed; under test: the
    // hook reports Replaced) — undo the env and drop the lock either way.
    match previous_lock_env {
        None => std::env::remove_var(_HIVED_REEXEC_LOCK_ENV),
        Some(previous) => std::env::set_var(_HIVED_REEXEC_LOCK_ENV, previous),
    }
    hooked_release_reexec_lock_fd(Some(lock_fd));
    match outcome {
        ExecOutcome::Replaced => return None,
        ExecOutcome::Failed(exc) => {
            eprintln!(
                "hived: reexec failed ({exc}); staying on build {}",
                &hived_build_hash()[..hived_build_hash().len().min(12)]
            );
        }
    }

    // Only reached when execv failed. Rebinding is the recovery; if it too
    // fails the loop must die through its own teardown — signal shutdown so
    // the next serve tick retires it.
    let replacement = match hooked_open_server_socket(workspace) {
        Ok(replacement) => replacement,
        Err(_) => {
            _SHUTDOWN.store(true, Ordering::SeqCst);
            return None;
        }
    };
    if let Some(monitor) = busy_monitor {
        monitor.start();
        _set_output_busy_monitor(Some(Arc::clone(monitor)));
    }
    Some(replacement)
}
