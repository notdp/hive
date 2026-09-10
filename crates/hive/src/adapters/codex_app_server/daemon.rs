// --------------------------------------------------------------------------
// daemon lifecycle
// --------------------------------------------------------------------------

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::auth_guard::{
    clear_auth_baseline, daemon_auth_stale_locked, disk_account_id, write_auth_baseline,
};
use super::client::{CodexDaemonClient, DaemonClient, ThreadRuntime, TurnResult, TurnStartFailure};
use super::records::{
    codex_home, shared_lock_path, shared_pidfile_path, shared_socket_path, thread_id_for_pane,
};
use super::transport::WsConn;
use super::{
    CONNECT_COOLDOWN, DAEMON_START_TIMEOUT, DAEMON_STOP_TIMEOUT, NO_RUNNING_TURN,
    TURN_INTERRUPT_ACCEPTED, TURN_START_ACCEPTED,
};
use crate::adapters::base::washed_spawner_env;

/// True when a live daemon answers initialize on this socket.
pub fn probe_socket(socket_path: &Path) -> bool {
    let mut conn = match WsConn::connect(socket_path, Duration::from_secs(2)) {
        Ok(conn) => conn,
        Err(_) => return false,
    };
    let probe = json!({"id": 1, "method": "initialize", "params": {
        "clientInfo": {"name": "hive-probe", "version": "0"},
    }});
    let answered = (|| -> io::Result<bool> {
        conn.send_text(&probe.to_string())?;
        let txt = conn.recv_text()?;
        let msg: Value = serde_json::from_str(&txt)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        Ok(msg.get("id").and_then(Value::as_i64) == Some(1))
    })()
    .unwrap_or(false);
    conn.close();
    answered
}

pub fn daemon_alive() -> bool {
    let sock = shared_socket_path();
    sock.exists() && probe_socket(&sock)
}

/// Daemon env: the shared daemon serves every pane, so per-pane identity
/// markers must not freeze into it — tool subprocesses inherit this env and a
/// stale TMUX_PANE would impersonate whichever pane spawned the daemon.
/// Identity rides codex's own per-thread CODEX_THREAD_ID injection instead.
///
/// CLAUDE*/ANTHROPIC* are washed for the same reason (as the grok leader
/// does): the spawner may itself run inside a claude engine, and an inherited
/// CLAUDE_CODE_MESSAGING_SOCKET makes every hive call from a codex tool shell
/// resolve to *that* engine's pane whenever the thread lookup misses.
pub(crate) fn daemon_env() -> HashMap<String, String> {
    washed_spawner_env(&["TMUX_PANE", "HIVE_CODEX_PANE"])
}

/// The exclusive lock on the shared daemon's lifecycle, released on drop.
///
/// Every caller that may replace the daemon — a `hive spawn`, a `hive
/// codex` launch, each team's hived tick — is a separate process, so the
/// probe → stale verdict → stop → start → record sequence runs under one
/// flock per CODEX_HOME. Without it a late caller stops the replacement,
/// or clears the replacement's socket and records after its own stop.
pub(super) struct DaemonLock(fs::File);

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub(super) fn lock_daemon() -> Option<DaemonLock> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(shared_lock_path())
        .ok()?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return None;
    }
    Some(DaemonLock(file))
}

/// What `ensure_daemon` did: kept the daemon that was answering, started
/// a new one (after stopping a stale one, or on no daemon at all), or
/// could not get one up. Only `Started` invalidates a client of the
/// daemon that was there before — a reused daemon keeps every tracked
/// turn its clients hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonOutcome {
    Reused,
    Started,
    Failed,
}

/// `ensure_daemon` as a plain readiness bool.
pub fn spawn_daemon() -> bool {
    ensure_daemon() != DaemonOutcome::Failed
}

/// Ensure the shared app-server daemon is listening.
///
/// Reuses a live daemon if one already answers on the shared socket
/// (idempotent spawn); a stale socket from a dead daemon is removed first.
/// Shares the real CODEX_HOME (auth/model/permission defaults stay correct).
/// The daemon is machine-level state: nothing in hive kills it when panes or
/// teams go away, and the hived re-spawns it if it dies while codex members
/// live. The one kill is a live daemon whose auth went stale
/// (`auth_guard.rs`): it is replaced here, so a member is never minted on a
/// daemon that cannot run a turn. The whole sequence — probe, the auth
/// verdict including a missing baseline settled by asking the daemon,
/// stop, start, record — holds the daemon lock. Returns false if the
/// daemon fails to bind or dies before ready.
pub fn ensure_daemon() -> DaemonOutcome {
    crate::plugin_manager::ensure_codex_plugin_current();
    let sock = shared_socket_path();
    if let Some(parent) = sock.parent() {
        if fs::create_dir_all(parent).is_err() {
            return DaemonOutcome::Failed;
        }
    }
    let Some(_lock) = lock_daemon() else {
        return DaemonOutcome::Failed;
    };
    if sock.exists() {
        if probe_socket(&sock) {
            if !daemon_auth_stale_locked() {
                return DaemonOutcome::Reused;
            }
            if !stop_daemon() {
                return DaemonOutcome::Failed;
            }
        }
        let _ = fs::remove_file(&sock); // stale socket from a dead daemon
    }
    // The account the child is about to load, read before it starts: a
    // login that lands during startup then shows as a change on the next
    // look instead of being recorded as the daemon's own.
    let born_with = disk_account_id();
    let stderr_path = codex_home()
        .join("app-server-control")
        .join("daemon.stderr");
    let stderr_file = match fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(stderr_path)
    {
        Ok(file) => file,
        Err(_) => return DaemonOutcome::Failed,
    };
    let mut cmd = Command::new("codex");
    cmd.arg("app-server")
        .arg("--listen")
        .arg(format!("unix://{}", sock.display()))
        .env_clear()
        .envs(daemon_env())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file));
    unsafe {
        // setsid: a session of its own, so the daemon outlives the
        // short-lived caller and its controlling terminal.
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(_) => return DaemonOutcome::Failed,
    };
    let deadline = Instant::now() + Duration::from_secs_f64(DAEMON_START_TIMEOUT);
    while Instant::now() < deadline {
        if let Ok(Some(_status)) = child.try_wait() {
            return DaemonOutcome::Failed; // died before binding
        }
        if probe_socket(&sock) {
            let _ = fs::write(shared_pidfile_path(), child.id().to_string());
            match born_with {
                Some(account) => {
                    write_auth_baseline(account.as_deref());
                }
                None => clear_auth_baseline(),
            }
            reap_in_background(child);
            return DaemonOutcome::Started;
        }
        thread::sleep(Duration::from_millis(200));
    }
    terminate_and_reap(child);
    DaemonOutcome::Failed
}

/// Wait on a started daemon from a thread. The daemon outlives this process
/// by design (setsid), but while both live this process is its parent, and
/// a dropped `Child` leaves a zombie behind when the daemon later exits or
/// is replaced — a hived collects one per daemon generation otherwise.
fn reap_in_background(mut child: Child) {
    let _ = thread::Builder::new()
        .name("codex-daemon-reaper".to_string())
        .spawn(move || {
            let _ = child.wait();
        });
}

/// SIGTERM a daemon that never bound, escalate to SIGKILL after 2s, and
/// wait so it does not linger as a zombie.
fn terminate_and_reap(mut child: Child) {
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The pid hive recorded for the daemon, only while a process by that pid
/// is still a `codex app-server` listening on this CODEX_HOME's socket: a
/// recycled pid, or another CODEX_HOME's daemon, is never signalled.
fn recorded_daemon_pid() -> Option<libc::pid_t> {
    let text = fs::read_to_string(shared_pidfile_path()).ok()?;
    let pid: libc::pid_t = text.trim().parse().ok()?;
    if pid <= 1 {
        return None;
    }
    let out = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let command = String::from_utf8_lossy(&out.stdout);
    let listen = format!("unix://{}", shared_socket_path().display());
    (command.contains("app-server") && command.contains(&listen)).then_some(pid)
}

/// Whether a live process answers to *target* (a pid, or `-pgid`), by the
/// process table rather than `kill(0)`: an exited daemon stays a zombie
/// of whichever hive process spawned it (a hived that respawned it, most
/// often) until that parent reaps it, and a zombie still answers
/// `kill(0)` and still counts in its group. Reaps first when the zombie
/// is this process's own.
fn target_alive(pid: libc::pid_t, target: libc::pid_t) -> bool {
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    let Ok(out) = Command::new("ps")
        .args(["-axo", "pid=,pgid=,stat="])
        .output()
    else {
        // No process table: fall back to the signal probe.
        return unsafe { libc::kill(target, 0) } == 0
            || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
    };
    String::from_utf8_lossy(&out.stdout).lines().any(|line| {
        let mut cols = line.split_whitespace();
        let (Some(row_pid), Some(row_pgid), Some(stat)) = (cols.next(), cols.next(), cols.next())
        else {
            return false;
        };
        if stat.starts_with('Z') {
            return false;
        }
        if target < 0 {
            row_pgid.parse::<libc::pid_t>().ok() == Some(-target)
        } else {
            row_pid.parse::<libc::pid_t>().ok() == Some(target)
        }
    })
}

/// Stop the live daemon; clear its socket, pidfile and auth baseline only
/// once it is gone. Caller holds the daemon lock.
///
/// SIGTERM goes to the daemon's process group: `spawn_daemon` made it a
/// session leader, and the npm launcher is a node wrapper whose native
/// child sits in the same group (the wrapper forwards SIGTERM itself; the
/// group is the belt to that brace). Exit is the process's, not the
/// socket's: codex stops listening before it finishes shutting down, so
/// a silent socket is not a gone daemon. Escalates to SIGKILL after the
/// stop budget; a daemon that survives that keeps its records, and false
/// says so. Attached TUIs (`codex --remote`) reconnect to the replacement
/// on their own. False also when hive never recorded the daemon's pid or
/// the pid is no longer this socket's codex app-server.
fn stop_daemon() -> bool {
    stop_daemon_within(DAEMON_STOP_TIMEOUT)
}

/// Uninstall only stops the process recorded for this CODEX_HOME's socket.
/// A stale pid pointing at another process is an error, never a signal target.
pub(crate) fn uninstall_daemon() -> Result<(), String> {
    let pidfile = shared_pidfile_path();
    if !pidfile.exists() {
        return if shared_socket_path().exists() {
            Err("shared socket exists without a recorded pid".into())
        } else {
            Ok(())
        };
    }
    let _lock = lock_daemon().ok_or("cannot lock the shared codex daemon")?;
    let text = fs::read_to_string(&pidfile).map_err(|e| e.to_string())?;
    let pid = text
        .trim()
        .parse::<libc::pid_t>()
        .map_err(|e| e.to_string())?;
    if pid <= 1 {
        return Err("invalid shared codex daemon pid".into());
    }
    if unsafe { libc::kill(pid, 0) } == -1
        && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    {
        for path in [
            shared_pidfile_path(),
            shared_socket_path(),
            super::shared_auth_baseline_path(),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        return Ok(());
    }
    if !stop_daemon() {
        return Err("could not stop this socket's recorded codex daemon; records kept".into());
    }
    Ok(())
}

pub(super) fn stop_daemon_within(term_budget: f64) -> bool {
    let Some(pid) = recorded_daemon_pid() else {
        return false;
    };
    let target = if unsafe { libc::getpgid(pid) } == pid {
        -pid
    } else {
        pid
    };
    if unsafe { libc::kill(target, libc::SIGTERM) } != 0 && target_alive(pid, target) {
        return false; // not ours to signal
    }
    let deadline = Instant::now() + Duration::from_secs_f64(term_budget);
    while Instant::now() < deadline && target_alive(pid, target) {
        thread::sleep(Duration::from_millis(200));
    }
    if target_alive(pid, target) {
        unsafe {
            libc::kill(target, libc::SIGKILL);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && target_alive(pid, target) {
            thread::sleep(Duration::from_millis(200));
        }
    }
    if target_alive(pid, target) {
        return false;
    }
    let _ = fs::remove_file(shared_socket_path());
    let _ = fs::remove_file(shared_pidfile_path());
    clear_auth_baseline();
    true
}

/// What the live daemon says its account is, asked over
/// `account/rateLimits/read`: the backend answers that call with the
/// account of the token the daemon actually holds. `Unauthorized` is the
/// daemon failing that call on its auth (the cross-account recovery
/// error, or no auth at all); `Unknown` is no daemon client, a transport
/// failure, or an answer without an account.
pub(super) enum DaemonAccount {
    Account(String),
    Unauthorized,
    Unknown,
}

pub(super) fn daemon_account() -> DaemonAccount {
    let Some(client) = shared_client() else {
        return DaemonAccount::Unknown;
    };
    let response = client.account_rate_limits();
    if let Some(account) = response
        .get("result")
        .and_then(|r| r.get("accountId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        return DaemonAccount::Account(account.to_string());
    }
    if response.get("__rejected__").is_some() {
        let text = response
            .get("__error__")
            .map(Value::to_string)
            .unwrap_or_default();
        if text.contains("signed in to another account")
            || text.contains("authentication required")
            || text.contains("401")
        {
            return DaemonAccount::Unauthorized;
        }
    }
    DaemonAccount::Unknown
}

// --------------------------------------------------------------------------
// shared client (one per process, lazily connected)
// --------------------------------------------------------------------------

struct SharedSlot {
    client: Option<CodexDaemonClient>,
    cooldown_until: Option<Instant>,
}

static CLIENT: Mutex<SharedSlot> = Mutex::new(SharedSlot {
    client: None,
    cooldown_until: None,
});

fn shared_client() -> Option<Arc<dyn DaemonClient>> {
    #[cfg(test)]
    {
        if let Some(overridden) = super::tests::shared_client_override() {
            return overridden;
        }
    }
    shared_client_prod().map(|client| {
        let dynamic: Arc<dyn DaemonClient> = Arc::new(client);
        dynamic
    })
}

fn shared_client_prod() -> Option<CodexDaemonClient> {
    {
        let mut slot = CLIENT.lock().unwrap();
        if let Some(client) = slot.client.as_ref() {
            if client.is_alive() {
                return Some(client.clone());
            }
        }
        if let Some(client) = slot.client.take() {
            client.close();
        }
        if let Some(until) = slot.cooldown_until {
            if Instant::now() < until {
                return None;
            }
        }
    }
    let sock = shared_socket_path();
    if !sock.exists() {
        set_cooldown();
        return None;
    }
    let client = match CodexDaemonClient::new(&sock) {
        Ok(client) => client,
        Err(_) => {
            set_cooldown();
            return None;
        }
    };
    if !client.initialize() {
        client.close();
        set_cooldown();
        return None;
    }
    client.attach(); // busy late-join recovery
    CLIENT.lock().unwrap().client = Some(client.clone());
    Some(client)
}

fn set_cooldown() {
    CLIENT.lock().unwrap().cooldown_until =
        Some(Instant::now() + Duration::from_secs_f64(CONNECT_COOLDOWN));
}

/// Eagerly bring hive's client online (spawn time / hived request).
pub fn connect() -> bool {
    shared_client().is_some()
}

/// Close the process's client so the next use reconnects (daemon respawn).
pub fn drop_client() {
    let client = {
        let mut slot = CLIENT.lock().unwrap();
        slot.cooldown_until = None;
        slot.client.take()
    };
    if let Some(client) = client {
        client.close();
    }
}

// --------------------------------------------------------------------------
// pane-keyed API (thread resolved through the pane's record)
// --------------------------------------------------------------------------

pub fn runtime_for_pane(pane: &str) -> Option<ThreadRuntime> {
    let tid = thread_id_for_pane(pane)?;
    runtime_for_thread(&tid)
}

pub fn runtime_for_thread(thread_id: &str) -> Option<ThreadRuntime> {
    let client = shared_client()?;
    client.runtime_or_backfill(thread_id)
}

/// Deliver text as a new turn on the pane's recorded thread.
///
/// Returns `TURN_START_ACCEPTED` when `turn/start` answered with a result —
/// the daemon accepted the turn, which is codex's transport boundary (not
/// proof the turn ran to completion). Returns `None` on transport failure:
/// no recorded thread (unmanaged codex), no daemon, an RPC error response,
/// or a connection failure. There is no keystroke fallback — normal hive
/// delivery never touches the composer. A *busy* thread is not bounced:
/// `turn/start` carries steer semantics in core, so hive hands it straight
/// to the RPC and lets codex pick the landing.
pub fn send_to_pane(pane: &str, text: &str) -> Option<&'static str> {
    let tid = thread_id_for_pane(pane)?;
    send_to_thread(&tid, text)
}

/// Deliver text as a new turn on *thread_id* — the engine-keyed core.
pub fn send_to_thread(thread_id: &str, text: &str) -> Option<&'static str> {
    let client = shared_client()?;
    let response = client.turn_start(thread_id, text).ok()?;
    if response.get("result").is_some() {
        Some(TURN_START_ACCEPTED)
    } else {
        None
    }
}

/// A workflow node's task as a tracked turn on the pane's recorded thread:
/// the turn id comes back so `turn_result` can read the turn's outcome.
/// `Refused` covers no recorded thread and no daemon as well.
pub fn dispatch_to_pane(pane: &str, text: &str) -> Result<(String, String), TurnStartFailure> {
    let tid = thread_id_for_pane(pane).ok_or_else(|| {
        TurnStartFailure::Refused(format!("pane {pane} has no recorded codex thread"))
    })?;
    let turn_id = dispatch_to_thread(&tid, text)?;
    Ok((tid, turn_id))
}

/// `dispatch_to_pane` keyed by the thread.
pub fn dispatch_to_thread(thread_id: &str, text: &str) -> Result<String, TurnStartFailure> {
    let client = shared_client()
        .ok_or_else(|| TurnStartFailure::Refused("codex daemon unreachable".to_string()))?;
    client.turn_start_tracked(thread_id, text)
}

/// The outcome of a turn `dispatch_to_thread` started, from the shared
/// client that started it; None when no client, or one that never saw
/// the turn (reconnected since).
pub fn turn_result(turn_id: &str) -> Option<TurnResult> {
    shared_client()?.turn_result(turn_id)
}

/// Abort the running turn on the pane's recorded thread.
///
/// Returns `TURN_INTERRUPT_ACCEPTED` when the daemon took the interrupt,
/// `NO_RUNNING_TURN` when the thread has no in-progress turn (nothing to
/// abort — not a failure), and `None` on transport failure. There is no
/// keystroke fallback: an Escape into the pane would land on whatever the
/// viewer is showing, while `turn/interrupt` is addressed to the thread.
pub fn interrupt_pane(pane: &str) -> Option<&'static str> {
    let tid = thread_id_for_pane(pane)?;
    interrupt_thread(&tid)
}

/// Whether a turn is open on *thread_id*, asked of the shared daemon:
/// `Some(false)` is the daemon answering that no turn is in progress,
/// `None` is no answer (no daemon, RPC error) — never a guess.
pub fn turn_open_for_thread(thread_id: &str) -> Option<bool> {
    let client = shared_client()?;
    client
        .active_turn_id(thread_id)
        .ok()
        .map(|turn| turn.is_some())
}

/// Abort the running turn on *thread_id* — the engine-keyed core.
pub fn interrupt_thread(thread_id: &str) -> Option<&'static str> {
    let client = shared_client()?;
    let turn_id = client.active_turn_id(thread_id).ok()?;
    let turn_id = match turn_id {
        Some(turn_id) if !turn_id.is_empty() => turn_id,
        _ => return Some(NO_RUNNING_TURN),
    };
    let response = client.turn_interrupt(thread_id, &turn_id).ok()?;
    if response.get("result").is_some() {
        Some(TURN_INTERRUPT_ACCEPTED)
    } else {
        None
    }
}

/// Start context compaction on the pane's recorded thread.
///
/// Compaction is *not* steerable: codex runs it as a Compact turn whose
/// first act is to abort any running turn. Firing it at a busy agent would
/// kill the in-flight work, so hive gates compaction on busy and only
/// compacts an idle thread.
///
/// Returns `"compacted"` (RPC accepted), `"busy"` (agent mid-turn), or
/// `"unavailable"` (no record / no daemon). On anything but `"compacted"`
/// the caller keystrokes `/compact` into the TUI so codex itself surfaces
/// its native "disabled while a task is in progress" refusal.
pub fn compact_pane(pane: &str) -> &'static str {
    let tid = match thread_id_for_pane(pane) {
        Some(tid) => tid,
        None => return "unavailable",
    };
    let client = match shared_client() {
        Some(client) => client,
        None => return "unavailable",
    };
    if let Some(rt) = client.runtime_or_backfill(&tid) {
        if rt.busy {
            return "busy";
        }
    }
    if client.compact_start(&tid).get("result").is_some() {
        "compacted"
    } else {
        "unavailable"
    }
}

/// Transcript session id of the pane's recorded thread.
///
/// threadId == sessionId on the app-server surface, so this is a plain
/// record read — no daemon round-trip and no lsof.
pub fn session_id_for_pane(pane: &str) -> Option<String> {
    thread_id_for_pane(pane)
}

// --------------------------------------------------------------------------
// spawn-flow helpers
// --------------------------------------------------------------------------

/// Renew ~/.codex/models_cache.json's fetched_at so a mint stays warm.
///
/// thread/start synchronously refetches /models when the cache is older than
/// codex's 300s TTL (~2.5s, up to its 5s timeout). The data barely changes
/// and codex itself renews the stamp without refetching on an etag match, so
/// extending the last real fetch is the same semantic; the daemon's periodic
/// Online refresh still overwrites with real data.
pub fn freshen_models_cache() -> bool {
    let path = codex_home().join("models_cache.json");
    let freshen = || -> Option<()> {
        let text = fs::read_to_string(&path).ok()?;
        let mut entry: Value = serde_json::from_str(&text).ok()?;
        let obj = entry.as_object_mut()?;
        obj.insert(
            "fetched_at".to_string(),
            Value::String(format!("{}.000000Z", crate::clock::utc_now_iso_seconds())),
        );
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string(&entry).ok()?).ok()?;
        fs::rename(&tmp, &path).ok()?;
        Some(())
    };
    freshen().is_some()
}

/// Mint a resumable thread for a new member; None on any failure.
pub fn start_member_thread(cwd: &str, name: &str, model: &str) -> Option<String> {
    let client = shared_client()?;
    freshen_models_cache();
    client.start_thread(cwd, name, model)
}

/// Server-side fork of *thread_id*; returns the fork's id, None on failure.
pub fn fork_member_thread(thread_id: &str, name: &str) -> Option<String> {
    let client = shared_client()?;
    freshen_models_cache();
    client.fork_thread(thread_id, name)
}

#[cfg(test)]
mod reap_tests {
    use super::*;

    /// `ps -o stat=` for *pid*: empty once the process is collected; `Z` while
    /// it is a zombie.
    fn stat(pid: u32) -> String {
        let out = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn test_reap_in_background_collects_a_child_that_exits_after_start() {
        let child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        reap_in_background(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let stat = stat(pid);
            if stat.is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "pid {pid} still present: stat {stat}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn test_terminate_and_reap_kills_and_collects_a_child_that_never_bound() {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        terminate_and_reap(child);
        assert_eq!(stat(pid), "", "pid {pid} must be gone, not a zombie");
    }
}
