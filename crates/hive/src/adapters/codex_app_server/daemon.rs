// --------------------------------------------------------------------------
// daemon lifecycle
// --------------------------------------------------------------------------

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::client::{CodexDaemonClient, DaemonClient, ThreadRuntime};
use super::records::{codex_home, shared_pidfile_path, shared_socket_path, thread_id_for_pane};
use super::transport::WsConn;
use super::{
    CONNECT_COOLDOWN, DAEMON_START_TIMEOUT, NO_RUNNING_TURN, TURN_INTERRUPT_ACCEPTED,
    TURN_START_ACCEPTED,
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

/// Ensure the shared app-server daemon is listening; return true if ready.
///
/// Reuses a live daemon if one already answers on the shared socket
/// (idempotent spawn); a stale socket from a dead daemon is removed first.
/// Shares the real CODEX_HOME (auth/model/permission defaults stay correct).
/// The daemon is machine-level state: nothing in hive kills it when panes or
/// teams go away, and the hived re-spawns it if it dies while codex members
/// live. Returns false if the daemon fails to bind or dies before ready.
pub fn spawn_daemon() -> bool {
    crate::plugin_manager::ensure_codex_plugin_current();
    let sock = shared_socket_path();
    if let Some(parent) = sock.parent() {
        if fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    if sock.exists() {
        if probe_socket(&sock) {
            return true; // reuse the live daemon
        }
        let _ = fs::remove_file(&sock); // stale socket from a dead daemon
    }
    let stderr_path = codex_home()
        .join("app-server-control")
        .join("daemon.stderr");
    let stderr_file = match fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(stderr_path)
    {
        Ok(file) => file,
        Err(_) => return false,
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
        Err(_) => return false,
    };
    let deadline = Instant::now() + Duration::from_secs_f64(DAEMON_START_TIMEOUT);
    while Instant::now() < deadline {
        if let Ok(Some(_status)) = child.try_wait() {
            return false; // died before binding
        }
        if probe_socket(&sock) {
            let _ = fs::write(shared_pidfile_path(), child.id().to_string());
            return true;
        }
        thread::sleep(Duration::from_millis(200));
    }
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    false
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
