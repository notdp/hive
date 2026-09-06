// --------------------------------------------------------------------------
// lifecycle
// --------------------------------------------------------------------------

use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::Value;

use crate::devlog;

use super::*;

pub(crate) fn is_tmux_window_alive_impl(tmux_window_id: &str) -> bool {
    crate::tmux::window_exists(tmux_window_id)
}

/// Ensure the team hived socket is alive.
///
/// A hived of this hive home that is another build, api version or team
/// is replaced from this binary. One serving the workspace from another
/// `HIVE_HOME` is refused, not restarted: nothing is spawned and the error
/// names both homes.
pub fn ensure_hived(
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
) -> Result<Option<i32>> {
    let lock_path = lock_path(workspace);
    if let Some(parent) = lock_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let cpath = CString::new(lock_path.as_os_str().as_bytes())?;
    let lock_fd = unsafe { libc::open(cpath.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644) };
    if lock_fd < 0 {
        bail!("cannot open hived lock {}", lock_path.display());
    }
    unsafe { libc::flock(lock_fd, libc::LOCK_EX) };
    let result = (|| {
        let response = hooked_request_ping(workspace);
        match hived_identity(response.as_ref(), team) {
            HivedIdentity::Matches => return Ok(None),
            HivedIdentity::ForeignHome(served) => bail!(
                "hived for {workspace} serves HIVE_HOME {served}, this hive runs with {}",
                crate::paths::hive_home().display()
            ),
            HivedIdentity::Restart => {}
        }
        if response.is_some() {
            stop_hived(workspace);
        }
        hooked_cleanup_socket(workspace);
        let pid = start_hived(workspace, team, tmux_window, tmux_window_id);
        let deadline = monotonic() + SOCKET_READY_TIMEOUT;
        while monotonic() < deadline {
            let response = hooked_request_ping(workspace);
            if hived_identity_matches(response.as_ref(), team) {
                return Ok(pid);
            }
            thread::sleep(Duration::from_secs_f64(SOCKET_RETRY_INTERVAL));
        }
        Ok(pid)
    })();
    unsafe {
        libc::flock(lock_fd, libc::LOCK_UN);
        libc::close(lock_fd);
    }
    result
}

pub(super) fn hooked_current_exe() -> String {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.current_exe.clone()).flatten() {
        return f();
    }
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

pub(crate) fn start_hived(
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
) -> Option<i32> {
    let command = hived_reexec_argv(workspace, team, tmux_window, tmux_window_id);
    let stderr_path = devlog::hived_stderr_path(Path::new(workspace));
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.popen.clone()).flatten() {
        return Some(f(&command, &stderr_path));
    }
    if let Some(parent) = stderr_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let stderr_log = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&stderr_path)
        .ok()?;
    let mut cmd = std::process::Command::new(&command[0]);
    cmd.args(&command[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr_log);
    // Own session: the hived must outlive the terminal of the CLI that spawned it.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().ok()?;
    Some(child.id() as i32)
}

pub fn run_spawned_hived(argv: &[String]) -> i32 {
    if argv.len() != 5 || argv[0] != "--hived" {
        eprintln!("usage: hive --hived <workspace> <team> <tmux_window> <tmux_window_id>");
        return 1;
    }
    hooked_ignore_sigint();
    hooked_hived_loop(&argv[1], &argv[2], &argv[3], &argv[4]);
    0
}

fn hooked_ignore_sigint() {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.ignore_sigint.clone()).flatten() {
        f();
        return;
    }
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
    }
}

fn hooked_hived_loop(workspace: &str, team: &str, tmux_window: &str, tmux_window_id: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.hived_loop.clone()).flatten() {
        f(workspace, team, tmux_window, tmux_window_id);
        return;
    }
    hived_loop(workspace, team, tmux_window, tmux_window_id);
}

fn hooked_make_busy_monitor(session_target: &str) -> Option<Arc<dyn OutputMonitor>> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.make_busy_monitor.clone()).flatten() {
        return f(session_target);
    }
    if session_target.is_empty() {
        return None;
    }
    Some(Arc::new(crate::tmux::ControlModeOutputMonitor::new(
        session_target,
    )))
}

pub(crate) fn hived_loop(workspace: &str, team: &str, tmux_window: &str, tmux_window_id: &str) {
    SHUTDOWN.store(false, Ordering::SeqCst);
    let hived_started_at = now_iso();
    let mut idle_notify: HashMap<String, IdleRecord> = HashMap::new();
    let mut notify_debug_state = NotifyDebugState::default();
    let mut code_reexec_state = ReexecState::default();
    let mut claude_view_state = ClaudeTickState::default();
    let mut status_state = StatusTickState::default();
    // `monotonic()` starts near zero, so a 0.0 seed would skip the first
    // periodic checks; negative infinity makes every one run on the first tick.
    let mut last_window_check = f64::NEG_INFINITY;
    let mut last_owner_check = f64::NEG_INFINITY;
    let mut last_daemon_cleanup = f64::NEG_INFINITY;
    let owner_token = format!(
        "{}:{}",
        getpid(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    hooked_notify_debug_emit(
        workspace,
        "hived.start",
        &[
            ("team", Value::from(team)),
            ("tmux_window", Value::from(tmux_window)),
            ("tmux_window_id", Value::from(tmux_window_id)),
            ("startedAt", Value::from(hived_started_at.clone())),
        ],
    );
    let inherited_reexec_lock_fd = take_reexec_lock_fd_from_env();
    let mut server = match hooked_open_server_socket(workspace) {
        Ok(server) => server,
        Err(err) => {
            // stderr is the hived.stderr log; the notify line is what
            // `hive spawn` / `hive send` surface behind "hived unavailable".
            // A silent exit here once cost hours: an over-long workspace
            // path failed `bind` and every command just said unavailable.
            let socket = socket_path(workspace).display().to_string();
            eprintln!("hived: cannot open server socket {socket}: {err}");
            hooked_notify_debug_emit(
                workspace,
                "hived.socket_bind_failed",
                &[
                    ("team", Value::from(team)),
                    ("socket", Value::from(socket)),
                    ("error", Value::from(err.to_string())),
                ],
            );
            hooked_release_reexec_lock_fd(inherited_reexec_lock_fd);
            return;
        }
    };
    hooked_write_hived_owner(workspace, getpid(), &hived_started_at, &owner_token);
    hooked_release_reexec_lock_fd(inherited_reexec_lock_fd);
    let session_target = tmux_window
        .split_once(':')
        .map(|(session, _)| session)
        .unwrap_or(tmux_window)
        .trim()
        .to_string();
    let busy_monitor = hooked_make_busy_monitor(&session_target);
    set_output_busy_monitor(busy_monitor.clone());
    if let Some(monitor) = busy_monitor.as_ref() {
        monitor.start();
    }

    // Every exit from the loop is a `break`, so the teardown after it runs
    // for all of them.
    loop {
        if !Path::new(workspace).is_dir() {
            break;
        }

        let now = monotonic();
        if now - last_window_check >= 30.0 {
            last_window_check = now;
            // The registry entry is the team's existence; the tmux window
            // is only its display. A dead window alone never retires the
            // hived (engines keep running headless); only a *missing*
            // registry file (`hive delete` removes it) with no display
            // window left behind it does. Corrupt or foreign-instance
            // entries are not "missing": never retire on a read that
            // might be wrong.
            if let Some(path) = crate::registry::entry_path(team) {
                if !path.is_file() && !hooked_is_tmux_window_alive(tmux_window_id) {
                    break;
                }
            }
        }

        if now - last_daemon_cleanup >= 30.0 {
            last_daemon_cleanup = now;
            // Supervision must never take the hived down: every tick below
            // swallows its own errors internally.
            cleanup_dead_daemons(workspace, team);
            codex_supervisor_tick(workspace, team);
            claude_supervisor_tick(workspace);
            write_registry_backfill(workspace, team);
        }

        if now - last_owner_check >= HIVED_OWNER_CHECK_SECONDS {
            last_owner_check = now;
            if let Some(foreign_pid) = foreign_owner_pid(workspace, &owner_token) {
                hooked_notify_debug_emit(
                    workspace,
                    "hived.retire_orphan",
                    &[
                        ("team", Value::from(team)),
                        ("tmux_window", Value::from(tmux_window)),
                        ("tmux_window_id", Value::from(tmux_window_id)),
                        ("currentPid", Value::from(getpid())),
                        ("socketPid", Value::from(foreign_pid)),
                    ],
                );
                break;
            }
        }

        let stale_hash = hooked_stale_disk_build_hash(&mut code_reexec_state, now);
        // Never exec out from under an in-flight request thread: its
        // transport work would die mid-flight with the message already on
        // the bus. The stale hash is still stale 5s later.
        if let Some(stale_hash) = stale_hash.filter(|_| !requests_in_flight()) {
            let emit_reexec = || {
                hooked_notify_debug_emit(
                    workspace,
                    "hived.reexec",
                    &[
                        ("team", Value::from(team)),
                        ("tmux_window", Value::from(tmux_window)),
                        ("tmux_window_id", Value::from(tmux_window_id)),
                        ("oldHash", Value::from(hived_build_hash())),
                        ("newHash", Value::from(stale_hash.clone())),
                    ],
                );
            };
            if let Some(replacement) = reexec_hived(
                workspace,
                team,
                tmux_window,
                tmux_window_id,
                server.as_ref(),
                busy_monitor.as_ref(),
                Some(&emit_reexec),
            ) {
                // exec failed: keep serving the old build on the rebound
                // socket instead of dying with the socket torn down.
                server = replacement;
            }
        }

        let tick_members = hooked_team_member_bindings(team).unwrap_or_default();

        // Job relabelling and border cosmetics must never take the hived
        // down (the tick fns swallow their own failures).
        claude_name_tick(&tick_members, team, &mut claude_view_state);
        claude_view_tick(workspace, team, &tick_members, &mut claude_view_state);
        status_tick(
            workspace,
            &tick_members,
            busy_monitor.as_deref(),
            &mut status_state,
            now_epoch_seconds(),
        );

        if !hooked_serve_requests(
            server.as_ref(),
            workspace,
            team,
            tmux_window,
            tmux_window_id,
            &hived_started_at,
            IDLE_NOTIFY_TICK_SECONDS,
        ) {
            break;
        }

        idle_notify_tick(
            team,
            &session_target,
            &mut idle_notify,
            busy_monitor.as_deref(),
            monotonic(),
            workspace,
            Some(&mut notify_debug_state),
            Some(tick_members.as_slice()),
        );
    }

    if let Some(monitor) = busy_monitor.as_ref() {
        monitor.stop();
    }
    set_output_busy_monitor(None);
    server.close();
    cleanup_socket_if_owner(workspace, &owner_token);
}

fn now_epoch_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

pub fn stop_hived(workspace: &str) {
    let _ = request_hived(workspace, &action_payload("shutdown"), SOCKET_READY_TIMEOUT);
    let deadline = monotonic() + SOCKET_READY_TIMEOUT;
    while monotonic() < deadline {
        if !socket_path(workspace).exists() {
            return;
        }
        thread::sleep(Duration::from_secs_f64(SOCKET_RETRY_INTERVAL));
    }
    hooked_cleanup_socket(workspace);
}
