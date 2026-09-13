// --------------------------------------------------------------------------
// client side: request helpers
// --------------------------------------------------------------------------

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value};

use super::*;

/// Why a request got no answer. `NoListener`, `NotSent` and `NotAdmitted`:
/// the business payload never reached the hived — no socket or nobody on
/// it, the connect or the write failed, or the hived answered the
/// admission preflight with a refusal (or went away before answering it) —
/// so nothing was served and the request may be sent again.
/// `Incompatible`: a hived answered, but speaks another api than this
/// binary; the payload was withheld. Not retried.
/// `AnswerLost`: the request went out whole and the answer did not come
/// back (read failed or timed out, empty, unparsable), so the hived may
/// have served it. A caller with a side effect on the line (a node
/// dispatch) must not take `AnswerLost` for a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestFailure {
    /// No socket, or a socket file nobody listens on (a hived that died
    /// without cleaning up): there is no hived to ask.
    NoListener,
    /// The request never reached a hived, for a reason that is not
    /// "nobody there" (a permission error, a failed timeout or write).
    NotSent(String),
    /// The hived did not admit the request: the gate was shut (it is
    /// retiring or draining), or the connection ended at the preflight.
    /// No business payload was written.
    NotAdmitted(String),
    Incompatible(String),
    AnswerLost(String),
}

fn connect_hived(workspace: &str) -> Result<UnixStream, RequestFailure> {
    let path = socket_path(workspace);
    if !path.exists() {
        return Err(RequestFailure::NoListener);
    }
    UnixStream::connect(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound => {
            RequestFailure::NoListener
        }
        _ => RequestFailure::NotSent(e.to_string()),
    })
}

/// The reply to a request the hived took whole: every failure from here is
/// a lost answer, not an unsent request.
fn read_answer(mut reader: impl Read) -> Result<Map<String, Value>, RequestFailure> {
    let lost = |e: std::io::Error| RequestFailure::AnswerLost(e.to_string());
    let mut chunks = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = reader.read(&mut buf).map_err(lost)?;
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

fn json_line(payload: &Map<String, Value>) -> String {
    let mut body = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    body.push('\n');
    body
}

/// The one-shot request format: the whole payload, then a read to EOF.
/// This is the format read-only and control requests use — ping, doctor,
/// shutdown — and the one an older hived understands, so a build that
/// changed the api can still identify and retire the generation it found.
pub(crate) fn request_hived_answer(
    workspace: &str,
    payload: &Map<String, Value>,
    timeout: f64,
) -> Result<Map<String, Value>, RequestFailure> {
    let dur = Some(Duration::from_secs_f64(timeout.max(0.001)));
    let not_sent = |e: std::io::Error| RequestFailure::NotSent(e.to_string());
    let mut client = connect_hived(workspace)?;
    client.set_read_timeout(dur).map_err(not_sent)?;
    client.set_write_timeout(dur).map_err(not_sent)?;
    client
        .write_all(json_line(payload).as_bytes())
        .map_err(not_sent)?;
    // From here the whole request is with the hived: every failure is a
    // lost answer, not an unsent request.
    let lost = |e: std::io::Error| RequestFailure::AnswerLost(e.to_string());
    client.shutdown(std::net::Shutdown::Write).map_err(lost)?;
    read_answer(&client)
}

/// A request with a side effect on the line, sent only once the hived has
/// admitted it on this very connection.
///
/// The preflight `{"action":"admit","forAction":<action>}` goes out first;
/// the hived answers `admitted` with its api version while it holds a
/// lease for this connection, or refuses. Only after that answer is the
/// payload written, so every failure up to it — a refusal, an EOF, a reset
/// as a retiring desk closes its listener, a timeout — is `NotAdmitted`
/// with nothing sent, and the caller may try again. From the payload write
/// on, the conservative `AnswerLost` rule applies.
pub(crate) fn request_admitted(
    workspace: &str,
    payload: &Map<String, Value>,
    timeout: f64,
) -> Result<Map<String, Value>, RequestFailure> {
    use std::io::BufRead;
    let not_sent = |e: std::io::Error| RequestFailure::NotSent(e.to_string());
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let client = connect_hived(workspace)?;
    let preflight_budget = Some(Duration::from_secs_f64(IDENTITY_PING_TIMEOUT));
    client
        .set_read_timeout(preflight_budget)
        .map_err(not_sent)?;
    client
        .set_write_timeout(preflight_budget)
        .map_err(not_sent)?;
    let mut preflight = action_payload(ADMIT_ACTION);
    preflight.insert("forAction".to_string(), Value::from(action.clone()));
    let not_admitted =
        |what: &str, e: std::io::Error| RequestFailure::NotAdmitted(format!("{what}: {e}"));
    (&client)
        .write_all(json_line(&preflight).as_bytes())
        .map_err(|e| not_admitted("preflight not sent", e))?;
    let mut reader = std::io::BufReader::new(&client);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => {
            return Err(RequestFailure::NotAdmitted(
                "hived closed the connection before admitting".to_string(),
            ))
        }
        Ok(_) => {}
        Err(e) => return Err(not_admitted("preflight answer lost", e)),
    }
    let answer = match serde_json::from_str::<Value>(&line) {
        Ok(Value::Object(map)) => map,
        _ => {
            return Err(RequestFailure::NotAdmitted(
                "preflight answer is not a JSON object".to_string(),
            ))
        }
    };
    if answer.get("ok") != Some(&Value::Bool(true))
        || answer.get("admitted") != Some(&Value::Bool(true))
    {
        return Err(RequestFailure::NotAdmitted(
            answer
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("request not admitted")
                .to_string(),
        ));
    }
    let api = answer.get("apiVersion").and_then(Value::as_i64);
    if api != Some(HIVED_API_VERSION) {
        return Err(RequestFailure::Incompatible(format!(
            "hived speaks api {}, this binary api {HIVED_API_VERSION}; '{action}' not sent",
            api.map_or("unknown".to_string(), |v| v.to_string())
        )));
    }
    let dur = Some(Duration::from_secs_f64(timeout.max(0.001)));
    client.set_read_timeout(dur).map_err(not_sent)?;
    client.set_write_timeout(dur).map_err(not_sent)?;
    // From the first payload byte the hived may have served it.
    let lost = |e: std::io::Error| RequestFailure::AnswerLost(e.to_string());
    (&client)
        .write_all(json_line(payload).as_bytes())
        .map_err(lost)?;
    client.shutdown(std::net::Shutdown::Write).map_err(lost)?;
    read_answer(reader)
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

pub fn request_ping_impl(workspace: &str, timeout: f64) -> Option<Map<String, Value>> {
    request_hived(workspace, &action_payload("ping"), timeout)
}

#[cfg(test)]
pub(crate) fn socket_alive(workspace: &str) -> bool {
    let response = hooked_request_ping(workspace, SOCKET_RETRY_INTERVAL);
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
    request_admitted(workspace, &action_payload("connect-codex"), 3.0).ok()
}

/// Ask the hived to bring the grok 2nd client for the pane's daemon key online now.
///
/// Called at spawn time so the stdio client has loaded the pane's session
/// before its first turn: ``session/load`` replays past updates, which the
/// display ignores — only a live-attached client sees the first real turn
/// as busy.
/// Best-effort: returns None when the hived is down, and the lazy connect on
/// the next runtime tick covers that case.
pub fn request_connect_grok(workspace: &str, pane: &str) -> Option<Map<String, Value>> {
    let mut payload = action_payload("connect-grok");
    payload.insert("pane".to_string(), Value::from(pane));
    request_admitted(workspace, &payload, 3.0).ok()
}

/// What a ping answer says about the hived on the workspace socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HivedIdentity {
    /// This build, this api version, this team, this hive home.
    Matches,
    /// No hived, or one of this hive home that is another build, api
    /// version or team: replace it from this binary.
    Restart,
    /// A hived is there and did not admit the ping: its gate is shut for a
    /// retirement it may yet cancel, or a drain. Not a generation to
    /// replace — ask again shortly.
    Busy,
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
    if map.get("ok") == Some(&Value::Bool(false))
        && map.get("notAdmitted") == Some(&Value::Bool(true))
    {
        return HivedIdentity::Busy;
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
pub(crate) fn request_send(
    workspace: &str,
    team: &str,
    sender_agent: &str,
    target_agent: &str,
    body: &str,
    artifact: &str,
) -> Result<Map<String, Value>, RequestFailure> {
    let timeout = send_request_timeout();
    let mut payload = action_payload("send");
    payload.insert("team".to_string(), Value::from(team));
    payload.insert("senderAgent".to_string(), Value::from(sender_agent));
    payload.insert("targetAgent".to_string(), Value::from(target_agent));
    payload.insert("body".to_string(), Value::from(body));
    payload.insert("artifact".to_string(), Value::from(artifact));
    request_admitted(workspace, &payload, timeout)
}

/// A `hive workflow run` dispatch: the same transport as a send, no sender.
/// The failure kind is kept: a dispatch whose answer was lost may have
/// been injected, and the node must not repeat it.
pub(crate) fn request_node_dispatch(
    workspace: &str,
    team: &str,
    target_agent: &str,
    body: &str,
    artifact: &str,
    dispatch_id: &str,
) -> Result<Map<String, Value>, RequestFailure> {
    let timeout = send_request_timeout();
    let mut payload = action_payload("node-dispatch");
    payload.insert("team".to_string(), Value::from(team));
    payload.insert("dispatchId".to_string(), Value::from(dispatch_id));
    payload.insert("targetAgent".to_string(), Value::from(target_agent));
    payload.insert("body".to_string(), Value::from(body));
    payload.insert("artifact".to_string(), Value::from(artifact));
    request_admitted(workspace, &payload, timeout)
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

/// `request_team_runtime` telling its two failures apart: `NoListener` (no
/// hived listens — a socket file nobody answers on is a dead hived's
/// leftover) from `AnswerLost` (one does, and did not answer).
pub(crate) fn request_team_runtime_answer(
    workspace: &str,
    team: &str,
) -> Result<Map<String, Value>, RequestFailure> {
    let mut payload = action_payload("team-runtime");
    payload.insert("team".to_string(), Value::from(team));
    request_hived_answer(workspace, &payload, SOCKET_READY_TIMEOUT)
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

/// Ask the hived what became of a node dispatch (`node-result`): the
/// answer's `state` is `running`, `ended` (with `status`, `text`, `error`)
/// or `unknown` (with `reason`).
pub fn request_node_result(workspace: &str, dispatch_id: &str) -> Option<Map<String, Value>> {
    let mut payload = action_payload("node-result");
    payload.insert("dispatchId".to_string(), Value::from(dispatch_id));
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
    fn test_request_hived_answer_finds_no_listener_without_a_socket() {
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
        assert!(matches!(err, RequestFailure::NoListener), "{err:?}");

        // A socket nobody listens on (a hived that died without cleaning up).
        let socket = run_tmp.path().join("hived.sock");
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        let err = request_hived_answer("/tmp/ws-x", &action_payload("ping"), 0.5).unwrap_err();
        assert!(matches!(err, RequestFailure::NoListener), "{err:?}");

        // A regular file where the socket should be is neither a hived nor
        // "nobody listens": unsent, with the reason kept.
        std::fs::remove_file(&socket).unwrap();
        std::fs::write(&socket, "").unwrap();
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
