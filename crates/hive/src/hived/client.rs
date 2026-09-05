// --------------------------------------------------------------------------
// client side: request helpers
// --------------------------------------------------------------------------

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::{Map, Value};

use super::*;

pub fn _request_hived(
    workspace: &str,
    payload: &Map<String, Value>,
    timeout: f64,
) -> Option<Map<String, Value>> {
    let path = _socket_path(workspace);
    if !path.exists() {
        return None;
    }
    let inner = || -> std::io::Result<Vec<u8>> {
        let mut client = UnixStream::connect(&path)?;
        let dur = Some(Duration::from_secs_f64(timeout.max(0.001)));
        client.set_read_timeout(dur)?;
        client.set_write_timeout(dur)?;
        let mut body = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
        body.push('\n');
        client.write_all(body.as_bytes())?;
        client.shutdown(std::net::Shutdown::Write)?;
        let mut chunks = Vec::new();
        let mut buf = [0u8; 65536];
        loop {
            let n = client.read(&mut buf)?;
            if n == 0 {
                break;
            }
            chunks.extend_from_slice(&buf[..n]);
        }
        Ok(chunks)
    };
    let chunks = inner().ok()?;
    if chunks.is_empty() {
        return None;
    }
    match serde_json::from_slice::<Value>(&chunks) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

pub(super) fn action_payload(action: &str) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("action".to_string(), Value::from(action));
    payload
}

pub fn request_ping_impl(workspace: &str) -> Option<Map<String, Value>> {
    _request_hived(workspace, &action_payload("ping"), SOCKET_RETRY_INTERVAL)
}

pub fn _socket_alive(workspace: &str) -> bool {
    let response = hooked_request_ping(workspace);
    match response {
        Some(map) => {
            map.get("ok") == Some(&Value::Bool(true))
                && map.get("apiVersion") == Some(&Value::from(HIVED_API_VERSION))
        }
        None => false,
    }
}

/// Ask the hived to bring its shared-daemon codex client online now.
///
/// Called at spawn time so the client holds the broadcast stream before the
/// member's first turn. Best-effort: returns None when the hived is down,
/// and the lazy connect on the next runtime tick covers that case.
pub fn request_connect_codex(workspace: &str) -> Option<Map<String, Value>> {
    _request_hived(workspace, &action_payload("connect-codex"), 3.0)
}

/// Ask the hived to bring the grok 2nd client for the pane's daemon key online now.
///
/// Called at spawn time so the stdio client has loaded the pane's session
/// before its first turn: ``session/load`` replays past updates, and a replay
/// is not evidence — only a live-attached client sees the first real turn.
/// Best-effort: returns None when the hived is down, and the lazy connect on
/// the next runtime tick covers that case.
pub fn request_connect_grok(workspace: &str, pane: &str) -> Option<Map<String, Value>> {
    let mut payload = action_payload("connect-grok");
    payload.insert("pane".to_string(), Value::from(pane));
    _request_hived(workspace, &payload, 3.0)
}

pub fn _hived_identity_matches(response: Option<&Map<String, Value>>, team: &str) -> bool {
    // Hived identity is (workspace socket, team) — never the window.
    //
    // The window is display: it can die, move, or be recreated by attach
    // without the team changing, so a window mismatch must not bounce a
    // healthy hived (and with it every live delivery client it holds).
    match response {
        Some(map) => {
            map.get("ok") == Some(&Value::Bool(true))
                && map.get("apiVersion") == Some(&Value::from(HIVED_API_VERSION))
                && map.get("buildHash").and_then(Value::as_str) == Some(hived_build_hash())
                && map.get("team").and_then(Value::as_str) == Some(team)
        }
        None => false,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn request_send(
    workspace: &str,
    team: &str,
    sender_agent: &str,
    target_agent: &str,
    body: &str,
    artifact: &str,
    reply_to: &str,
) -> Option<Map<String, Value>> {
    let timeout = _send_request_timeout();
    let mut payload = action_payload("send");
    payload.insert("team".to_string(), Value::from(team));
    payload.insert("senderAgent".to_string(), Value::from(sender_agent));
    payload.insert("targetAgent".to_string(), Value::from(target_agent));
    payload.insert("body".to_string(), Value::from(body));
    payload.insert("artifact".to_string(), Value::from(artifact));
    payload.insert("replyTo".to_string(), Value::from(reply_to));
    _request_hived(workspace, &payload, timeout)
}

pub fn request_doctor(
    workspace: &str,
    team: &str,
    target_agent: &str,
    verbose: bool,
) -> Option<Map<String, Value>> {
    let mut payload = action_payload("doctor");
    payload.insert("team".to_string(), Value::from(team));
    payload.insert("agent".to_string(), Value::from(target_agent));
    payload.insert("verbose".to_string(), Value::from(verbose));
    _request_hived(workspace, &payload, SOCKET_READY_TIMEOUT)
}

pub fn request_team_runtime(workspace: &str, team: &str) -> Option<Map<String, Value>> {
    let mut payload = action_payload("team-runtime");
    payload.insert("team".to_string(), Value::from(team));
    _request_hived(workspace, &payload, SOCKET_READY_TIMEOUT)
}

pub fn request_runtime_snapshot(workspace: &str, pane_id: &str) -> Option<Map<String, Value>> {
    let mut payload = action_payload("runtime-snapshot");
    payload.insert("pane".to_string(), Value::from(pane_id));
    _request_hived(workspace, &payload, SOCKET_READY_TIMEOUT)
}

pub fn request_thread(workspace: &str, message_id: &str) -> Option<Map<String, Value>> {
    let mut payload = action_payload("thread");
    payload.insert("msgId".to_string(), Value::from(message_id));
    _request_hived(workspace, &payload, SOCKET_READY_TIMEOUT)
}
