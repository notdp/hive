use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use super::keys::{
    daemon_env_for_pane, key_from_socket_name, member_key, resolve_pane_key, socket_path_for_key,
};
use super::{grok_home, DAEMON_START_TIMEOUT};
use crate::adapters::base::washed_spawner_env;

// --------------------------------------------------------------------------
// daemon lifecycle
// --------------------------------------------------------------------------

/// Connect budget for a liveness probe: a unix connect to a listening
/// socket completes in microseconds; anything longer is not a live leader.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// True when a listener accepts a connection on the socket.
///
/// No ACP traffic: the leader's socket protocol is private, so the probe is
/// the connect alone. A socket file whose leader died refuses; a pidfile is
/// not consulted, because a pid can be dead or reused by an unrelated
/// process while the file still names it.
pub fn probe_socket(socket_path: &Path) -> bool {
    socket_path.exists() && connect_within(socket_path, PROBE_TIMEOUT).is_ok()
}

/// Non-blocking unix connect that gives up after *timeout*.
fn connect_within(socket_path: &Path, timeout: Duration) -> io::Result<()> {
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

/// The spawned leader as `spawn_daemon_key` sees it: pid, exit poll, terminate.
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
fn spawn_leader_real(
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
        // setsid: a session of its own, so the leader outlives the
        // short-lived CLI and its controlling terminal.
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(Box::new(RealDaemonChild(Mutex::new(cmd.spawn()?))))
}

fn spawn_leader(
    argv: &[String],
    env: &HashMap<String, String>,
) -> io::Result<Box<dyn DaemonChild>> {
    #[cfg(test)]
    {
        super::tests::daemon_spawn_override(argv, env)
    }
    #[cfg(not(test))]
    {
        spawn_leader_real(argv, env)
    }
}

/// Ensure the leader daemon the pane addresses is listening.
///
/// Idempotent: a live daemon on the resolved key's socket is reused (a tagged
/// member pane and its spawner reach the same member daemon). The daemon env
/// carries `TMUX_PANE`, so shell tools report the right pane.
pub fn spawn_daemon(pane: &str) -> bool {
    spawn_daemon_key(
        &resolve_pane_key(pane),
        daemon_env_for_pane(pane),
        "grok",
        DAEMON_START_TIMEOUT,
    )
}

/// Ensure the member's leader daemon is listening — keyed by identity,
/// no pane involved.
///
/// The engine-first lane: a team member's engine lives on
/// `m-<team>.<member>` and is raised before any pane exists; a pane that
/// later runs the TUI is one more client of this daemon. The identity
/// markers are washed as for the pane lane (see `daemon_env_for_pane`),
/// and no `TMUX_PANE` is pinned: the member's tool subprocesses identify
/// by the `GROK_SESSION_ID` the leader exports, matched against the roster,
/// and find their pane from that row (`tmux::get_current_pane_id`) — the
/// pane is display resolved on top of identity, never the other way round.
pub fn spawn_member_daemon(team: &str, member: &str) -> bool {
    let env = washed_spawner_env(&["CODEX_THREAD_ID", "GROK_SESSION_ID", "TMUX_PANE", "TMUX"]);
    spawn_daemon_key(&member_key(team, member), env, "grok", DAEMON_START_TIMEOUT)
}

/// The pid's command line as `ps` reports it; None once the pid is gone.
fn process_args(pid: libc::pid_t) -> Option<String> {
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

/// Every process and the command line `ps` reports for it, in one listing.
///
/// The reap needs every process bound to a socket, and nothing records
/// their pids: only the machine's own process table names them.
fn process_listing() -> Vec<(libc::pid_t, String)> {
    #[cfg(test)]
    {
        super::tests::process_listing_override()
    }
    #[cfg(not(test))]
    {
        process_listing_real()
    }
}

#[cfg_attr(test, allow(dead_code))]
fn process_listing_real() -> Vec<(libc::pid_t, String)> {
    // `-ww`: an argv cut off at the terminal width would hide the socket.
    let Ok(out) = Command::new("ps")
        .args(["-A", "-ww", "-o", "pid=,args="])
        .stdin(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (pid, args) = line.trim_start().split_once(char::is_whitespace)?;
            Some((pid.parse::<libc::pid_t>().ok()?, args.trim().to_string()))
        })
        .collect()
}

/// True when *args* passes `--leader-socket <sock>` — that exact path, never
/// another key's socket and never one this path is a prefix of.
fn names_leader_socket(args: &str, sock: &str) -> bool {
    let mut tokens = args.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "--leader-socket" {
            if tokens.next() == Some(sock) {
                return true;
            }
        } else if token.strip_prefix("--leader-socket=") == Some(sock) {
            return true;
        }
    }
    false
}

/// True when *args* is the leader of *sock*, never a client of it (the TUI
/// and hive's stdio client both carry the socket but not the `agent leader`
/// verb).
///
/// Two argv shapes bind a key. Hive's own spawn names the socket
/// (`grok agent leader --leader-socket <sock>`); the leader a grok client
/// raises for itself when it finds none names nothing, because grok passes
/// the path in `GROK_LEADER_SOCKET`, which `ps` does not print. A bare
/// `grok agent leader` is therefore trusted only for coming out of grok's
/// own `<sock>.lock`, which the holder of *this* socket's flock writes —
/// see [`leader_pid`] — while one naming a *different* socket is a
/// stranger a stale record points at and is never signalled.
fn is_leader_args(args: &str, sock: &str) -> bool {
    args.contains("agent leader")
        && (names_leader_socket(args, sock) || !args.contains("--leader-socket"))
}

/// True when the pid runs a leader that may hold *sock*: either shape of
/// [`is_leader_args`].
fn is_leader_of(pid: libc::pid_t, sock: &Path) -> bool {
    leader_args_of(pid).is_some_and(|args| is_leader_args(&args, &sock.to_string_lossy()))
}

/// True when the pid runs the leader hive itself spawned on *sock*: the
/// exact socket in argv, never the bare shape. Hive's `.pid` is never
/// cleared by a crash, so a recycled pid running some other key's
/// grok-raised leader (bare argv too) must not pass through it.
fn is_spawned_leader_of(pid: libc::pid_t, sock: &Path) -> bool {
    leader_args_of(pid).is_some_and(|args| {
        args.contains("agent leader") && names_leader_socket(&args, &sock.to_string_lossy())
    })
}

fn leader_args_of(pid: libc::pid_t) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    process_args(pid)
}

/// Pids of every client attached to *sock*, whoever started them: the pane's
/// grok TUI (`grok --leader --leader-socket <sock>`) and each stdio client
/// (`grok agent --leader stdio --leader-socket <sock>`) — hive's own, the
/// hived's pool client, one a member engine's `hive` call left behind.
fn socket_clients(sock: &Path) -> Vec<libc::pid_t> {
    let sock = sock.to_string_lossy();
    process_listing()
        .into_iter()
        .filter(|(pid, args)| {
            *pid > 0
                && !is_leader_args(args, sock.as_ref())
                && names_leader_socket(args, sock.as_ref())
        })
        .map(|(pid, _args)| pid)
        .collect()
}

/// Clear the socket once: every client first, then the leader itself.
///
/// *signalled* carries across the passes of one kill — a pid already
/// terminated has had SIGTERM and SIGKILL both, so seeing it again means
/// only that a stale record still names it.
fn reap_socket_once(sock: &Path, signalled: &mut Vec<libc::pid_t>) {
    let mut terminate = |pid: libc::pid_t| {
        if !signalled.contains(&pid) {
            signalled.push(pid);
            terminate_process_group(pid);
        }
    };
    for pid in socket_clients(sock) {
        terminate(pid);
    }
    // Read after the clients are down: killing one can raise a leader.
    if let Some(pid) = leader_pid(sock) {
        terminate(pid);
    }
}

/// The pid of the leader still running on this key, verified by identity.
///
/// Two files can name it: our own pidfile, written once a spawn binds, and
/// grok's `<sock>.lock`, which the leader writes with its pid. Only the
/// second one exists after a spawn that never bound, which is exactly the
/// case that needs reclaiming. Nothing clears either file when a leader
/// crashes, so a recorded pid is only trusted when the process behind it
/// is the leader of this socket — a dead or recycled pid is never signalled.
fn leader_pid(sock: &Path) -> Option<libc::pid_t> {
    let recorded = |ext: &str| {
        fs::read_to_string(sock.with_extension(ext))
            .ok()?
            .trim()
            .parse::<libc::pid_t>()
            .ok()
    };
    // Hive's own pidfile only ever names a leader hive spawned, so it must
    // pass on the exact socket; grok's lock names whichever leader holds
    // this socket's flock, including one a client raised with a bare argv.
    recorded("pid")
        .filter(|pid| is_spawned_leader_of(*pid, sock))
        .or_else(|| recorded("lock").filter(|pid| is_leader_of(*pid, sock)))
}

/// Start (or reuse) the leader daemon on *key*'s socket.
///
/// `setsid()` gives it a session of its own, detaching it from the
/// short-lived CLI; the hived
/// reaps member daemons the registry no longer lists, and pane-keyed ones
/// when their pane dies.
fn spawn_daemon_key(key: &str, env: HashMap<String, String>, grok_bin: &str, timeout: f64) -> bool {
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
    if let Some(pid) = leader_pid(&sock) {
        terminate_process_group(pid);
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
    let child = match spawn_leader(&argv, &env) {
        Ok(child) => child,
        Err(_) => return false,
    };
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    while Instant::now() < deadline {
        if child.poll().is_some() {
            return false; // died before binding
        }
        if sock.exists() {
            // Names only a leader hive spawned: `leader_pid` trusts it after
            // an identity check, and the hived reads its mtime as the newborn
            // grace. Liveness is the socket connect, never this pid.
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
                if let Some(key) = key_from_socket_name(name) {
                    keys.push(key);
                }
            }
        }
    }
    keys
}

/// SIGTERM the pid, escalating to SIGKILL if it lingers.
///
/// A leader hive spawned is `setsid()`'d in `spawn_leader_real`, so it
/// leads a group of
/// itself and whatever it forked, and `killpg` reaps them together. A holder
/// adopted from grok's lock file may sit in the pane shell's group instead,
/// where `killpg` would take the shell out with it, so a pid that is not its
/// own group leader is signalled alone.
fn terminate_process_group(pid: libc::pid_t) {
    #[cfg(test)]
    {
        if super::tests::terminate_pg_override(pid) {
            return;
        }
    }
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

/// Stop everything holding a key and remove its socket, lock, pidfile and
/// session record.
///
/// Clients before the leader: a grok client that finds its leader gone
/// raises a replacement on the same socket, so killing the leader while any
/// client lives only trades it for an orphan nobody owns — which is what
/// killing just the pane's TUI and this process's own pool client left
/// behind, since the hived's client and any a member engine started are
/// other processes entirely. The pass runs twice because a client killed
/// mid-respawn can leave a leader that appeared after the first one; grok's
/// `<sock>.lock` names it, so the key files go only once both passes are
/// done. Only a pid verified as this key's leader is signalled; a stale
/// record naming a dead or recycled pid is removed without touching the
/// process.
pub fn kill_daemon_key(key: &str) {
    let sock = socket_path_for_key(key);
    let mut signalled: Vec<libc::pid_t> = Vec::new();
    reap_socket_once(&sock, &mut signalled);
    reap_socket_once(&sock, &mut signalled);
    for path in [
        sock.clone(),
        sock.with_extension("lock"),
        sock.with_extension("pid"),
        sock.with_extension("session"),
    ] {
        let _ = fs::remove_file(path);
    }
}
