// --------------------------------------------------------------------------
// server socket
// --------------------------------------------------------------------------

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Map, Value};

use super::*;

/// The serve loop's view of its listener; tests implement it with a
/// recording fake.
pub trait HivedServerApi: Send + Sync {
    fn close(&self);
    /// Wait without consuming a connection or acquiring an admission lease.
    fn wait_readable(&self, timeout: f64) -> bool;
    /// A zero timeout must accept without blocking: the admission lock is held.
    fn accept_timeout(&self, timeout: f64) -> Option<UnixStream>;
}

pub struct ServerSocket {
    listener: Mutex<Option<UnixListener>>,
}

impl HivedServerApi for ServerSocket {
    fn close(&self) {
        *self.listener.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn wait_readable(&self, timeout: f64) -> bool {
        let guard = self.listener.lock().unwrap_or_else(|e| e.into_inner());
        let Some(listener) = guard.as_ref() else {
            return false;
        };
        let mut pfd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = (timeout * 1000.0).ceil().max(0.0) as i32;
        unsafe { libc::poll(&mut pfd, 1, ms) > 0 && pfd.revents & libc::POLLIN != 0 }
    }

    fn accept_timeout(&self, timeout: f64) -> Option<UnixStream> {
        if timeout > 0.0 && !self.wait_readable(timeout) {
            return None;
        }
        let guard = self.listener.lock().unwrap_or_else(|e| e.into_inner());
        let listener = guard.as_ref()?;
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                Some(stream)
            }
            Err(_) => None,
        }
    }
}

pub(crate) fn open_server_socket(workspace: &str) -> Result<ServerSocket> {
    fs::create_dir_all(hooked_run_dir(workspace))?;
    cleanup_socket_impl(workspace);
    let sock = socket_path(workspace);
    let link = socket_link_path(workspace);
    if sock != link {
        // Relocated socket: its directory is ours alone (0700), and the
        // in-tree name points at it so a human looking in run/ still
        // finds the socket.
        if let Some(dir) = sock.parent() {
            fs::create_dir_all(dir)?;
            let _ = fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    let listener = UnixListener::bind(&sock)?;
    if sock != link {
        let _ = std::os::unix::fs::symlink(&sock, &link);
    }
    listener.set_nonblocking(true)?;
    Ok(ServerSocket {
        listener: Mutex::new(Some(listener)),
    })
}

// --------------------------------------------------------------------------
// request dispatch
// --------------------------------------------------------------------------

pub(super) fn err_response(error: impl std::fmt::Display) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("ok".to_string(), Value::Bool(false));
    map.insert("error".to_string(), Value::from(error.to_string()));
    map
}

pub(crate) fn handle_request(
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
    hived_started_at: &str,
    request: &Map<String, Value>,
) -> (Map<String, Value>, bool) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.handle_request.clone()).flatten() {
        return f(request);
    }
    let hived = hived_metadata(hived_started_at);
    let action = request.get("action").and_then(Value::as_str).unwrap_or("");
    let team_in_request = || {
        let requested = map_get_str(request, "team");
        if requested.is_empty() {
            team.to_string()
        } else {
            requested
        }
    };
    match action {
        "ping" => {
            let mut response = Map::new();
            response.insert("ok".to_string(), Value::Bool(true));
            response.insert("apiVersion".to_string(), Value::from(HIVED_API_VERSION));
            response.insert("buildHash".to_string(), Value::from(hived_build_hash()));
            response.insert("team".to_string(), Value::from(team));
            response.insert(
                "hiveHome".to_string(),
                Value::from(crate::paths::hive_home().to_string_lossy().into_owned()),
            );
            response.insert("tmuxWindow".to_string(), Value::from(tmux_window));
            response.insert("tmuxWindowId".to_string(), Value::from(tmux_window_id));
            response.insert("hived".to_string(), Value::Object(hived));
            (response, true)
        }
        "send" => {
            let sender = map_get_str(request, "senderAgent");
            let response = send_payload(
                workspace,
                &team_in_request(),
                SendOrigin::Member(&sender),
                &map_get_str(request, "targetAgent"),
                &map_get_str(request, "body"),
                &map_get_str(request, "artifact"),
            )
            .unwrap_or_else(err_response);
            (response, true)
        }
        "node-dispatch" => {
            let dispatch_id = map_get_str(request, "dispatchId");
            if dispatch_id.is_empty() {
                return (err_response("node-dispatch needs a dispatchId"), true);
            }
            let response = send_payload(
                workspace,
                &team_in_request(),
                SendOrigin::Node {
                    dispatch_id: &dispatch_id,
                },
                &map_get_str(request, "targetAgent"),
                &map_get_str(request, "body"),
                &map_get_str(request, "artifact"),
            )
            .unwrap_or_else(err_response);
            (response, true)
        }
        "doctor" => {
            let response = doctor_payload(
                workspace,
                &team_in_request(),
                &map_get_str(request, "agent"),
                request
                    .get("verbose")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                Some(&hived),
            )
            .unwrap_or_else(err_response);
            (response, true)
        }
        "team-runtime" => {
            let response = team_runtime_payload(&team_in_request()).unwrap_or_else(err_response);
            (response, true)
        }
        "runtime-snapshot" => {
            let response = runtime_snapshot_payload(&map_get_str(request, "pane"));
            (response, true)
        }
        "node-result" => {
            let dispatch_id = map_get_str(request, "dispatchId");
            if dispatch_id.is_empty() {
                return (err_response("node-result needs a dispatchId"), true);
            }
            (
                durable_node_result(workspace, &team_in_request(), &dispatch_id),
                true,
            )
        }
        "turn-open" => {
            let response = turn_open_payload(
                workspace,
                &team_in_request(),
                &map_get_str(request, "agent"),
            )
            .unwrap_or_else(err_response);
            (response, true)
        }
        "connect-codex" => {
            let mut response = Map::new();
            response.insert("ok".to_string(), Value::Bool(true));
            response.insert("connected".to_string(), Value::Bool(hooked_cas_connect()));
            (response, true)
        }
        "connect-grok" => {
            let pane = map_get_str(request, "pane");
            let connected = !pane.is_empty() && hooked_gl_connect_pane(&pane);
            let mut response = Map::new();
            response.insert("ok".to_string(), Value::Bool(true));
            response.insert("connected".to_string(), Value::Bool(connected));
            (response, true)
        }
        "shutdown" => {
            if let Some(expected) = request.get("expectedHived") {
                if expected != &Value::Object(hived.clone()) {
                    let mut response = Map::new();
                    response.insert("ok".into(), Value::Bool(false));
                    response.insert("generationChanged".into(), Value::Bool(true));
                    return (response, true);
                }
            }
            close_admission();
            let force = FORCE_SHUTDOWN.load(Ordering::SeqCst)
                || request
                    .get("force")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            let pending = pending_operations(workspace);
            if !force && pending > 0 {
                reopen_admission();
                return (
                    serde_json::json!({"ok":false,"draining":true,"pendingOperations":pending})
                        .as_object()
                        .unwrap()
                        .clone(),
                    true,
                );
            }
            FORCE_SHUTDOWN.store(force, Ordering::SeqCst);
            let mut response = Map::new();
            response.insert("ok".to_string(), Value::Bool(true));
            (response, false)
        }
        _ => (err_response("unknown action"), true),
    }
}

/// The preflight a new-format client sends before a request with a side
/// effect: one JSON line naming the action to come, answered on the same
/// connection before any business payload is written.
pub(crate) const ADMIT_ACTION: &str = "admit";

/// The reply to a connection the shut gate refused, whichever line it
/// carried: a preflight sees it as "not admitted", an old-format ping as a
/// busy desk. Nothing of the request was served.
const NOT_ADMITTED_REPLY: &[u8] =
    b"{\"ok\":false,\"notAdmitted\":true,\"error\":\"hived is draining; request not admitted; retry later\"}\n";

/// Newline-framed lines off a connection, each read under one monotonic
/// budget. Every underlying read gets what is left of the frame's budget,
/// so a peer that drips a byte at a time below the socket timeout still
/// ends at the deadline; a whole frame that arrives within it is taken
/// however large. Bytes past a line stay for the next read, so a body
/// sent on the heels of its preflight is not lost.
pub(super) struct FrameReader<'a> {
    conn: &'a UnixStream,
    pending: Vec<u8>,
}

impl<'a> FrameReader<'a> {
    pub(super) fn new(conn: &'a UnixStream) -> Self {
        FrameReader {
            conn,
            pending: Vec::new(),
        }
    }

    /// One read once the socket is readable, waited for with `poll` under
    /// what is left of the deadline — not a socket timeout, which Darwin
    /// refuses to set (EINVAL) on a socket shut in both directions, the
    /// state a peer that answered and closed leaves behind.
    fn read_more(&mut self, deadline: std::time::Instant) -> std::io::Result<usize> {
        use std::os::fd::AsRawFd;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "read budget exhausted",
                ));
            }
            let wait = remaining
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as i32;
            let mut fds = libc::pollfd {
                fd: self.conn.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            match unsafe { libc::poll(&mut fds, 1, wait) } {
                0 => continue,
                -1 => {
                    let err = std::io::Error::last_os_error();
                    if err.kind() != std::io::ErrorKind::Interrupted {
                        return Err(err);
                    }
                }
                _ => break,
            }
        }
        let mut chunk = [0u8; 65536];
        let n = self.conn.read(&mut chunk)?;
        self.pending.extend_from_slice(&chunk[..n]);
        Ok(n)
    }

    /// One newline-terminated line within `budget`, or `None` when the
    /// peer closed with nothing pending. A line without its newline still
    /// counts at EOF; a budget that runs out is an error, whatever was
    /// pending.
    pub(super) fn read_line(&mut self, budget: Duration) -> std::io::Result<Option<Vec<u8>>> {
        let deadline = std::time::Instant::now() + budget;
        loop {
            if let Some(at) = self.pending.iter().position(|b| *b == b'\n') {
                let rest = self.pending.split_off(at + 1);
                return Ok(Some(std::mem::replace(&mut self.pending, rest)));
            }
            if self.read_more(deadline)? == 0 {
                return Ok((!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending)));
            }
        }
    }

    /// Everything up to EOF within `budget`, pending bytes first.
    pub(super) fn read_to_end(&mut self, budget: Duration) -> std::io::Result<Vec<u8>> {
        let deadline = std::time::Instant::now() + budget;
        while self.read_more(deadline)? > 0 {}
        Ok(std::mem::take(&mut self.pending))
    }
}

/// One JSON line within the read budget, or none when the peer closed
/// with nothing, the budget ran out or the read failed. A line that is no
/// object is an empty map: answered, never served.
fn read_line_object(reader: &mut FrameReader<'_>, budget: Duration) -> Option<Map<String, Value>> {
    match reader.read_line(budget) {
        Ok(Some(line)) => match serde_json::from_slice::<Value>(&line) {
            Ok(Value::Object(map)) => Some(map),
            _ => Some(Map::new()),
        },
        Ok(None) | Err(_) => None,
    }
}

fn write_line(conn: &UnixStream, response: Map<String, Value>) {
    let mut body = serde_json::to_string(&Value::Object(response)).unwrap_or_default();
    body.push('\n');
    let _ = (&*conn).write_all(body.as_bytes());
}

#[allow(clippy::too_many_arguments)]
fn serve_connection(
    conn: UnixStream,
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
    hived_started_at: &str,
    read_timeout: f64,
    mut lease: RequestLease,
) {
    // The read budget bounds each frame — the first line, and the body
    // after an admission — from its first byte; a peer that never
    // completes one is dropped unserved, whatever pace it drips at.
    let budget = Duration::from_secs_f64(read_timeout.max(0.001));
    let _ = conn.set_write_timeout(Some(budget));
    let mut reader = FrameReader::new(&conn);
    let Some(first) = read_line_object(&mut reader, budget) else {
        return;
    };
    let first_action = first
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let request = if first_action == ADMIT_ACTION {
        // The lease reserved at accept spans the preflight, the body and
        // the reply: closing the gate now cannot retire the desk under
        // this request. A client that goes quiet after the handshake
        // releases it with nothing served.
        let for_action = map_get_str(&first, "forAction");
        if for_action.is_empty() || for_action == ADMIT_ACTION {
            write_line(&conn, err_response("admit needs the action it is for"));
            let _ = conn.shutdown(std::net::Shutdown::Write);
            return;
        }
        let mut admitted = Map::new();
        admitted.insert("ok".to_string(), Value::Bool(true));
        admitted.insert("admitted".to_string(), Value::Bool(true));
        admitted.insert("apiVersion".to_string(), Value::from(HIVED_API_VERSION));
        write_line(&conn, admitted);
        #[cfg(test)]
        if let Some(f) = hookget(|h| h.after_admit.clone()).flatten() {
            f();
        }
        let Some(body) = read_line_object(&mut reader, budget) else {
            return;
        };
        let action = body.get("action").and_then(Value::as_str).unwrap_or("");
        if action != for_action {
            write_line(
                &conn,
                err_response(format!(
                    "admitted for '{for_action}', request carries '{action}'; not served"
                )),
            );
            let _ = conn.shutdown(std::net::Shutdown::Write);
            return;
        }
        body
    } else if admission_required(&first_action) {
        // An old-format side-effect request: the sender is a build that
        // speaks an older api. Refused whole, so a payload the two builds
        // may read differently never reaches a transport.
        write_line(
            &conn,
            err_response(format!(
                "'{first_action}' requires an admission preflight (hived api {HIVED_API_VERSION}); \
                 the calling hive binary speaks an older api"
            )),
        );
        let _ = conn.shutdown(std::net::Shutdown::Write);
        return;
    } else {
        first
    };
    lease.classify(request.get("action").and_then(Value::as_str).unwrap_or(""));
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.before_handler.clone()).flatten() {
        f();
    }
    let (response, keep_running) = handle_request(
        workspace,
        team,
        tmux_window,
        tmux_window_id,
        hived_started_at,
        &request,
    );
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.before_reply.clone()).flatten() {
        f();
    }
    write_line(&conn, response);
    let _ = conn.shutdown(std::net::Shutdown::Write);
    // Answer first, then retire: the reply must be on the wire before the
    // loop tears the socket down.
    if !keep_running {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }
}

/// Refuse a connection the shut gate accepted. Bounded: whatever the peer
/// sent is drained for at most the read budget, then the reply goes out.
/// No lease, no handler, no engine operation.
fn reject_connection(mut conn: UnixStream) {
    let timeout = Some(Duration::from_millis(100));
    let _ = conn.set_read_timeout(timeout);
    let _ = conn.set_write_timeout(timeout);
    let mut buf = [0u8; 4096];
    let deadline = std::time::Instant::now() + Duration::from_millis(100);
    while std::time::Instant::now() < deadline {
        match conn.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) if buf[..n].contains(&b'\n') => break,
            Ok(_) => {}
        }
    }
    let _ = conn.write_all(NOT_ADMITTED_REPLY);
    let _ = conn.shutdown(std::net::Shutdown::Write);
}

/// Owns the accept worker with the socket generation. `close` joins it before
/// the listener can be unlinked or re-executed; handlers retain their leases.
/// The worker is the listener's only acceptor: with the gate open it hands
/// each connection a lease and a handler thread, with it shut it refuses
/// each one within a bounded budget, so the coordinator never accepts.
pub(super) struct RequestServer {
    server: Arc<dyn HivedServerApi>,
    stopped: Arc<AtomicBool>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

impl RequestServer {
    pub(super) fn start(
        server: Box<dyn HivedServerApi>,
        workspace: &str,
        team: &str,
        tmux_window: &str,
        tmux_window_id: &str,
        started_at: &str,
    ) -> Result<Box<dyn HivedServerApi>> {
        let server: Arc<dyn HivedServerApi> = Arc::from(server);
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_server = Arc::clone(&server);
        let worker_stopped = Arc::clone(&stopped);
        let context = RequestContext {
            workspace: workspace.into(),
            team: team.into(),
            tmux_window: tmux_window.into(),
            tmux_window_id: tmux_window_id.into(),
            started_at: started_at.into(),
        };
        let worker = thread::Builder::new()
            .name("hived-accept".into())
            .spawn(move || {
                while !worker_stopped.load(Ordering::SeqCst) {
                    if SHUTDOWN.load(Ordering::SeqCst) {
                        // Retiring: every arrival is refused until the
                        // listener closes, none is left to be reset.
                        reject_until(worker_server.as_ref(), 0.1, &worker_stopped);
                    } else {
                        serve_until(worker_server.as_ref(), &context, 1.0, &worker_stopped);
                    }
                }
            })?;
        Ok(Box::new(Self {
            server,
            stopped,
            worker: Mutex::new(Some(worker)),
        }))
    }
}

impl HivedServerApi for RequestServer {
    fn close(&self) {
        let mut worker = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(worker) = worker.take() {
            self.stopped.store(true, Ordering::SeqCst);
            worker.thread().unpark();
            let _ = worker.join();
            self.server.close();
        }
    }

    fn wait_readable(&self, timeout: f64) -> bool {
        self.server.wait_readable(timeout)
    }

    fn accept_timeout(&self, timeout: f64) -> Option<UnixStream> {
        self.server.accept_timeout(timeout)
    }
}

impl Drop for RequestServer {
    fn drop(&mut self) {
        self.close();
    }
}

struct RequestContext {
    workspace: String,
    team: String,
    tmux_window: String,
    tmux_window_id: String,
    started_at: String,
}

#[cfg(test)]
pub(crate) fn serve_requests(
    server: &dyn HivedServerApi,
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
    hived_started_at: &str,
    timeout: f64,
) -> bool {
    serve_until(
        server,
        &RequestContext {
            workspace: workspace.into(),
            team: team.into(),
            tmux_window: tmux_window.into(),
            tmux_window_id: tmux_window_id.into(),
            started_at: hived_started_at.into(),
        },
        timeout,
        &AtomicBool::new(false),
    )
}

/// Accept one connection if the listener has one, under the admission
/// lock: with the gate open the connection gets a lease and a handler,
/// with it shut it is counted as an arrival and refused off-thread.
/// Returns false when nothing was accepted.
fn accept_one(server: &dyn HivedServerApi, context: &RequestContext, timeout: f64) -> bool {
    let (conn, lease) = {
        let mut state = admission().lock().unwrap_or_else(|e| e.into_inner());
        let Some(conn) = server.accept_timeout(0.0) else {
            return false;
        };
        if state.closed {
            state.arrivals = state.arrivals.wrapping_add(1);
            drop(state);
            let _ = thread::Builder::new()
                .name("hived-reject".to_string())
                .spawn(move || reject_connection(conn));
            return true;
        }
        let lease = RequestLease::reserve(&mut state);
        (conn, lease)
    };
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.after_accept.clone()).flatten() {
        f();
    }
    let workspace = context.workspace.clone();
    let team = context.team.clone();
    let tmux_window = context.tmux_window.clone();
    let tmux_window_id = context.tmux_window_id.clone();
    let hived_started_at = context.started_at.clone();
    let _ = thread::Builder::new()
        .name("hived-request".to_string())
        .spawn(move || {
            serve_connection(
                conn,
                &workspace,
                &team,
                &tmux_window,
                &tmux_window_id,
                &hived_started_at,
                timeout,
                lease,
            );
        });
    true
}

fn serve_until(
    server: &dyn HivedServerApi,
    context: &RequestContext,
    timeout: f64,
    stopped: &AtomicBool,
) -> bool {
    let end = std::time::Instant::now() + Duration::from_secs_f64(timeout);
    while !SHUTDOWN.load(Ordering::SeqCst) && !stopped.load(Ordering::SeqCst) {
        let remaining = end.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        // Waiting on an idle listener is not an in-flight request. Bound the
        // poll so close can join the worker even with no incoming connection.
        if !server.wait_readable(remaining.as_secs_f64().min(0.1)) {
            continue;
        }
        accept_one(server, context, timeout);
    }
    !SHUTDOWN.load(Ordering::SeqCst)
}

/// The worker's retiring mode: accept and refuse for up to `timeout`
/// seconds. Nothing is admitted whatever the gate says — the shutdown that
/// set the flag closed it — so no lease and no usage come out of it.
fn reject_until(server: &dyn HivedServerApi, timeout: f64, stopped: &AtomicBool) {
    let end = std::time::Instant::now() + Duration::from_secs_f64(timeout);
    while !stopped.load(Ordering::SeqCst) && SHUTDOWN.load(Ordering::SeqCst) {
        let remaining = end.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        if !server.wait_readable(remaining.as_secs_f64().min(0.1)) {
            continue;
        }
        let conn = {
            let mut state = admission().lock().unwrap_or_else(|e| e.into_inner());
            let Some(conn) = server.accept_timeout(0.0) else {
                continue;
            };
            state.arrivals = state.arrivals.wrapping_add(1);
            conn
        };
        let _ = thread::Builder::new()
            .name("hived-reject".to_string())
            .spawn(move || reject_connection(conn));
    }
}
