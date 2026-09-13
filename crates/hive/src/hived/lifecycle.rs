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
use serde_json::{Map, Value};

use crate::devlog;

use super::*;

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

/// How long a starting CLI waits for `run/hived.lock`, and how often it
/// retries inside that budget. A leaked holder must be a loud failure, not
/// an unbounded hang: nothing here can name the process that holds a flock.
pub const STARTUP_LOCK_TIMEOUT: f64 = 5.0;
pub const STARTUP_LOCK_RETRY_INTERVAL: f64 = 0.02;

/// The `run/hived.lock` descriptor a starting CLI holds while it decides
/// whether to replace the team's hived.
///
/// The fd is close-on-exec. `start_hived` spawns the hived while this lock
/// is held, and a descriptor riding into that child would keep the lock for
/// as long as the child (or anything it spawns) lived: every later
/// `ensure_hived` would then wait on a holder nothing can identify, and the
/// hived's own retirement would deadlock against itself. The reexec handoff
/// fd is the opposite contract — deliberately inheritable, opened separately
/// by `try_acquire_reexec_lock` and released by the generation that inherits
/// it.
pub struct StartupLock {
    fd: i32,
    path: std::path::PathBuf,
    held: bool,
}

impl StartupLock {
    /// Open the workspace's startup lock close-on-exec and take it within
    /// `STARTUP_LOCK_TIMEOUT`.
    pub fn acquire(workspace: &str) -> Result<StartupLock> {
        let path = lock_path(workspace);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let cpath = CString::new(path.as_os_str().as_bytes())?;
        let fd = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_CLOEXEC,
                0o644,
            )
        };
        if fd < 0 {
            bail!(
                "cannot open hived lock {} ({})",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
        let mut lock = StartupLock {
            fd,
            path,
            held: false,
        };
        lock.reacquire()?;
        Ok(lock)
    }

    /// Take the lock again on the same descriptor, on a fresh budget.
    pub(crate) fn reacquire(&mut self) -> Result<()> {
        let deadline = monotonic() + STARTUP_LOCK_TIMEOUT;
        loop {
            match hooked_flock_nb(self.fd) {
                Ok(()) => {
                    self.held = true;
                    return Ok(());
                }
                // EINTR retries on the same deadline; a busy lock is the
                // only other reason to keep trying.
                Err(errno)
                    if errno == libc::EWOULDBLOCK
                        || errno == libc::EAGAIN
                        || errno == libc::EINTR => {}
                Err(errno) => bail!(
                    "cannot lock hived lock {} ({})",
                    self.path.display(),
                    std::io::Error::from_raw_os_error(errno)
                ),
            }
            if monotonic() >= deadline {
                bail!(
                    "hived lock {} is still held after {STARTUP_LOCK_TIMEOUT}s; \
                     another starter or a process that inherited it has not released it",
                    self.path.display()
                );
            }
            thread::sleep(Duration::from_secs_f64(STARTUP_LOCK_RETRY_INTERVAL));
        }
    }

    /// Drop the lock but keep the descriptor: the retiring owner takes the
    /// same lock for its cleanup, so it must not be held across a stop.
    pub(crate) fn release(&mut self) {
        if std::mem::replace(&mut self.held, false) {
            unsafe {
                libc::flock(self.fd, libc::LOCK_UN);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn raw_fd(&self) -> i32 {
        self.fd
    }
}

impl Drop for StartupLock {
    fn drop(&mut self) {
        self.release();
        unsafe {
            libc::close(self.fd);
        }
    }
}

/// Ensure the team hived socket is alive.
///
/// A hived of this hive home that is another build, api version or team
/// is replaced from this binary. One serving the workspace from another
/// `HIVE_HOME` is refused, not restarted: nothing is spawned and the error
/// names both homes. One that is retiring or draining (`Busy`) is asked
/// again within the identity budget, never replaced on that answer alone.
///
/// A stale build that declines the graceful stop, or does not leave in
/// time, keeps serving only while it speaks this binary's api: the caller
/// hears which build answers and why. A different api is refused outright,
/// so no payload the two builds might read differently goes out.
///
/// The error cases are all loud: a startup lock that cannot be taken within
/// its budget, a hived this hive must not touch, one that stayed busy for
/// the whole budget, and a spawned hived that never answered a matching
/// ping.
pub fn ensure_hived(
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
) -> Result<Option<i32>> {
    let mut lock = StartupLock::acquire(workspace)?;
    let (response, identity) = identity_ping(workspace, team)?;
    match identity {
        HivedIdentity::Matches => return Ok(None),
        HivedIdentity::ForeignHome(served) => bail!(
            "hived for {workspace} serves HIVE_HOME {served}, this hive runs with {}",
            crate::paths::hive_home().display()
        ),
        HivedIdentity::Restart | HivedIdentity::Busy => {}
    }
    if response.is_some() {
        // The retiring owner takes the same lock for cleanup. Do not
        // hold it while waiting for its admission/operation drain.
        lock.release();
        let stopped = stop_hived_generation(
            workspace,
            response.as_ref().and_then(|r| r.get("hived")).cloned(),
            false,
        );
        lock.reacquire()?;
        let (response, identity) = identity_ping(workspace, team)?;
        if identity == HivedIdentity::Matches {
            return Ok(None);
        }
        match stopped {
            StopOutcome::Deferred => {
                keep_old_generation(
                    response.as_ref(),
                    team,
                    "it holds node operations and keeps serving until they finish",
                )?;
                return Ok(None);
            }
            StopOutcome::TimedOut if response.is_some() => {
                keep_old_generation(
                    response.as_ref(),
                    team,
                    "it did not leave within the stop budget and keeps serving",
                )?;
                return Ok(None);
            }
            StopOutcome::Stopped if response.is_none() => {}
            _ => bail!("hived is draining; retry after accepted operations finish"),
        }
    }
    if std::os::unix::net::UnixStream::connect(socket_path(workspace)).is_ok() {
        bail!("hived socket still accepts connections; refusing to replace an unresponsive owner");
    }
    hooked_cleanup_socket(workspace);
    let pid = start_hived(workspace, team, tmux_window, tmux_window_id);
    let deadline = monotonic() + SOCKET_READY_TIMEOUT;
    loop {
        let response = hooked_request_ping(workspace, SOCKET_RETRY_INTERVAL);
        if hived_identity_matches(response.as_ref(), team) {
            return Ok(pid);
        }
        if monotonic() >= deadline {
            break;
        }
        thread::sleep(Duration::from_secs_f64(SOCKET_RETRY_INTERVAL));
    }
    // The spawned hived may still come up; killing it here would race a
    // generation that is about to be correct, and unlinking its socket
    // without its owner token would take down whoever did bind.
    bail!(
        "hived for team '{team}' did not answer a matching ping within {SOCKET_READY_TIMEOUT}s; \
         see {}",
        devlog::hived_stderr_path(Path::new(workspace)).display()
    )
}

/// The identity ping and what it says, asked again while the desk answers
/// busy: a shut gate is a retirement it may cancel or a drain that ends,
/// not a generation to replace. The whole exchange fits the identity
/// budget; a desk still busy at its end is an error, not a restart.
fn identity_ping(
    workspace: &str,
    team: &str,
) -> Result<(Option<Map<String, Value>>, HivedIdentity)> {
    let deadline = monotonic() + IDENTITY_PING_TIMEOUT;
    loop {
        let response = hooked_request_ping(workspace, IDENTITY_PING_TIMEOUT);
        let identity = hived_identity(response.as_ref(), team);
        if identity != HivedIdentity::Busy {
            return Ok((response, identity));
        }
        if monotonic() >= deadline {
            bail!(
                "hived for team '{team}' is busy (retiring or draining) and did not admit a ping \
                 within {IDENTITY_PING_TIMEOUT}s; retry"
            );
        }
        thread::sleep(Duration::from_secs_f64(SOCKET_RETRY_INTERVAL));
    }
}

/// A generation this binary asked to stop is still serving: allowed to,
/// with the reason on stderr, when it is this team's under this home and
/// speaks this api; refused otherwise, since a payload the two builds
/// read differently must not go out.
fn keep_old_generation(response: Option<&Map<String, Value>>, team: &str, why: &str) -> Result<()> {
    let Some(response) = response else {
        bail!("hived is draining; retry after accepted operations finish");
    };
    if let HivedIdentity::ForeignHome(home) = hived_identity(Some(response), team) {
        bail!("hived now serves another hive home: {home}");
    }
    let served_team = response.get("team").and_then(Value::as_str).unwrap_or("");
    if served_team != team {
        bail!("hived on this workspace now serves team '{served_team}', not '{team}'");
    }
    let api = response.get("apiVersion").and_then(Value::as_i64);
    let old_build = response
        .get("buildHash")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let short = |hash: &str| hash[..hash.len().min(12)].to_string();
    if api != Some(HIVED_API_VERSION) {
        bail!(
            "hived for team '{team}' is build {} speaking api {} while this binary is build {} \
             speaking api {HIVED_API_VERSION}; {why}, and this binary will not send it requests \
             it may read differently — retry once it has retired",
            short(old_build),
            api.map_or("unknown".to_string(), |v| v.to_string()),
            short(hived_build_hash())
        );
    }
    eprintln!(
        "warning: hived for team '{team}' is build {} (this binary is build {}); {why}",
        short(old_build),
        short(hived_build_hash())
    );
    Ok(())
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

pub fn start_hived(
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

/// The display as the loop last resolved it, and the busy monitor on its
/// session. The location follows the windows' own tags tick by tick — a
/// window moved to another session, a session renamed, a display rebuilt
/// after a windowless stretch — and the monitor, the viewer sessions the
/// sleep gate counts and the idle notifier's session follow it; the hived
/// itself stays. What the CLI passed at start is where the display was
/// then, not an authority the loop keeps.
struct DisplayTrack {
    location: Option<DisplayLocation>,
    monitor: Option<Arc<dyn OutputMonitor>>,
}

impl DisplayTrack {
    fn window(&self) -> &str {
        self.location.as_ref().map_or("", |l| l.window.as_str())
    }

    fn window_id(&self) -> &str {
        self.location.as_ref().map_or("", |l| l.window_id.as_str())
    }

    fn session_id(&self) -> &str {
        self.location.as_ref().map_or("", |l| l.session_id.as_str())
    }

    /// Take this tick's resolution. A change of session stops the monitor
    /// on the old one and starts one on the new, which also gets the wake
    /// hooks; the same location again changes nothing.
    fn follow(&mut self, workspace: &str, team: &str, next: Option<DisplayLocation>) {
        if next == self.location {
            return;
        }
        let next_session = next.as_ref().map_or("", |l| l.session_id.as_str());
        hooked_notify_debug_emit(
            workspace,
            "hived.display",
            &[
                ("team", Value::from(team)),
                (
                    "window",
                    next.as_ref()
                        .map_or(Value::Null, |l| Value::from(l.window.as_str())),
                ),
                (
                    "windowId",
                    next.as_ref()
                        .map_or(Value::Null, |l| Value::from(l.window_id.as_str())),
                ),
                (
                    "session",
                    if next_session.is_empty() {
                        Value::Null
                    } else {
                        Value::from(next_session)
                    },
                ),
                (
                    "sessions",
                    Value::Array(
                        next.as_ref()
                            .map(|l| l.sessions.iter().map(|s| Value::from(s.as_str())).collect())
                            .unwrap_or_default(),
                    ),
                ),
            ],
        );
        if next_session != self.session_id() {
            if let Some(monitor) = self.monitor.take() {
                monitor.stop();
            }
            set_output_busy_monitor(None);
            if !next_session.is_empty() {
                let monitor = hooked_make_busy_monitor(next_session, workspace);
                if let Some(monitor) = monitor.as_ref() {
                    monitor.start();
                }
                set_output_busy_monitor(monitor.clone());
                self.monitor = monitor;
                hooked_install_wake_hooks(team, next_session);
            }
        }
        self.location = next;
    }
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
    let mut sleep = SleepState::default();
    let instance = TeamInstance::from_registry(team, workspace);
    let mut track = DisplayTrack {
        location: None,
        monitor: None,
    };
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
    let start_serving = |server| {
        hooked_start_request_server(
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
    // Ready: the listener is bound, the accept worker is up and the owner
    // file names this generation. Only now does the retired desk's marker
    // go — a start that failed before this point leaves it byte for byte,
    // so the session hooks can still wake the desk.
    sleep::clear_asleep_marker(workspace);
    hooked_release_reexec_lock_fd(inherited_reexec_lock_fd);

    // Every exit from the loop is a `break`, so the teardown after it runs
    // for all of them.
    loop {
        if !Path::new(workspace).is_dir() {
            retirement_reason = "workspace removed";
            break;
        }

        let now = monotonic();
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
                        ("tmux_window", Value::from(track.window())),
                        ("tmux_window_id", Value::from(track.window_id())),
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
            // The next generation is told where the display was last
            // seen, never where this one was born.
            let emit_reexec = || {
                hooked_notify_debug_emit(
                    workspace,
                    "hived.reexec",
                    &[
                        ("team", Value::from(team)),
                        ("tmux_window", Value::from(track.window())),
                        ("tmux_window_id", Value::from(track.window_id())),
                        ("oldHash", Value::from(hived_build_hash())),
                        ("newHash", Value::from(stale_hash.clone())),
                    ],
                );
            };
            if let Some(replacement) = reexec_hived(
                workspace,
                team,
                track.window(),
                track.window_id(),
                server.as_ref(),
                track.monitor.as_ref(),
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
        let (snap, status) = if display.due(now) {
            let snap = TickSnapshot::collect();
            let status = snap.status;
            if let Some(transition) = display.record(status, now) {
                hooked_notify_debug_emit(
                    workspace,
                    transition.event(),
                    &[
                        ("team", Value::from(team)),
                        ("status", Value::from(status)),
                        ("nextProbeSeconds", Value::from(display.next_in(now))),
                    ],
                );
            }
            (Some(snap).filter(TickSnapshot::reachable), Some(status))
        } else {
            (None, None)
        };
        // Where the display is now. A server that is gone has no windows;
        // a server that did not answer keeps the last location, so a
        // blink of tmux moves nothing.
        match (snap.as_ref(), status) {
            (Some(snap), _) => {
                let preferred = || {
                    crate::registry::load(team).and_then(|entry| {
                        entry
                            .get("display")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                };
                track.follow(workspace, team, snap.display_location(&instance, preferred));
            }
            (None, Some("no-server")) => track.follow(workspace, team, None),
            _ => {}
        }

        if now - last_window_check >= 30.0 {
            last_window_check = now;
            // The registry entry is the team's existence; the display is
            // only where it shows. A missing entry (`hive delete` removes
            // it) with no window of this instance left behind it retires
            // the desk; a display still up keeps it on the idle policy
            // below. Corrupt or foreign-instance entries are not
            // "missing": never retire on a read that might be wrong.
            if let Some(path) = crate::registry::entry_path(team) {
                if !path.is_file() && track.location.is_none() {
                    retirement_reason = "team removed";
                    break;
                }
            }
        }

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
                track.monitor.as_deref(),
                &mut status_state,
                now_epoch_seconds(),
                snap,
            );
            tick_members
        });

        if !hooked_wait_tick(IDLE_NOTIFY_TICK_SECONDS) {
            if finish_shutdown(workspace, Duration::from_secs(5)) {
                break;
            }
            continue;
        }

        if let (Some(snap), Some(tick_members)) = (snap.as_ref(), tick_members.as_deref()) {
            idle_notify_tick(
                team,
                track.session_id(),
                &mut idle_notify,
                track.monitor.as_deref(),
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
            snap.as_ref(),
            track.location.as_ref(),
            &owner_token,
            monotonic(),
        ) {
            break;
        }
    }

    // Retirement, in one order for every reason: under the startup lock
    // — a sleep took it at its final commit, every other exit takes it
    // here (ensure releases it before requesting shutdown, so a competing
    // starter cannot bind between the owner check and the unlink) — the
    // gate shuts, the accept worker is joined and the listener closed, the
    // journal gets its interruptions, the socket goes if it is still this
    // generation's, and only then the slow work: the monitor's join and
    // the pool clients this desk held. The lock outlasts all of it, so no
    // starter binds while this generation may still write shared state.
    let retirement = sleep.take_retirement();
    let lock_fd = match retirement.as_ref() {
        Some(retirement) => Some(retirement.lock_fd),
        // The lock lives in the workspace's run dir: a workspace that is
        // gone has nothing left to serialize against, and taking the lock
        // would recreate the directory the team's end removed.
        None if !Path::new(workspace).is_dir() => None,
        None => Some(loop {
            if let Some(fd) = hooked_try_acquire_reexec_lock(workspace) {
                break fd;
            }
            thread::sleep(Duration::from_millis(20));
        }),
    };
    close_admission();
    server.close();
    if !SHUTDOWN.load(Ordering::SeqCst) && Path::new(workspace).is_dir() {
        interrupt_operations(workspace, retirement_reason);
    }
    cleanup_socket_if_owner(workspace, &owner_token);
    if let Some(monitor) = track.monitor.as_ref() {
        monitor.stop();
    }
    set_output_busy_monitor(None);
    if let Some(retirement) = retirement {
        retirement.drop_clients();
    }
    hooked_release_reexec_lock_fd(lock_fd);
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
pub(super) fn finish_shutdown(workspace: &str, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while requests_in_flight() && std::time::Instant::now() < deadline {
        // The accept worker refuses arrivals meanwhile; only the leases
        // already handed out are waited for.
        thread::sleep(Duration::from_millis(20));
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
