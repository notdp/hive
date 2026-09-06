// --------------------------------------------------------------------------
// server socket
// --------------------------------------------------------------------------

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Map, Value};

use super::*;

/// The serve loop's view of its listener; tests implement it with a
/// recording fake.
pub trait HivedServerApi: Send + Sync {
    fn close(&self);
    fn accept_timeout(&self, timeout: f64) -> Option<UnixStream>;
}

pub struct ServerSocket {
    listener: Mutex<Option<UnixListener>>,
}

impl HivedServerApi for ServerSocket {
    fn close(&self) {
        *self.listener.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn accept_timeout(&self, timeout: f64) -> Option<UnixStream> {
        let guard = self.listener.lock().unwrap_or_else(|e| e.into_inner());
        let listener = guard.as_ref()?;
        let mut pfd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = (timeout * 1000.0).ceil().max(0.0) as i32;
        let ret = unsafe { libc::poll(&mut pfd, 1, ms) };
        if ret <= 0 {
            return None;
        }
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
            (node_result_payload(&dispatch_id), true)
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
            let mut response = Map::new();
            response.insert("ok".to_string(), Value::Bool(true));
            (response, false)
        }
        _ => (err_response("unknown action"), true),
    }
}

fn serve_connection(
    conn: UnixStream,
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
    hived_started_at: &str,
    read_timeout: f64,
) {
    INFLIGHT_REQUESTS.fetch_add(1, Ordering::SeqCst);
    let _ = conn.set_read_timeout(Some(Duration::from_secs_f64(read_timeout.max(0.001))));
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        match (&conn).read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(_) => {
                raw.clear();
                break;
            }
        }
    }
    let request = match serde_json::from_slice::<Value>(&raw) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let (response, keep_running) = handle_request(
        workspace,
        team,
        tmux_window,
        tmux_window_id,
        hived_started_at,
        &request,
    );
    let mut body = serde_json::to_string(&Value::Object(response)).unwrap_or_default();
    body.push('\n');
    let _ = (&conn).write_all(body.as_bytes());
    // Answer first, then retire: the reply must be on the wire before the
    // loop tears the socket down.
    if !keep_running {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }
    INFLIGHT_REQUESTS.fetch_sub(1, Ordering::SeqCst);
}

/// Accept for up to ``timeout`` seconds, handling each request off-loop.
///
/// Handlers run on their own thread because their budgets differ by an order
/// of magnitude: a delivery may hold the native transport for
/// ``send_request_timeout()`` while ``hive team`` / ``hive doctor`` give up
/// after ``SOCKET_READY_TIMEOUT`` and report a missing hived. Serving them
/// in accept order made one slow send fake the hived's death for every
/// short read behind it.
pub(crate) fn serve_requests(
    server: &dyn HivedServerApi,
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
    hived_started_at: &str,
    timeout: f64,
) -> bool {
    let end = monotonic() + timeout;
    while !SHUTDOWN.load(Ordering::SeqCst) {
        let remaining = end - monotonic();
        if remaining <= 0.0 {
            break;
        }
        let Some(conn) = server.accept_timeout(remaining) else {
            break;
        };
        let workspace = workspace.to_string();
        let team = team.to_string();
        let tmux_window = tmux_window.to_string();
        let tmux_window_id = tmux_window_id.to_string();
        let hived_started_at = hived_started_at.to_string();
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
                );
            });
    }
    !SHUTDOWN.load(Ordering::SeqCst)
}
