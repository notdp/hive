// --------------------------------------------------------------------------
// client side: request helpers
// --------------------------------------------------------------------------

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value};

use super::*;

/// Why a request got no answer. `NotSent`: it never reached the hived —
/// no socket, the connect or the write failed — so nothing was served.
/// `AnswerLost`: the request went out whole and the answer did not come
/// back (read failed or timed out, empty, unparsable), so the hived may
/// have served it. A caller with a side effect on the line (a node
/// dispatch) must not take `AnswerLost` for a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestFailure {
    NotSent(String),
    AnswerLost(String),
}

pub(crate) fn request_hived_answer(
    workspace: &str,
    payload: &Map<String, Value>,
    timeout: f64,
) -> Result<Map<String, Value>, RequestFailure> {
    let path = socket_path(workspace);
    if !path.exists() {
        return Err(RequestFailure::NotSent(format!(
            "no hived socket at {}",
            path.display()
        )));
    }
    let dur = Some(Duration::from_secs_f64(timeout.max(0.001)));
    let not_sent = |e: std::io::Error| RequestFailure::NotSent(e.to_string());
    let mut client = UnixStream::connect(&path).map_err(not_sent)?;
    client.set_read_timeout(dur).map_err(not_sent)?;
    client.set_write_timeout(dur).map_err(not_sent)?;
    let mut body = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    body.push('\n');
    client.write_all(body.as_bytes()).map_err(not_sent)?;
    // From here the whole request is with the hived: every failure is a
    // lost answer, not an unsent request.
    let lost = |e: std::io::Error| RequestFailure::AnswerLost(e.to_string());
    client.shutdown(std::net::Shutdown::Write).map_err(lost)?;
    let mut chunks = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = client.read(&mut buf).map_err(lost)?;
        if n == 0 {
            break;
        }
        chunks.extend_from_slice(&buf[..n]);
    }
    if chunks.is_empty() {
        return Err(RequestFailure::AnswerLost("empty answer".to_string()));
    }
    match serde_json::from_slice::<Value>(&chunks) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(RequestFailure::AnswerLost(
            "answer is not a JSON object".to_string(),
        )),
    }
}

pub(crate) fn request_hived(
    workspace: &str,
    payload: &Map<String, Value>,
    timeout: f64,
) -> Option<Map<String, Value>> {
    request_hived_answer(workspace, payload, timeout).ok()
}

pub(super) fn action_payload(action: &str) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("action".to_string(), Value::from(action));
    payload
}

pub fn request_ping_impl(workspace: &str) -> Option<Map<String, Value>> {
    request_hived(workspace, &action_payload("ping"), SOCKET_RETRY_INTERVAL)
}

#[cfg(test)]
pub(crate) fn socket_alive(workspace: &str) -> bool {
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
    request_hived(workspace, &action_payload("connect-codex"), 3.0)
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
    request_hived(workspace, &payload, 3.0)
}

/// What a ping answer says about the hived on the workspace socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HivedIdentity {
    /// This build, this api version, this team, this hive home.
    Matches,
    /// No hived, or one of this hive home that is another build, api
    /// version or team: replace it from this binary.
    Restart,
    /// A hived serving the workspace from another `HIVE_HOME` (the path it
    /// reported). Not this hive's to restart: a replacement started from
    /// here would run with this home, could not see that team's registry,
    /// and would reap the members it does not own.
    ForeignHome(String),
}

pub(crate) fn hived_identity(response: Option<&Map<String, Value>>, team: &str) -> HivedIdentity {
    // Hived identity is (workspace socket, team, hive home) — never the
    // window.
    //
    // The window is display: it can die, move, or be recreated by attach
    // without the team changing, so a window mismatch must not bounce a
    // healthy hived (and with it every live delivery client it holds).
    let Some(map) = response else {
        return HivedIdentity::Restart;
    };
    if let Some(home) = map.get("hiveHome").and_then(Value::as_str) {
        if Path::new(home) != crate::paths::hive_home().as_path() {
            return HivedIdentity::ForeignHome(home.to_string());
        }
    }
    let matches = map.get("ok") == Some(&Value::Bool(true))
        && map.get("apiVersion") == Some(&Value::from(HIVED_API_VERSION))
        && map.get("buildHash").and_then(Value::as_str) == Some(hived_build_hash())
        && map.get("team").and_then(Value::as_str) == Some(team);
    if matches {
        HivedIdentity::Matches
    } else {
        HivedIdentity::Restart
    }
}

pub(crate) fn hived_identity_matches(response: Option<&Map<String, Value>>, team: &str) -> bool {
    hived_identity(response, team) == HivedIdentity::Matches
}

#[allow(clippy::too_many_arguments)]
pub fn request_send(
    workspace: &str,
    team: &str,
    sender_agent: &str,
    target_agent: &str,
    body: &str,
    artifact: &str,
) -> Option<Map<String, Value>> {
    let timeout = send_request_timeout();
    let mut payload = action_payload("send");
    payload.insert("team".to_string(), Value::from(team));
    payload.insert("senderAgent".to_string(), Value::from(sender_agent));
    payload.insert("targetAgent".to_string(), Value::from(target_agent));
    payload.insert("body".to_string(), Value::from(body));
    payload.insert("artifact".to_string(), Value::from(artifact));
    request_hived(workspace, &payload, timeout)
}

/// A `hive node run` dispatch: the same transport as a send, no sender.
/// The failure kind is kept: a dispatch whose answer was lost may have
/// been injected, and the node must not repeat it.
pub(crate) fn request_node_dispatch(
    workspace: &str,
    team: &str,
    target_agent: &str,
    body: &str,
    artifact: &str,
) -> Result<Map<String, Value>, RequestFailure> {
    let timeout = send_request_timeout();
    let mut payload = action_payload("node-dispatch");
    payload.insert("team".to_string(), Value::from(team));
    payload.insert("targetAgent".to_string(), Value::from(target_agent));
    payload.insert("body".to_string(), Value::from(body));
    payload.insert("artifact".to_string(), Value::from(artifact));
    request_hived_answer(workspace, &payload, timeout)
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
    request_hived(workspace, &payload, SOCKET_READY_TIMEOUT)
}

pub fn request_team_runtime(workspace: &str, team: &str) -> Option<Map<String, Value>> {
    let mut payload = action_payload("team-runtime");
    payload.insert("team".to_string(), Value::from(team));
    request_hived(workspace, &payload, SOCKET_READY_TIMEOUT)
}

/// Ask the hived whether a member has a turn open (`turn-open`): the
/// answer's `open` is a bool, or null when the hived holds no such state
/// for the member.
pub fn request_turn_open(workspace: &str, team: &str, agent: &str) -> Option<Map<String, Value>> {
    let mut payload = action_payload("turn-open");
    payload.insert("team".to_string(), Value::from(team));
    payload.insert("agent".to_string(), Value::from(agent));
    request_hived(workspace, &payload, SOCKET_READY_TIMEOUT)
}

pub fn request_runtime_snapshot(workspace: &str, pane_id: &str) -> Option<Map<String, Value>> {
    let mut payload = action_payload("runtime-snapshot");
    payload.insert("pane".to_string(), Value::from(pane_id));
    request_hived(workspace, &payload, SOCKET_READY_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hived::testhook::{install, Hook};
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;

    /// A run dir under /tmp (short enough for the socket to live in-tree)
    /// hooked in as the workspace's, and a listener on its socket path
    /// that serves one connection with `reply`: drains the request, then
    /// runs the reply against the connection.
    fn one_shot_hived(
        reply: impl FnOnce(&mut UnixStream) + Send + 'static,
    ) -> (tempfile::TempDir, crate::hived::testhook::Guard) {
        let run_tmp = tempfile::Builder::new()
            .prefix("hrq")
            .tempdir_in("/tmp")
            .unwrap();
        let run_dir = run_tmp.path().to_path_buf();
        let guard = install(Hook {
            run_dir: Some(Arc::new(move |_ws| run_dir.clone())),
            ..Default::default()
        });
        let listener = UnixListener::bind(run_tmp.path().join("hived.sock")).unwrap();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 65536];
            loop {
                match conn.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
            reply(&mut conn);
        });
        (run_tmp, guard)
    }

    #[test]
    fn test_request_hived_answer_is_not_sent_without_a_socket() {
        let run_tmp = tempfile::Builder::new()
            .prefix("hrq")
            .tempdir_in("/tmp")
            .unwrap();
        let run_dir = run_tmp.path().to_path_buf();
        let _guard = install(Hook {
            run_dir: Some(Arc::new(move |_ws| run_dir.clone())),
            ..Default::default()
        });
        let err = request_hived_answer("/tmp/ws-x", &action_payload("ping"), 0.5).unwrap_err();
        assert!(
            matches!(&err, RequestFailure::NotSent(reason) if reason.contains("no hived socket")),
            "{err:?}"
        );

        // A socket file nobody listens on: the connect fails, nothing was sent.
        let _ = std::fs::write(run_tmp.path().join("hived.sock"), "");
        let err = request_hived_answer("/tmp/ws-x", &action_payload("ping"), 0.5).unwrap_err();
        assert!(matches!(err, RequestFailure::NotSent(_)), "{err:?}");
    }

    #[test]
    fn test_request_hived_answer_returns_the_served_object() {
        let (_run, _guard) = one_shot_hived(|conn| {
            let _ = conn.write_all(b"{\"ok\": true, \"seq\": 7}\n");
        });
        let answer = request_hived_answer("/tmp/ws-x", &action_payload("ping"), 2.0).unwrap();
        assert_eq!(answer["ok"], Value::Bool(true));
        assert_eq!(answer["seq"], Value::from(7));
    }

    #[test]
    fn test_request_hived_answer_reports_a_lost_answer_after_the_request_went_out() {
        // The request was drained and the connection closed with no reply.
        let (_run, _guard) = one_shot_hived(|_conn| {});
        let err = request_hived_answer("/tmp/ws-x", &action_payload("ping"), 2.0).unwrap_err();
        assert_eq!(err, RequestFailure::AnswerLost("empty answer".to_string()));

        // A reply that is not a JSON object.
        let (_run, _guard) = one_shot_hived(|conn| {
            let _ = conn.write_all(b"[1, 2]\n");
        });
        let err = request_hived_answer("/tmp/ws-x", &action_payload("ping"), 2.0).unwrap_err();
        assert_eq!(
            err,
            RequestFailure::AnswerLost("answer is not a JSON object".to_string())
        );

        // A reply held past the read timeout.
        let (_run, _guard) = one_shot_hived(|conn| {
            std::thread::sleep(Duration::from_secs_f64(1.0));
            let _ = conn.write_all(b"{\"ok\": true}\n");
        });
        let err = request_hived_answer("/tmp/ws-x", &action_payload("ping"), 0.2).unwrap_err();
        assert!(matches!(err, RequestFailure::AnswerLost(_)), "{err:?}");
        // The Option form folds every failure kind away.
        assert_eq!(
            request_hived("/tmp/ws-x", &action_payload("ping"), 0.2),
            None
        );
    }
}
