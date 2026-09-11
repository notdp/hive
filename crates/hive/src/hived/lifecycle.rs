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

/// A display probe result that flipped the display's reachability; the
/// loop logs each flip once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayTransition {
    Unreachable,
    Recovered,
}

impl DisplayTransition {
    pub fn event(self) -> &'static str {
        match self {
            DisplayTransition::Unreachable => "display.unreachable",
            DisplayTransition::Recovered => "display.recovered",
        }
    }
}

/// The display probe schedule: every tick while the tmux server answers,
/// doubling from one tick up to `DISPLAY_PROBE_MAX_BACKOFF_SECONDS` while
/// it does not (`no-server` and `unknown` alike: neither lets the display
/// be read right now), reset by the first reachable probe.
#[derive(Debug)]
pub struct DisplayProbe {
    next_at: f64,
    backoff: f64,
    unreachable: bool,
}

impl Default for DisplayProbe {
    fn default() -> Self {
        DisplayProbe::new()
    }
}

impl DisplayProbe {
    pub fn new() -> DisplayProbe {
        DisplayProbe {
            next_at: f64::NEG_INFINITY,
            backoff: IDLE_NOTIFY_TICK_SECONDS,
            unreachable: false,
        }
    }

    pub fn due(&self, now: f64) -> bool {
        now >= self.next_at
    }

    /// Seconds until the next probe, 0 while the display is reachable.
    pub fn next_in(&self, now: f64) -> f64 {
        (self.next_at - now).max(0.0)
    }

    /// Record a probe's status (`tmux::list_panes_all_status`); the
    /// transition when reachability flipped.
    pub fn record(&mut self, status: &str, now: f64) -> Option<DisplayTransition> {
        if status == "ok" {
            self.next_at = f64::NEG_INFINITY;
            self.backoff = IDLE_NOTIFY_TICK_SECONDS;
            return std::mem::replace(&mut self.unreachable, false)
                .then_some(DisplayTransition::Recovered);
        }
        self.next_at = now + self.backoff;
        self.backoff = (self.backoff * 2.0).min(DISPLAY_PROBE_MAX_BACKOFF_SECONDS);
        (!std::mem::replace(&mut self.unreachable, true)).then_some(DisplayTransition::Unreachable)
    }
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
        let response = hooked_request_ping(workspace, IDENTITY_PING_TIMEOUT);
        match hived_identity(response.as_ref(), team) {
            HivedIdentity::Matches => return Ok(None),
            HivedIdentity::ForeignHome(served) => bail!(
                "hived for {workspace} serves HIVE_HOME {served}, this hive runs with {}",
                crate::paths::hive_home().display()
            ),
            HivedIdentity::Restart => {}
        }
        if response.is_some() {
            // The retiring owner takes the same lock for cleanup. Do not
            // hold it while waiting for its admission/operation drain.
            unsafe {
                libc::flock(lock_fd, libc::LOCK_UN);
            }
            let stopped = stop_hived_generation(
                workspace,
                response.as_ref().and_then(|r| r.get("hived")).cloned(),
                false,
            );
            unsafe {
                libc::flock(lock_fd, libc::LOCK_EX);
            }
            if stopped == StopOutcome::Deferred {
                return Ok(None);
            }
            let response = hooked_request_ping(workspace, IDENTITY_PING_TIMEOUT);
            if hived_identity_matches(response.as_ref(), team) {
                return Ok(None);
            }
            if stopped == StopOutcome::TimedOut
                && response
                    .as_ref()
                    .and_then(|r| r.get("team"))
                    .and_then(Value::as_str)
                    == Some(team)
            {
                if let HivedIdentity::ForeignHome(home) = hived_identity(response.as_ref(), team) {
                    bail!("hived now serves another hive home: {home}");
                }
                return Ok(None);
            }
            if stopped != StopOutcome::Stopped || response.is_some() {
                bail!("hived is draining; retry after accepted operations finish");
            }
        }
        if std::os::unix::net::UnixStream::connect(socket_path(workspace)).is_ok() {
            bail!(
                "hived socket still accepts connections; refusing to replace an unresponsive owner"
            );
        }
        hooked_cleanup_socket(workspace);
        let pid = start_hived(workspace, team, tmux_window, tmux_window_id);
        let deadline = monotonic() + SOCKET_READY_TIMEOUT;
        while monotonic() < deadline {
            let response = hooked_request_ping(workspace, SOCKET_RETRY_INTERVAL);
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

/// Whether this process may serve *team*: its registry entry must exist
/// under this process's hive home. A hived that cannot see the registry
/// owns nothing and can only do harm — `cleanup_dead_daemons` would read
/// every engine of the team as an orphan and reap it, and every team load
/// would fail — so it must not run ticks at all.
pub(crate) fn registry_visible(team: &str) -> std::result::Result<(), String> {
    let home = crate::paths::hive_home();
    match crate::registry::entry_path(team) {
        Some(path) if path.is_file() => Ok(()),
        Some(path) => Err(format!(
            "no registry entry for team '{team}' at {} (HIVE_HOME {})",
            path.display(),
            home.display()
        )),
        None => Err(format!("'{team}' is not a team name")),
    }
}

pub fn run_spawned_hived(argv: &[String]) -> i32 {
    if argv.len() != 5 || argv[0] != "--hived" {
        eprintln!("usage: hive --hived <workspace> <team> <tmux_window> <tmux_window_id>");
        return 1;
    }
    if let Err(reason) = registry_visible(&argv[2]) {
        // stderr is the hived.stderr log under the workspace run dir.
        eprintln!("hived: refusing to serve {}: {reason}", argv[1]);
        return 2;
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

fn hooked_make_busy_monitor(
    session_target: &str,
    workspace: &str,
) -> Option<Arc<dyn OutputMonitor>> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.make_busy_monitor.clone()).flatten() {
        return f(session_target);
    }
    if session_target.is_empty() {
        return None;
    }
    Some(Arc::new(crate::tmux::ControlModeOutputMonitor::new(
        session_target,
        workspace,
    )))
}

pub(crate) fn hived_loop(workspace: &str, team: &str, tmux_window: &str, tmux_window_id: &str) {
    SHUTDOWN.store(false, Ordering::SeqCst);
    FORCE_SHUTDOWN.store(false, Ordering::SeqCst);
    reopen_admission();
    let hived_started_at = now_iso();
    let mut retirement_reason = "shutdown";
    let mut idle_notify: HashMap<String, IdleRecord> = HashMap::new();
    let mut notify_debug_state = NotifyDebugState::default();
    let mut code_reexec_state = ReexecState::default();
    let mut claude_view_state = ClaudeTickState::default();
    let mut status_state = StatusTickState::default();
    let mut display = DisplayProbe::new();
    let mut sleep = SleepState::for_window(tmux_window);
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
    sleep::clear_asleep_marker(workspace);
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
    let start_serving = |server| {
        RequestServer::start(
            server,
            workspace,
            team,
            tmux_window,
            tmux_window_id,
            &hived_started_at,
        )
    };
    let mut server = match hooked_open_server_socket(workspace).and_then(start_serving) {
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
    let busy_monitor = hooked_make_busy_monitor(&session_target, workspace);
    set_output_busy_monitor(busy_monitor.clone());
    if let Some(monitor) = busy_monitor.as_ref() {
        monitor.start();
    }

    // Every exit from the loop is a `break`, so the teardown after it runs
    // for all of them.
    loop {
        if !Path::new(workspace).is_dir() {
            retirement_reason = "workspace removed";
            break;
        }

        let now = monotonic();
        if now - last_window_check >= 30.0 {
            last_window_check = now;
            // The registry entry is the team's existence; the tmux window
            // is only its display. A dead window starts the idle sleep
            // check below; a missing registry file (`hive delete` removes
            // it) with no display
            // window left behind it does. Corrupt or foreign-instance
            // entries are not "missing": never retire on a read that
            // might be wrong.
            if let Some(path) = crate::registry::entry_path(team) {
                if !path.is_file() && !hooked_is_tmux_window_alive(tmux_window_id) {
                    retirement_reason = "team removed";
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
            super::succession::reconcile_successions(workspace, team);
        }

        if now - last_owner_check >= HIVED_OWNER_CHECK_SECONDS {
            last_owner_check = now;
            if let Some(foreign_pid) = foreign_owner_pid(workspace, &owner_token) {
                retirement_reason = "hived replaced";
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
        flush_operations(workspace);
        if let Some(stale_hash) = stale_hash {
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
                match start_serving(replacement) {
                    Ok(replacement) => server = replacement,
                    Err(err) => {
                        eprintln!("hived: cannot restart accept worker: {err}");
                        SHUTDOWN.store(true, Ordering::SeqCst);
                    }
                }
            }
        }

        // One display snapshot per tick: while the tmux server answers, it
        // is what every display-dependent tick reads (pane liveness,
        // windows, CLIs, tokens are lookups into it); while it does not,
        // those ticks are skipped and the probe backs off. The accept worker
        // serves independently of this sampling and maintenance.
        let snap = if display.due(now) {
            let snap = TickSnapshot::collect();
            if let Some(transition) = display.record(snap.status, now) {
                hooked_notify_debug_emit(
                    workspace,
                    transition.event(),
                    &[
                        ("team", Value::from(team)),
                        ("status", Value::from(snap.status)),
                        ("nextProbeSeconds", Value::from(display.next_in(now))),
                    ],
                );
            }
            Some(snap).filter(TickSnapshot::reachable)
        } else {
            None
        };
        let tick_members = snap.as_ref().map(|snap| {
            let tick_members = hooked_team_member_bindings(team, snap).unwrap_or_default();
            // Job relabelling and border cosmetics must never take the hived
            // down (the tick fns swallow their own failures).
            claude_name_tick(&tick_members, team, &mut claude_view_state);
            claude_view_tick(
                workspace,
                team,
                &tick_members,
                &mut claude_view_state,
                &snap.panes,
            );
            status_tick(
                workspace,
                &tick_members,
                busy_monitor.as_deref(),
                &mut status_state,
                now_epoch_seconds(),
                snap,
            );
            tick_members
        });

        if !hooked_wait_tick(IDLE_NOTIFY_TICK_SECONDS) {
            if finish_shutdown(workspace, server.as_ref(), Duration::from_secs(5)) {
                break;
            }
            continue;
        }

        if let (Some(snap), Some(tick_members)) = (snap.as_ref(), tick_members.as_deref()) {
            idle_notify_tick(
                team,
                &session_target,
                &mut idle_notify,
                busy_monitor.as_deref(),
                monotonic(),
                workspace,
                Some(&mut notify_debug_state),
                Some(tick_members),
                snap,
            );
        }
        if sleep.tick(
            workspace,
            team,
            tmux_window_id,
            snap.as_ref(),
            server.as_ref(),
            monotonic(),
        ) {
            break;
        }
    }

    if let Some(monitor) = busy_monitor.as_ref() {
        monitor.stop();
    }
    set_output_busy_monitor(None);
    close_admission();
    if !SHUTDOWN.load(Ordering::SeqCst) && Path::new(workspace).is_dir() {
        interrupt_operations(workspace, retirement_reason);
    }
    server.close();
    // ensure releases this lock before requesting shutdown; competing
    // starters cannot bind between our owner check and unlink.
    loop {
        if let Some(fd) = hooked_try_acquire_reexec_lock(workspace) {
            cleanup_socket_if_owner(workspace, &owner_token);
            hooked_release_reexec_lock_fd(Some(fd));
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn now_epoch_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

pub(super) fn drain_ready(workspace: &str) -> bool {
    close_admission() && flush_operations(workspace)
}

/// Await only already accepted request handlers. A late node dispatch or a
/// slow handler cancels graceful retirement; forced deletion has a deadline.
pub(super) fn finish_shutdown(
    workspace: &str,
    server: &dyn HivedServerApi,
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while requests_in_flight() && std::time::Instant::now() < deadline {
        reject_draining_request(server);
    }
    if FORCE_SHUTDOWN.load(Ordering::SeqCst) {
        interrupt_operations(workspace, "forced shutdown");
        return true;
    }
    if requests_in_flight() || pending_operations(workspace) > 0 {
        SHUTDOWN.store(false, Ordering::SeqCst);
        reopen_admission();
        return false;
    }
    true
}

pub fn stop_hived(workspace: &str) {
    if stop_hived_generation(workspace, None, true) == StopOutcome::TimedOut {
        eprintln!("hived did not exit within {SOCKET_READY_TIMEOUT}s");
    }
}

/// Ask the hived to retire gracefully: true when it is gone, false when it
/// declined (a node result pending) or did not leave in time.
pub(crate) fn stop_hived_graceful(workspace: &str) -> bool {
    stop_hived_generation(workspace, None, false) == StopOutcome::Stopped
}

#[derive(PartialEq, Eq)]
enum StopOutcome {
    Stopped,
    Deferred,
    TimedOut,
}

fn stop_hived_generation(workspace: &str, expected: Option<Value>, force: bool) -> StopOutcome {
    let mut request = action_payload("shutdown");
    request.insert("force".into(), Value::Bool(force));
    if let Some(expected) = expected {
        request.insert("expectedHived".into(), expected);
    }
    let response = request_hived(workspace, &request, SOCKET_READY_TIMEOUT);
    if response.as_ref().and_then(|r| r.get("draining")) == Some(&Value::Bool(true)) {
        return StopOutcome::Deferred;
    }
    if response.as_ref().and_then(|r| r.get("generationChanged")) == Some(&Value::Bool(true)) {
        return StopOutcome::Stopped;
    }
    let deadline = monotonic() + SOCKET_READY_TIMEOUT;
    while monotonic() < deadline {
        if !socket_path(workspace).exists() {
            return StopOutcome::Stopped;
        }
        thread::sleep(Duration::from_secs_f64(SOCKET_RETRY_INTERVAL));
    }
    StopOutcome::TimedOut
}
