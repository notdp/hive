//! The claude hooks endpoint: where a claude member's own engine reports
//! its turn boundaries.
//!
//! Claude Code's function hooks run inside the engine and reach the host
//! only through `$`; the hive plugin's hooks module
//! (`plugins/hive/mod/register.ts`) posts `session.start`, `turn.start`
//! and `turn.complete` over `$.http.fetch` to a loopback port this module
//! listens on. The engine's own turn boundary thereby becomes a signal the
//! hived holds, the way codex's `turn/completed` and grok's prompt
//! response already are. `$.process.run` is CLI-only, so HTTP is the one
//! path a desktop session's hooks can take too.
//!
//! Discovery is a file, `<workspace>/run/hooks-endpoint.json`, mode 0600,
//! written atomically once the listener is up and removed when this
//! generation leaves: the port, a bearer token minted per generation, the
//! team instance (`teamCreatedAt`) and the generation (`startedAt`). The
//! plugin finds the workspace through the roster row carrying its session
//! id and reads this file; it decides no membership.
//!
//! A request is admitted when its bearer token is this generation's, its
//! `teamCreatedAt` is this instance's and its `sessionId` is a claude row
//! of this team's roster, read per request (the CLI owns membership; the
//! endpoint only observes). Anything else is refused with a status and
//! touches no observation.
//!
//! What the observations answer: [`hook_busy`], whether the engine is
//! inside a turn, while the channel is fresh. Claude Code skips a hook
//! that throws, overruns or answers the wrong shape and carries on
//! (fail-open), so a `turn.complete` can be lost; the answer therefore
//! expires [`HOOK_FRESH_SECONDS`] after the last event and the registry
//! status (`busy.rs::claude_registry_busy`) stands again. A subagent's
//! turn (`agentId` set) is acknowledged and not counted: the main loop's
//! busy is what the display shows.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::{Map, Value};

use super::*;

/// The endpoint description beside `hived.sock`.
pub const HOOKS_ENDPOINT_NAME: &str = "hooks-endpoint.json";
/// The one route.
pub const HOOKS_PATH: &str = "/v1/hooks";
/// How long the last hook event vouches for the engine's turn state.
pub const HOOK_FRESH_SECONDS: f64 = 600.0;
const READ_BUDGET: Duration = Duration::from_secs(2);
const MAX_HEAD_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;
const RECENT_EVENT_IDS: usize = 512;
const ACCEPT_POLL_SECONDS: f64 = 0.1;
const EVENTS: [&str; 3] = ["session.start", "turn.start", "turn.complete"];

/// Who may post: the roster's claude row for a session id, by name.
pub(crate) type RosterLookup = dyn Fn(&str) -> Option<String> + Send + Sync;

pub(crate) struct HookContext {
    pub token: String,
    pub team: String,
    /// `team::created_at_key` of the instance; empty when the entry had
    /// none, and then not checked.
    pub created: String,
    pub workspace: String,
    pub roster: Box<RosterLookup>,
}

/// One session's last report.
#[derive(Clone, Debug)]
pub(crate) struct HookObservation {
    pub last_event: String,
    pub turn_id: Option<String>,
    pub seen: Instant,
}

fn observations() -> &'static Mutex<HashMap<String, HookObservation>> {
    static CELL: OnceLock<Mutex<HashMap<String, HookObservation>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(HashMap::new()))
}

fn recent_event_ids() -> &'static Mutex<VecDeque<String>> {
    static CELL: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn current() -> &'static Mutex<Option<Arc<HooksEndpoint>>> {
    static CELL: OnceLock<Mutex<Option<Arc<HooksEndpoint>>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// Whether *session_id*'s engine is inside a turn, by its own hooks, or
/// None when no fresh report exists and the registry status decides.
pub(crate) fn hook_busy(session_id: &str) -> Option<bool> {
    hook_busy_at(session_id, Instant::now())
}

fn hook_busy_at(session_id: &str, now: Instant) -> Option<bool> {
    fresh_observation_at(session_id, now).map(|seen| seen.turn_id.is_some())
}

/// *session_id*'s last report while it is fresh: `hook_busy` with the
/// event that produced it, for the runtime payload.
pub(crate) fn fresh_observation(session_id: &str) -> Option<HookObservation> {
    fresh_observation_at(session_id, Instant::now())
}

fn fresh_observation_at(session_id: &str, now: Instant) -> Option<HookObservation> {
    if session_id.is_empty() {
        return None;
    }
    let store = observations().lock().unwrap_or_else(|e| e.into_inner());
    let seen = store.get(session_id)?;
    if now.duration_since(seen.seen).as_secs_f64() > HOOK_FRESH_SECONDS {
        return None;
    }
    Some(seen.clone())
}

// --------------------------------------------------------------------------
// endpoint file
// --------------------------------------------------------------------------

pub fn hooks_endpoint_path(workspace: &str) -> PathBuf {
    hooked_run_dir(workspace).join(HOOKS_ENDPOINT_NAME)
}

fn write_endpoint_file(workspace: &str, payload: &Map<String, Value>) -> std::io::Result<()> {
    let path = hooks_endpoint_path(workspace);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_file_name(format!("{HOOKS_ENDPOINT_NAME}.{}.tmp", getpid()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(
            serde_json::to_string(payload)
                .unwrap_or_default()
                .as_bytes(),
        )?;
        file.sync_all()?;
        fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Remove the description when it still names *token*: a later generation's
/// file is left alone.
fn remove_endpoint_file_if(workspace: &str, token: &str) {
    let path = hooks_endpoint_path(workspace);
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let names_token = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("token").and_then(Value::as_str).map(|t| t == token))
        .unwrap_or(false);
    if names_token {
        let _ = fs::remove_file(&path);
    }
}

fn mint_token() -> String {
    let mut bytes = [0u8; 16];
    if fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_err()
    {
        use sha2::Digest;
        let seed = format!(
            "{}:{:?}:{}",
            getpid(),
            Instant::now(),
            crate::clock::utc_timestamp_ms()
        );
        bytes.copy_from_slice(&sha2::Sha256::digest(seed.as_bytes())[..16]);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// --------------------------------------------------------------------------
// listener
// --------------------------------------------------------------------------

pub(crate) struct HooksEndpoint {
    listener: Mutex<Option<TcpListener>>,
    closed: AtomicBool,
    worker: Mutex<Option<JoinHandle<()>>>,
    ctx: Arc<HookContext>,
    pub port: u16,
}

/// Bind the loopback listener, publish the description and start the
/// accept worker. The endpoint stays the generation's until
/// [`close_endpoint`].
pub(crate) fn open_endpoint(
    workspace: &str,
    team: &str,
    created: &str,
    started_at: &str,
) -> Result<Arc<HooksEndpoint>> {
    let team_name = team.to_string();
    let roster: Box<RosterLookup> = Box::new(move |sid| roster_claude_member(&team_name, sid));
    let ctx = HookContext {
        token: mint_token(),
        team: team.to_string(),
        created: created.to_string(),
        workspace: workspace.to_string(),
        roster,
    };
    let endpoint = open_endpoint_with(ctx, started_at)?;
    *current().lock().unwrap_or_else(|e| e.into_inner()) = Some(endpoint.clone());
    Ok(endpoint)
}

pub(crate) fn open_endpoint_with(ctx: HookContext, started_at: &str) -> Result<Arc<HooksEndpoint>> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let mut payload = Map::new();
    payload.insert("port".to_string(), Value::from(port));
    payload.insert("token".to_string(), Value::from(ctx.token.clone()));
    payload.insert(
        "teamCreatedAt".to_string(),
        Value::from(ctx.created.clone()),
    );
    payload.insert("startedAt".to_string(), Value::from(started_at));
    payload.insert("pid".to_string(), Value::from(getpid()));
    write_endpoint_file(&ctx.workspace, &payload).map_err(|e| {
        anyhow!(
            "cannot write {}: {e}",
            hooks_endpoint_path(&ctx.workspace).display()
        )
    })?;
    let endpoint = Arc::new(HooksEndpoint {
        listener: Mutex::new(Some(listener)),
        closed: AtomicBool::new(false),
        worker: Mutex::new(None),
        ctx: Arc::new(ctx),
        port,
    });
    let worker_endpoint = endpoint.clone();
    let worker = thread::Builder::new()
        .name("hived-hooks".to_string())
        .spawn(move || worker_endpoint.accept_loop())?;
    *endpoint.worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(worker);
    Ok(endpoint)
}

/// Close this generation's endpoint, if one is open: the listener goes,
/// the worker is joined, the description is removed when it is still ours.
pub(crate) fn close_endpoint() {
    let taken = current().lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(endpoint) = taken {
        endpoint.close();
    }
}

/// A roster row of *team* naming *session_id*'s claude engine, by member
/// name. A joined session's row carries the session id itself; a bg
/// member's row carries its jobId, and the engine registry says which
/// session id that job minted.
fn roster_claude_member(team: &str, session_id: &str) -> Option<String> {
    let entry = crate::registry::load(team)?;
    let members = entry.get("members")?.as_array()?;
    let job_session = |job_id: &str| hooked_cb_engine_session_for_job(job_id).map(|e| e.session_id);
    members
        .iter()
        .filter_map(Value::as_object)
        .find_map(|row| roster_row_member(row, session_id, &job_session))
}

/// The member name of *row* when it is a claude row for *session_id*:
/// its `sessionId` is that id, or a jobId *job_session* resolves to it.
fn roster_row_member(
    row: &Map<String, Value>,
    session_id: &str,
    job_session: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    use crate::json_fields::map_str;
    if session_id.is_empty() || map_str(row, "cli") != "claude" {
        return None;
    }
    let recorded = map_str(row, "sessionId");
    if recorded.is_empty() {
        return None;
    }
    let same = recorded == session_id
        || (recorded.len() < session_id.len()
            && job_session(&recorded).as_deref() == Some(session_id));
    same.then(|| map_str(row, "name"))
}

impl HooksEndpoint {
    #[cfg(test)]
    fn token(&self) -> &str {
        &self.ctx.token
    }

    fn wait_readable(&self) -> bool {
        let guard = self.listener.lock().unwrap_or_else(|e| e.into_inner());
        let Some(listener) = guard.as_ref() else {
            return false;
        };
        let mut pfd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = (ACCEPT_POLL_SECONDS * 1000.0) as i32;
        unsafe { libc::poll(&mut pfd, 1, ms) > 0 && pfd.revents & libc::POLLIN != 0 }
    }

    fn accept_loop(&self) {
        while !self.closed.load(Ordering::SeqCst) {
            if !self.wait_readable() {
                if self
                    .listener
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none()
                {
                    return;
                }
                continue;
            }
            let accepted = {
                let guard = self.listener.lock().unwrap_or_else(|e| e.into_inner());
                match guard.as_ref() {
                    Some(listener) => listener.accept().ok(),
                    None => return,
                }
            };
            let Some((stream, _)) = accepted else {
                continue;
            };
            let ctx = self.ctx.clone();
            let _ = thread::Builder::new()
                .name("hived-hooks-request".to_string())
                .spawn(move || serve_connection(stream, &ctx));
        }
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        *self.listener.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let worker = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
        remove_endpoint_file_if(&self.ctx.workspace, &self.ctx.token);
    }
}

impl Drop for HooksEndpoint {
    fn drop(&mut self) {
        self.close();
    }
}

// --------------------------------------------------------------------------
// HTTP
// --------------------------------------------------------------------------

struct HttpRequest {
    method: String,
    path: String,
    authorization: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    let mut buf: Vec<u8> = Vec::new();
    let head_end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        if buf.len() >= MAX_HEAD_BYTES {
            return None;
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut content_length = 0usize;
    let mut authorization = String::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.trim().parse().ok()?,
            "authorization" => authorization = value.trim().to_string(),
            _ => {}
        }
    }
    if content_length > MAX_BODY_BYTES {
        return None;
    }
    let mut body = buf.split_off(head_end);
    while body.len() < content_length {
        let mut chunk = vec![0u8; content_length - body.len()];
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Some(HttpRequest {
        method,
        path,
        authorization,
        body,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        _ => "Error",
    }
}

fn write_response(stream: &mut TcpStream, status: u16, body: &Map<String, Value>) {
    let json = serde_json::to_string(body).unwrap_or_default();
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason(status),
        json.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(json.as_bytes());
    let _ = stream.flush();
}

fn serve_connection(mut stream: TcpStream, ctx: &HookContext) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(READ_BUDGET));
    let _ = stream.set_write_timeout(Some(READ_BUDGET));
    let Some(request) = read_request(&mut stream) else {
        write_response(&mut stream, 400, &refusal("unreadable request"));
        return;
    };
    let (status, body) = handle_hook_request(
        ctx,
        &request.method,
        &request.path,
        &request.authorization,
        &request.body,
    );
    if status != 200 {
        // A refusal is worth a line: the plugin retries nothing loudly, so
        // this is where a stale token or a stranger's post shows up.
        hooked_notify_debug_emit(
            &ctx.workspace,
            "claude.hook_refused",
            &[
                ("team", Value::from(ctx.team.clone())),
                ("status", Value::from(status)),
                ("error", body.get("error").cloned().unwrap_or(Value::Null)),
                ("path", Value::from(request.path.clone())),
            ],
        );
    }
    write_response(&mut stream, status, &body);
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

fn refusal(error: &str) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("ok".to_string(), Value::Bool(false));
    map.insert("error".to_string(), Value::from(error));
    map
}

/// Decide one request: the status and the JSON body. Pure over *ctx*.
pub(crate) fn handle_hook_request(
    ctx: &HookContext,
    method: &str,
    path: &str,
    authorization: &str,
    body: &[u8],
) -> (u16, Map<String, Value>) {
    if method != "POST" {
        return (405, refusal("POST only"));
    }
    if path != HOOKS_PATH {
        return (404, refusal("no such route"));
    }
    let presented = authorization
        .strip_prefix("Bearer ")
        .map(str::trim)
        .unwrap_or("");
    if presented.is_empty() || !constant_time_eq(presented.as_bytes(), ctx.token.as_bytes()) {
        return (401, refusal("bad token"));
    }
    let event = match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => map,
        _ => return (400, refusal("body is not a JSON object")),
    };
    let created = map_get_str(&event, "teamCreatedAt");
    if !ctx.created.is_empty() && created != ctx.created {
        return (409, refusal("another team instance"));
    }
    let session_id = map_get_str(&event, "sessionId");
    let Some(member) = (ctx.roster)(&session_id) else {
        return (404, refusal("session not on this team's claude roster"));
    };
    let name = map_get_str(&event, "event");
    if !EVENTS.contains(&name.as_str()) {
        return (400, refusal("unknown event"));
    }
    let mut answer = Map::new();
    answer.insert("ok".to_string(), Value::Bool(true));
    answer.insert("member".to_string(), Value::from(member.clone()));
    let event_id = map_get_str(&event, "eventId");
    if !event_id.is_empty() && !remember_event_id(&event_id) {
        answer.insert("duplicate".to_string(), Value::Bool(true));
        return (200, answer);
    }
    let turn_id = map_get_str(&event, "turnId");
    let agent_id = map_get_str(&event, "agentId");
    if !agent_id.is_empty() {
        answer.insert("ignored".to_string(), Value::from("subagent"));
        return (200, answer);
    }
    apply_event(&session_id, &name, &turn_id);
    let reason = map_get_str(&event, "reason");
    hooked_notify_debug_emit(
        &ctx.workspace,
        "claude.hook",
        &[
            ("team", Value::from(ctx.team.clone())),
            ("member", Value::from(member)),
            ("hook", Value::from(name)),
            ("turnId", non_empty(&turn_id)),
            ("reason", non_empty(&reason)),
            ("surface", non_empty(&map_get_str(&event, "surface"))),
        ],
    );
    (200, answer)
}

fn non_empty(value: &str) -> Value {
    if value.is_empty() {
        Value::Null
    } else {
        Value::from(value)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// True the first time *event_id* is seen.
fn remember_event_id(event_id: &str) -> bool {
    let mut seen = recent_event_ids().lock().unwrap_or_else(|e| e.into_inner());
    if seen.iter().any(|id| id == event_id) {
        return false;
    }
    seen.push_back(event_id.to_string());
    while seen.len() > RECENT_EVENT_IDS {
        seen.pop_front();
    }
    true
}

/// The turn state machine: a start opens the turn it names, a complete
/// closes that turn (a late complete of an earlier turn leaves the open one
/// alone), a session start opens nothing.
fn apply_event(session_id: &str, event: &str, turn_id: &str) {
    let mut store = observations().lock().unwrap_or_else(|e| e.into_inner());
    let previous = store.get(session_id).and_then(|o| o.turn_id.clone());
    let turn = match event {
        "turn.start" => Some(turn_id.to_string()),
        "turn.complete" => match previous {
            Some(open) if !turn_id.is_empty() && open != turn_id => Some(open),
            _ => None,
        },
        _ => None,
    };
    store.insert(
        session_id.to_string(),
        HookObservation {
            last_event: event.to_string(),
            turn_id: turn,
            seen: Instant::now(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with(roster: Box<RosterLookup>, workspace: &str) -> HookContext {
        HookContext {
            token: "tok-1".to_string(),
            team: "probe".to_string(),
            created: "1700000000".to_string(),
            workspace: workspace.to_string(),
            roster,
        }
    }

    fn roster() -> Box<RosterLookup> {
        Box::new(|sid| (sid == "sid-a").then(|| "alpha".to_string()))
    }

    fn body(fields: &[(&str, &str)]) -> Vec<u8> {
        let mut map = Map::new();
        map.insert("teamCreatedAt".to_string(), Value::from("1700000000"));
        map.insert("sessionId".to_string(), Value::from("sid-a"));
        for (k, v) in fields {
            map.insert(k.to_string(), Value::from(*v));
        }
        serde_json::to_vec(&map).unwrap()
    }

    fn post(ctx: &HookContext, body: &[u8]) -> (u16, Map<String, Value>) {
        handle_hook_request(ctx, "POST", HOOKS_PATH, "Bearer tok-1", body)
    }

    #[test]
    fn test_hook_request_refuses_route_method_token_instance_and_stranger() {
        let ctx = ctx_with(roster(), "");
        let ok = body(&[("event", "turn.start"), ("turnId", "t1")]);
        assert_eq!(
            handle_hook_request(&ctx, "GET", HOOKS_PATH, "Bearer tok-1", &ok).0,
            405
        );
        assert_eq!(
            handle_hook_request(&ctx, "POST", "/v1/other", "Bearer tok-1", &ok).0,
            404
        );
        assert_eq!(
            handle_hook_request(&ctx, "POST", HOOKS_PATH, "Bearer nope", &ok).0,
            401
        );
        assert_eq!(
            handle_hook_request(&ctx, "POST", HOOKS_PATH, "", &ok).0,
            401
        );
        let mut other = Map::new();
        other.insert("teamCreatedAt".to_string(), Value::from("1600000000"));
        other.insert("sessionId".to_string(), Value::from("sid-a"));
        other.insert("event".to_string(), Value::from("turn.start"));
        assert_eq!(post(&ctx, &serde_json::to_vec(&other).unwrap()).0, 409);
        let mut stranger = Map::new();
        stranger.insert("teamCreatedAt".to_string(), Value::from("1700000000"));
        stranger.insert("sessionId".to_string(), Value::from("sid-z"));
        stranger.insert("event".to_string(), Value::from("turn.start"));
        assert_eq!(post(&ctx, &serde_json::to_vec(&stranger).unwrap()).0, 404);
        assert_eq!(post(&ctx, &body(&[("event", "tool.call")])).0, 400);
        assert_eq!(post(&ctx, b"not json").0, 400);
        assert!(
            hook_busy("sid-a").is_none(),
            "a refused request records nothing"
        );
    }

    #[test]
    fn test_roster_row_matches_a_session_id_or_the_job_that_minted_it() {
        let row = |cli: &str, sid: &str| {
            let mut m = Map::new();
            m.insert("name".to_string(), Value::from("w1"));
            m.insert("cli".to_string(), Value::from(cli));
            m.insert("sessionId".to_string(), Value::from(sid));
            m
        };
        let jobs = |job: &str| {
            (job == "66e7f18e").then(|| "66e7f18e-c50e-481d-961a-a7568b52ac0d".to_string())
        };
        let full = "66e7f18e-c50e-481d-961a-a7568b52ac0d";
        assert_eq!(
            roster_row_member(&row("claude", full), full, &jobs).as_deref(),
            Some("w1")
        );
        assert_eq!(
            roster_row_member(&row("claude", "66e7f18e"), full, &jobs).as_deref(),
            Some("w1")
        );
        assert_eq!(
            roster_row_member(&row("claude", "deadbeef"), full, &jobs),
            None
        );
        assert_eq!(roster_row_member(&row("codex", full), full, &jobs), None);
        assert_eq!(roster_row_member(&row("claude", ""), full, &jobs), None);
        assert_eq!(roster_row_member(&row("claude", full), "", &jobs), None);
    }

    #[test]
    fn test_hook_turn_state_opens_and_closes_the_named_turn() {
        let ctx = ctx_with(roster(), "");
        let (status, answer) = post(
            &ctx,
            &body(&[("event", "session.start"), ("surface", "terminal")]),
        );
        assert_eq!(status, 200);
        assert_eq!(answer.get("member"), Some(&Value::from("alpha")));
        assert_eq!(hook_busy("sid-a"), Some(false));
        post(&ctx, &body(&[("event", "turn.start"), ("turnId", "t1")]));
        assert_eq!(hook_busy("sid-a"), Some(true));
        // a late complete of another turn leaves the open one alone
        post(&ctx, &body(&[("event", "turn.complete"), ("turnId", "t0")]));
        assert_eq!(hook_busy("sid-a"), Some(true));
        post(
            &ctx,
            &body(&[
                ("event", "turn.complete"),
                ("turnId", "t1"),
                ("reason", "answer"),
            ]),
        );
        assert_eq!(hook_busy("sid-a"), Some(false));
        assert_eq!(
            fresh_observation("sid-a").unwrap().last_event,
            "turn.complete"
        );
    }

    #[test]
    fn test_hook_subagent_turns_and_duplicates_are_acknowledged_not_counted() {
        let ctx = ctx_with(roster(), "");
        post(&ctx, &body(&[("event", "session.start")]));
        let (status, answer) = post(
            &ctx,
            &body(&[
                ("event", "turn.start"),
                ("turnId", "sub-1"),
                ("agentId", "agent-9"),
            ]),
        );
        assert_eq!(status, 200);
        assert_eq!(answer.get("ignored"), Some(&Value::from("subagent")));
        assert_eq!(hook_busy("sid-a"), Some(false));
        let first = post(
            &ctx,
            &body(&[("event", "turn.start"), ("turnId", "t1"), ("eventId", "e1")]),
        );
        assert!(first.1.get("duplicate").is_none());
        post(
            &ctx,
            &body(&[
                ("event", "turn.complete"),
                ("turnId", "t1"),
                ("eventId", "e2"),
            ]),
        );
        let again = post(
            &ctx,
            &body(&[("event", "turn.start"), ("turnId", "t1"), ("eventId", "e1")]),
        );
        assert_eq!(again.1.get("duplicate"), Some(&Value::Bool(true)));
        assert_eq!(
            hook_busy("sid-a"),
            Some(false),
            "a replayed start does not reopen the turn"
        );
    }

    #[test]
    fn test_hook_busy_expires_after_the_freshness_window() {
        let ctx = ctx_with(roster(), "");
        post(&ctx, &body(&[("event", "turn.start"), ("turnId", "t1")]));
        let now = Instant::now();
        assert_eq!(hook_busy_at("sid-a", now), Some(true));
        let later = now + Duration::from_secs_f64(HOOK_FRESH_SECONDS + 1.0);
        assert_eq!(hook_busy_at("sid-a", later), None);
        assert_eq!(hook_busy_at("", now), None);
    }

    #[test]
    fn test_hook_endpoint_serves_http_publishes_and_removes_its_description() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_str().unwrap().to_string();
        let ctx = ctx_with(roster(), &workspace);
        let endpoint = open_endpoint_with(ctx, "2026-09-17T00:00:00Z").unwrap();
        let path = hooks_endpoint_path(&workspace);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let described: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(described["port"].as_u64(), Some(endpoint.port as u64));
        assert_eq!(described["token"].as_str(), Some(endpoint.token()));
        assert_eq!(described["teamCreatedAt"].as_str(), Some("1700000000"));
        assert_eq!(
            described["startedAt"].as_str(),
            Some("2026-09-17T00:00:00Z")
        );

        let request = |auth: &str, json: &str| {
            let mut conn =
                TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, endpoint.port))).unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let head = format!(
                "POST {HOOKS_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: {auth}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                json.len()
            );
            conn.write_all(head.as_bytes()).unwrap();
            conn.write_all(json.as_bytes()).unwrap();
            let mut out = String::new();
            conn.read_to_string(&mut out).unwrap();
            out
        };
        let token = format!("Bearer {}", endpoint.token());
        let ok = request(
            &token,
            r#"{"teamCreatedAt":"1700000000","sessionId":"sid-a","event":"turn.start","turnId":"t1"}"#,
        );
        assert!(ok.starts_with("HTTP/1.1 200 OK\r\n"), "{ok}");
        assert!(ok.contains(r#""member":"alpha""#), "{ok}");
        assert_eq!(hook_busy("sid-a"), Some(true));
        let refused = request("Bearer wrong", r#"{"event":"turn.complete"}"#);
        assert!(refused.starts_with("HTTP/1.1 401 "), "{refused}");
        assert_eq!(hook_busy("sid-a"), Some(true));

        let port = endpoint.port;
        endpoint.close();
        assert!(!path.exists(), "the description goes with the endpoint");
        assert!(TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).is_err());
    }

    #[test]
    fn test_hook_endpoint_close_keeps_a_newer_generations_description() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_str().unwrap().to_string();
        let endpoint = open_endpoint_with(ctx_with(roster(), &workspace), "gen-1").unwrap();
        let path = hooks_endpoint_path(&workspace);
        fs::write(&path, r#"{"token":"someone-else"}"#).unwrap();
        endpoint.close();
        assert!(path.exists());
    }
}
