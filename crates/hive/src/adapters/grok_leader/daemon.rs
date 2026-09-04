use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use super::keys::{
    _daemon_env_for_pane, _key_from_socket_name, member_key, resolve_pane_key, socket_path_for_key,
};
use super::{grok_home, _DAEMON_START_TIMEOUT};

// --------------------------------------------------------------------------
// daemon lifecycle
// --------------------------------------------------------------------------

/// Connect budget for a liveness probe: a unix connect to a listening
/// socket completes in microseconds; anything longer is not a live leader.
const _PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// True when a listener accepts a connection on the socket.
///
/// No ACP traffic: the leader's socket protocol is private, so the probe is
/// the connect alone. A socket file whose leader died refuses; a pidfile is
/// not consulted, because a pid can be dead or reused by an unrelated
/// process while the file still names it.
pub fn probe_socket(socket_path: &Path) -> bool {
    socket_path.exists() && _connect_within(socket_path, _PROBE_TIMEOUT).is_ok()
}

/// Non-blocking unix connect that gives up after *timeout*.
fn _connect_within(socket_path: &Path, timeout: Duration) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::{FromRawFd, OwnedFd};

    let bytes = socket_path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path too long",
        ));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let _fd = unsafe { OwnedFd::from_raw_fd(raw) }; // closed on every return
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    let rc = unsafe { libc::connect(raw, &addr as *const _ as *const libc::sockaddr, len) };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() != Some(libc::EINPROGRESS) {
        return Err(err);
    }
    let mut pfd = libc::pollfd {
        fd: raw,
        events: libc::POLLOUT,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as libc::c_int) };
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    if ready == 0 {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "connect timed out"));
    }
    let mut so_err: libc::c_int = 0;
    let mut so_len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            raw,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            &mut so_err as *mut _ as *mut libc::c_void,
            &mut so_len,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if so_err != 0 {
        return Err(io::Error::from_raw_os_error(so_err));
    }
    Ok(())
}

/// The spawned leader as `_spawn_daemon_key` sees it (Python `Popen` handle).
pub(super) trait DaemonChild: Send {
    fn pid(&self) -> u32;
    fn poll(&self) -> Option<i32>;
    fn terminate(&self);
}

#[cfg_attr(test, allow(dead_code))]
struct RealDaemonChild(Mutex<Child>);

impl DaemonChild for RealDaemonChild {
    fn pid(&self) -> u32 {
        self.0.lock().unwrap().id()
    }

    fn poll(&self) -> Option<i32> {
        let mut child = self.0.lock().unwrap();
        match child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or_else(|| {
                use std::os::unix::process::ExitStatusExt;
                -status.signal().unwrap_or(1)
            })),
            Ok(None) => None,
            Err(_) => Some(-1),
        }
    }

    fn terminate(&self) {
        let mut child = self.0.lock().unwrap();
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
    }
}

#[cfg_attr(test, allow(dead_code))]
fn _spawn_leader_real(
    argv: &[String],
    env: &HashMap<String, String>,
) -> io::Result<Box<dyn DaemonChild>> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        use std::os::unix::process::CommandExt;
        // Python start_new_session=True: detach from the short-lived CLI.
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(Box::new(RealDaemonChild(Mutex::new(cmd.spawn()?))))
}

fn _spawn_leader(
    argv: &[String],
    env: &HashMap<String, String>,
) -> io::Result<Box<dyn DaemonChild>> {
    #[cfg(test)]
    {
        super::tests::daemon_spawn_override(argv, env)
    }
    #[cfg(not(test))]
    {
        _spawn_leader_real(argv, env)
    }
}

/// Ensure the leader daemon the pane addresses is listening.
///
/// Idempotent: a live daemon on the resolved key's socket is reused (a tagged
/// member pane and its spawner reach the same member daemon). The daemon env
/// carries `TMUX_PANE` (shell tools report the right pane) and, for a
/// member key, `HIVE_TEAM`/`HIVE_MEMBER`.
pub fn spawn_daemon(pane: &str) -> bool {
    _spawn_daemon_key(
        &resolve_pane_key(pane),
        _daemon_env_for_pane(pane),
        "grok",
        _DAEMON_START_TIMEOUT,
    )
}

/// Ensure the member's leader daemon is listening — no pane involved.
///
/// The headless spawn lane: env carries the member identity only (no
/// `TMUX_PANE` — there is no pane to report).
pub fn spawn_member_daemon(team: &str, member: &str) -> bool {
    let mut env: HashMap<String, String> = env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| {
            !(key.starts_with("CLAUDE")
                || key.starts_with("ANTHROPIC")
                || matches!(
                    key.as_str(),
                    "CODEX_THREAD_ID" | "HIVE_TEAM" | "HIVE_MEMBER" | "TMUX_PANE" | "TMUX"
                ))
        })
        .collect();
    env.insert("HIVE_TEAM".to_string(), team.to_string());
    env.insert("HIVE_MEMBER".to_string(), member.to_string());
    _spawn_daemon_key(
        &member_key(team, member),
        env,
        "grok",
        _DAEMON_START_TIMEOUT,
    )
}

/// The pid's command line as `ps` reports it; None once the pid is gone.
fn _process_args(pid: libc::pid_t) -> Option<String> {
    #[cfg(test)]
    {
        if let Some(args) = super::tests::process_args_override(pid) {
            return args;
        }
    }
    let out = Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let args = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!args.is_empty()).then_some(args)
}

/// True when the pid runs the leader of *sock*: `grok agent leader
/// --leader-socket <sock>`. Neither the TUI nor hive's stdio client, which
/// both carry the socket path but not the `agent leader` verb.
fn _is_leader_of(pid: libc::pid_t, sock: &Path) -> bool {
    if pid <= 0 {
        return false;
    }
    let sock = sock.to_string_lossy();
    _process_args(pid)
        .map(|args| args.contains("agent leader") && args.contains(sock.as_ref()))
        .unwrap_or(false)
}

/// The pid of the leader still running on this key, verified by identity.
///
/// Two files can name it: our own pidfile, written once a spawn binds, and
/// grok's `<sock>.lock`, which the leader writes with its pid. Only the
/// second one exists after a spawn that never bound, which is exactly the
/// case that needs reclaiming. Nothing clears either file when a leader
/// crashes, so a recorded pid is only trusted when the process behind it
/// is the leader of this socket — a dead or recycled pid is never signalled.
fn _leader_pid(sock: &Path) -> Option<libc::pid_t> {
    [sock.with_extension("pid"), sock.with_extension("lock")]
        .iter()
        .filter_map(|path| fs::read_to_string(path).ok())
        .filter_map(|text| text.trim().parse::<libc::pid_t>().ok())
        .find(|pid| _is_leader_of(*pid, sock))
}

/// Start (or reuse) the leader daemon on *key*'s socket.
///
/// `start_new_session` detaches it from the short-lived CLI; the hived
/// reaps member daemons the registry no longer lists, and pane-keyed ones
/// when their pane dies.
fn _spawn_daemon_key(
    key: &str,
    env: HashMap<String, String>,
    grok_bin: &str,
    timeout: f64,
) -> bool {
    let sock = socket_path_for_key(key);
    if let Some(parent) = sock.parent() {
        if fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    if probe_socket(&sock) {
        return true;
    }
    // Nothing answers, but a leader may still be alive holding grok's flock
    // for this key (`<sock>.lock`, written with its pid). While it holds it,
    // a replacement cannot bind, and deleting the socket only makes the pair
    // permanent: no socket to probe, no pidfile of ours to reclaim by, and
    // every later spawn times out into plain grok. Reclaim the key first;
    // a recorded pid that is not this key's leader is stale and only its
    // files go.
    if let Some(pid) = _leader_pid(&sock) {
        _terminate_process_group(pid);
    }
    for path in [
        sock.clone(),
        sock.with_extension("lock"),
        sock.with_extension("pid"),
    ] {
        let _ = fs::remove_file(path);
    }
    let argv: Vec<String> = vec![
        grok_bin.to_string(),
        "agent".to_string(),
        "leader".to_string(),
        "--leader-socket".to_string(),
        sock.to_string_lossy().into_owned(),
        "--no-auto-update".to_string(),
        "--no-exit-on-disconnect".to_string(),
    ];
    let child = match _spawn_leader(&argv, &env) {
        Ok(child) => child,
        Err(_) => return false,
    };
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    while Instant::now() < deadline {
        if child.poll().is_some() {
            return false; // died before binding
        }
        if sock.exists() {
            let _ = fs::write(sock.with_extension("pid"), child.pid().to_string());
            return true;
        }
        thread::sleep(Duration::from_millis(200));
    }
    child.terminate();
    false
}

/// Daemon keys that currently have a leader socket on disk.
pub fn list_daemon_keys() -> Vec<String> {
    let root = grok_home().join("hive");
    if !root.is_dir() {
        return Vec::new();
    }
    let mut keys = Vec::new();
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if let Some(key) = _key_from_socket_name(name) {
                    keys.push(key);
                }
            }
        }
    }
    keys
}

/// SIGTERM the pid's process group, escalating to SIGKILL if it lingers.
///
/// spawn_daemon uses `start_new_session`, so the leader is a process-group
/// leader and its children share the group; `killpg` reaps them together.
fn _terminate_process_group(pid: libc::pid_t) {
    #[cfg(test)]
    {
        if super::tests::terminate_pg_override(pid) {
            return;
        }
    }
    // Our own leaders are start_new_session'd, so the group is the daemon and
    // whatever it forked. A holder we adopted from grok's lock file may not be:
    // signalling its group could then take out the pane's shell with it, so a
    // pid that is not its own group leader is signalled alone.
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid < 0 {
        return;
    }
    let group = pgid == pid;
    for sig in [libc::SIGTERM, libc::SIGKILL] {
        let sent = if group {
            unsafe { libc::killpg(pgid, sig) }
        } else {
            unsafe { libc::kill(pid, sig) }
        };
        if sent != 0 {
            return;
        }
        for _ in 0..10 {
            // up to ~1s before escalating
            if unsafe { libc::kill(pid, 0) } != 0 {
                return; // exited
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Stop a key's leader and remove its socket, pidfile and session record.
///
/// Only a pid verified as this key's leader is signalled; a stale record
/// naming a dead or recycled pid is removed without touching the process.
pub fn kill_daemon_key(key: &str) {
    let sock = socket_path_for_key(key);
    let pidfile = sock.with_extension("pid");
    if let Some(pid) = _leader_pid(&sock) {
        _terminate_process_group(pid);
    }
    for path in [
        sock.clone(),
        sock.with_extension("lock"),
        pidfile,
        sock.with_extension("session"),
    ] {
        let _ = fs::remove_file(path);
    }
}

/// Stop the leader the pane addresses (member daemon for a tagged pane).
pub fn kill_pane_daemon(pane: &str) {
    kill_daemon_key(&resolve_pane_key(pane));
}
