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
//! plugin finds the workspace through the roster row naming its session
//! and reads this file; it decides no membership.
//!
//! A request is admitted when its bearer token is this generation's, its
//! `teamCreatedAt` is this instance's and its `sessionId` is a claude row
//! of this team's roster, read per request from an entry that names the
//! same instance (the CLI owns membership; the endpoint only observes).
//! Anything else is refused with a status and touches no observation.
//!
//! Events are ordered by the module, not by arrival: each carries the
//! module instance's `epoch` and a per-instance `seq`. The store takes an
//! event only when it is newer than the last it took for that session;
//! a late `turn.start` whose `turn.complete` already landed is stale and
//! changes nothing. An epoch is one engine process (a wake, a claim of a
//! pre-booted spare starts another) and registers with `session.start`,
//! which becomes the session's current epoch and retires the one before
//! it; a retired epoch's late `session.start` is stale, so the current
//! epoch cannot be rolled back (the store keeps the last eight retired
//! epochs of a session; an older one is forgotten). A turn event from an
//! epoch the store does not know is answered `unregistered`, and the
//! module registers (a `session.start` at `seq` 0, which advances no
//! watermark) and sends the event again as it was, same `seq`, same
//! `eventId`: a resend can never outrun what landed meanwhile. That is
//! also how the lane recovers after a lost `session.start` or a hived
//! generation that started empty.
//!
//! What the observations answer: [`hook_busy`], whether the engine is
//! inside a turn, while the channel is fresh. Claude Code skips a hook
//! that throws, overruns or answers the wrong shape and carries on
//! (fail-open), so a `turn.complete` can be lost; the answer therefore
//! expires [`HOOK_FRESH_SECONDS`] after the last event and the registry
//! status (`busy.rs::claude_registry_busy`) stands again. A subagent's
//! turn (`agentId` set) is acknowledged and not counted: the main loop's
//! busy is what the display shows.
//!
//! The endpoint is closed explicitly ([`close_endpoint`]); the accept
//! worker holds it alive, so dropping the handle alone stops nothing.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
/// One request's whole budget from its first byte: the read shares it
/// with the reply, which keeps at least 200ms of it.
const REQUEST_BUDGET: Duration = Duration::from_secs(2);
const MAX_HEAD_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;
/// Requests served at once; the rest are refused at accept with a 503.
const MAX_INFLIGHT: usize = 8;
const RECENT_EVENT_IDS: usize = 512;
const ACCEPT_POLL_SECONDS: f64 = 0.1;
const EVENTS: [&str; 3] = ["session.start", "turn.start", "turn.complete"];

/// Who may post: the roster's claude row for a session id, by name, read
/// from an entry that names this endpoint's instance.
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

/// One session's last accepted report.
#[derive(Clone, Debug)]
pub(crate) struct HookObservation {
    pub last_event: String,
    pub turn_id: Option<String>,
    pub seen: Instant,
    /// The module instance that reported, and its counter.
    pub epoch: String,
    pub seq: u64,
    /// Epochs this session had before, newest last; a late `session.start`
    /// from one of them is stale.
    pub retired: Vec<String>,
}

/// Retired epochs kept per session.
const RETIRED_EPOCHS: usize = 8;

#[derive(Default)]
struct Store {
    by_session: HashMap<String, HookObservation>,
    recent_event_ids: VecDeque<String>,
    /// Set under the store's lock by `close`, read under it by every
    /// admission: no report is taken after the endpoint closed.
    closed: bool,
}

fn store() -> &'static Mutex<Store> {
    static CELL: OnceLock<Mutex<Store>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(Store::default()))
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
    let store = store().lock().unwrap_or_else(|e| e.into_inner());
    let seen = store.by_session.get(session_id)?;
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

/// A bearer token from the system's random source; without one there is
/// no endpoint, never a guessable substitute.
fn mint_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| anyhow!("no random source for the hooks token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

// --------------------------------------------------------------------------
// listener
// --------------------------------------------------------------------------

pub(crate) struct HooksEndpoint {
    listener: Mutex<Option<TcpListener>>,
    closed: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
    inflight: Arc<AtomicUsize>,
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
    let instance = created.to_string();
    let roster: Box<RosterLookup> =
        Box::new(move |sid| roster_claude_member(&team_name, &instance, sid));
    let ctx = HookContext {
        token: mint_token()?,
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
    mark_closed(false);
    let endpoint = Arc::new(HooksEndpoint {
        listener: Mutex::new(Some(listener)),
        closed: Arc::new(AtomicBool::new(false)),
        worker: Mutex::new(None),
        inflight: Arc::new(AtomicUsize::new(0)),
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
/// name — from an entry that names *instance* (a recycled team name is
/// another instance, and its roster vouches for nobody here). A joined
/// session's row carries the session id itself; a bg member's row carries
/// its jobId, and the engine registry says which session id that job
/// minted.
fn roster_claude_member(team: &str, instance: &str, session_id: &str) -> Option<String> {
    let entry = crate::registry::load(team)?;
    if !instance.is_empty() && TeamInstance::from_entry(&entry).created != instance {
        return None;
    }
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

/// One served request's slot among [`MAX_INFLIGHT`]; released on drop.
struct InflightSlot(Arc<AtomicUsize>);

impl InflightSlot {
    fn take(counter: &Arc<AtomicUsize>) -> Option<InflightSlot> {
        let mut seen = counter.load(Ordering::SeqCst);
        loop {
            if seen >= MAX_INFLIGHT {
                return None;
            }
            match counter.compare_exchange(seen, seen + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(InflightSlot(counter.clone())),
                Err(now) => seen = now,
            }
        }
    }
}

impl Drop for InflightSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
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
            let Some((mut stream, _)) = accepted else {
                continue;
            };
            let Some(slot) = InflightSlot::take(&self.inflight) else {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_write_timeout(Some(REQUEST_BUDGET));
                write_response(&mut stream, 503, &refusal("too many requests in flight"));
                continue;
            };
            let ctx = self.ctx.clone();
            let _ = thread::Builder::new()
                .name("hived-hooks-request".to_string())
                .spawn(move || {
                    let _slot = slot;
                    serve_connection(stream, &ctx)
                });
        }
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        mark_closed(true);
        *self.listener.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let worker = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
        remove_endpoint_file_if(&self.ctx.workspace, &self.ctx.token);
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

/// One read within what is left of *deadline*; None at the deadline, on a
/// closed peer, or on an error.
fn read_some(stream: &mut TcpStream, buf: &mut Vec<u8>, deadline: Instant) -> Option<usize> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return None;
    }
    stream.set_read_timeout(Some(remaining)).ok()?;
    let mut chunk = [0u8; 4096];
    let n = stream.read(&mut chunk).ok()?;
    if n == 0 {
        return None;
    }
    buf.extend_from_slice(&chunk[..n]);
    Some(n)
}

/// The request line, the two headers this route reads and the body, all
/// within one budget from the first byte; a head over its cap in one read
/// or across several, or a body over its cap, is refused.
fn read_request(stream: &mut TcpStream, deadline: Instant) -> Option<HttpRequest> {
    let mut buf: Vec<u8> = Vec::new();
    let head_end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            if at + 4 > MAX_HEAD_BYTES {
                return None;
            }
            break at + 4;
        }
        if buf.len() >= MAX_HEAD_BYTES {
            return None;
        }
        read_some(stream, &mut buf, deadline)?;
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
        read_some(stream, &mut body, deadline)?;
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
        503 => "Service Unavailable",
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
    let deadline = Instant::now() + REQUEST_BUDGET;
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(REQUEST_BUDGET));
    let Some(request) = read_request(&mut stream, deadline) else {
        write_response(&mut stream, 400, &refusal("unreadable request"));
        let _ = stream.shutdown(std::net::Shutdown::Both);
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
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(200));
    let _ = stream.set_write_timeout(Some(remaining));
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
    let turn_id = map_get_str(&event, "turnId");
    if name != "session.start" && turn_id.is_empty() {
        return (400, refusal("a turn event names its turnId"));
    }
    let epoch = map_get_str(&event, "epoch");
    let seq = event.get("seq").and_then(Value::as_u64);
    let (Some(seq), false) = (seq, epoch.is_empty()) else {
        return (400, refusal("an event carries its epoch and seq"));
    };
    let mut answer = Map::new();
    answer.insert("ok".to_string(), Value::Bool(true));
    answer.insert("member".to_string(), Value::from(member.clone()));
    let agent_id = map_get_str(&event, "agentId");
    if !agent_id.is_empty() {
        answer.insert("ignored".to_string(), Value::from("subagent"));
        return (200, answer);
    }
    let event_id = map_get_str(&event, "eventId");
    match apply_event(&session_id, &name, &turn_id, &epoch, seq, &event_id) {
        Applied::Taken => {}
        Applied::Duplicate => {
            answer.insert("duplicate".to_string(), Value::Bool(true));
            return (200, answer);
        }
        Applied::Stale => {
            answer.insert("stale".to_string(), Value::Bool(true));
            return (200, answer);
        }
        Applied::Unregistered => {
            answer.insert("unregistered".to_string(), Value::Bool(true));
            return (200, answer);
        }
        Applied::Closed => return (503, refusal("endpoint closing")),
    }
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

#[derive(Debug, PartialEq, Eq)]
enum Applied {
    Taken,
    Duplicate,
    Stale,
    /// A turn event from an epoch the session has not registered: the
    /// module registers with `session.start` and sends it again.
    Unregistered,
    Closed,
}

/// Set the store's closed flag under its lock: linearized with every
/// admission, so no report is taken once `close` has passed this point.
fn mark_closed(closed: bool) {
    store().lock().unwrap_or_else(|e| e.into_inner()).closed = closed;
}

/// The turn state machine, under one lock with the replay window.
///
/// Order is the module's (`epoch`, `seq`), not arrival: an event at or
/// behind the last one taken for the session is stale. An epoch registers
/// through its `session.start`, which opens no turn and retires the epoch
/// before it; a retired epoch is stale for good, and a turn event from an
/// epoch not yet registered is answered `Unregistered` so the module
/// registers first. Within an epoch a `session.start` leaves an open turn
/// alone, a `turn.start` opens the turn it names, and a `turn.complete`
/// closes that turn (one naming another turn leaves the open one alone).
fn apply_event(
    session_id: &str,
    event: &str,
    turn_id: &str,
    epoch: &str,
    seq: u64,
    event_id: &str,
) -> Applied {
    let mut store = store().lock().unwrap_or_else(|e| e.into_inner());
    if store.closed {
        return Applied::Closed;
    }
    // Only a taken event enters the replay window: one answered
    // `Unregistered` comes back as it was once the epoch registered.
    if !event_id.is_empty() && store.recent_event_ids.iter().any(|id| id == event_id) {
        return Applied::Duplicate;
    }
    let previous = store.by_session.get(session_id);
    let (open, retired) = match previous {
        Some(seen) if seen.epoch == epoch => {
            if seq <= seen.seq {
                return Applied::Stale;
            }
            (seen.turn_id.clone(), seen.retired.clone())
        }
        Some(seen) if seen.retired.iter().any(|e| e == epoch) => return Applied::Stale,
        Some(_) if event != "session.start" => return Applied::Unregistered,
        Some(seen) => {
            let mut retired = seen.retired.clone();
            retired.push(seen.epoch.clone());
            while retired.len() > RETIRED_EPOCHS {
                retired.remove(0);
            }
            (None, retired)
        }
        None => (None, Vec::new()),
    };
    let turn = match event {
        "turn.start" => Some(turn_id.to_string()),
        "turn.complete" => match open {
            Some(open) if open != turn_id => Some(open),
            _ => None,
        },
        _ => open,
    };
    store.by_session.insert(
        session_id.to_string(),
        HookObservation {
            last_event: event.to_string(),
            turn_id: turn,
            seen: Instant::now(),
            epoch: epoch.to_string(),
            seq,
            retired,
        },
    );
    if !event_id.is_empty() {
        store.recent_event_ids.push_back(event_id.to_string());
        while store.recent_event_ids.len() > RECENT_EVENT_IDS {
            store.recent_event_ids.pop_front();
        }
    }
    Applied::Taken
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

    /// A body for `sid-a` in epoch `e1` at *seq*, plus *fields*.
    fn body_at(seq: u64, fields: &[(&str, &str)]) -> Vec<u8> {
        let mut map = Map::new();
        map.insert("teamCreatedAt".to_string(), Value::from("1700000000"));
        map.insert("sessionId".to_string(), Value::from("sid-a"));
        map.insert("epoch".to_string(), Value::from("e1"));
        map.insert("seq".to_string(), Value::from(seq));
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
        let ok = body_at(1, &[("event", "turn.start"), ("turnId", "t1")]);
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
        let mut other: Map<String, Value> = serde_json::from_slice(&ok).unwrap();
        other.insert("teamCreatedAt".to_string(), Value::from("1600000000"));
        assert_eq!(post(&ctx, &serde_json::to_vec(&other).unwrap()).0, 409);
        let mut stranger: Map<String, Value> = serde_json::from_slice(&ok).unwrap();
        stranger.insert("sessionId".to_string(), Value::from("sid-z"));
        assert_eq!(post(&ctx, &serde_json::to_vec(&stranger).unwrap()).0, 404);
        assert_eq!(post(&ctx, &body_at(1, &[("event", "tool.call")])).0, 400);
        assert_eq!(
            post(&ctx, &body_at(1, &[("event", "turn.start")])).0,
            400,
            "no turnId"
        );
        assert_eq!(
            post(&ctx, &body_at(1, &[("event", "turn.complete")])).0,
            400
        );
        assert_eq!(post(&ctx, b"not json").0, 400);
        let mut unordered: Map<String, Value> = serde_json::from_slice(&ok).unwrap();
        unordered.remove("seq");
        assert_eq!(
            post(&ctx, &serde_json::to_vec(&unordered).unwrap()).0,
            400,
            "no seq"
        );
        unordered.insert("seq".to_string(), Value::from(1));
        unordered.remove("epoch");
        assert_eq!(
            post(&ctx, &serde_json::to_vec(&unordered).unwrap()).0,
            400,
            "no epoch"
        );
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
            &body_at(1, &[("event", "session.start"), ("surface", "terminal")]),
        );
        assert_eq!(status, 200);
        assert_eq!(answer.get("member"), Some(&Value::from("alpha")));
        assert_eq!(hook_busy("sid-a"), Some(false));
        post(
            &ctx,
            &body_at(2, &[("event", "turn.start"), ("turnId", "t1")]),
        );
        assert_eq!(hook_busy("sid-a"), Some(true));
        // a session.start in the same epoch (the claim of a pre-booted
        // spare) leaves the open turn alone
        post(&ctx, &body_at(3, &[("event", "session.start")]));
        assert_eq!(hook_busy("sid-a"), Some(true));
        // a complete naming another turn leaves the open one alone
        post(
            &ctx,
            &body_at(4, &[("event", "turn.complete"), ("turnId", "t0")]),
        );
        assert_eq!(hook_busy("sid-a"), Some(true));
        post(
            &ctx,
            &body_at(
                5,
                &[
                    ("event", "turn.complete"),
                    ("turnId", "t1"),
                    ("reason", "answer"),
                ],
            ),
        );
        assert_eq!(hook_busy("sid-a"), Some(false));
        assert_eq!(
            fresh_observation("sid-a").unwrap().last_event,
            "turn.complete"
        );
    }

    #[test]
    fn test_hook_events_are_ordered_by_the_modules_sequence_not_arrival() {
        let ctx = ctx_with(roster(), "");
        // the complete lands first (the start's POST ran past its budget
        // and arrives late): the late start is stale, the turn stays closed
        post(
            &ctx,
            &body_at(2, &[("event", "turn.complete"), ("turnId", "t1")]),
        );
        assert_eq!(hook_busy("sid-a"), Some(false));
        let (status, answer) = post(
            &ctx,
            &body_at(1, &[("event", "turn.start"), ("turnId", "t1")]),
        );
        assert_eq!(status, 200);
        assert_eq!(answer.get("stale"), Some(&Value::Bool(true)));
        assert_eq!(hook_busy("sid-a"), Some(false));
        // a late session.start of the same epoch is stale too
        post(
            &ctx,
            &body_at(3, &[("event", "turn.start"), ("turnId", "t2")]),
        );
        let (_, answer) = post(&ctx, &body_at(0, &[("event", "session.start")]));
        assert_eq!(answer.get("stale"), Some(&Value::Bool(true)));
        assert_eq!(hook_busy("sid-a"), Some(true));
        // a new epoch (a woken engine) enters through its session.start and
        // opens nothing; its own turn events then count
        let mut fresh: Map<String, Value> =
            serde_json::from_slice(&body_at(1, &[("event", "turn.start"), ("turnId", "t9")]))
                .unwrap();
        fresh.insert("epoch".to_string(), Value::from("e2"));
        let (_, answer) = post(&ctx, &serde_json::to_vec(&fresh).unwrap());
        assert_eq!(
            answer.get("unregistered"),
            Some(&Value::Bool(true)),
            "a new epoch registers with session.start first"
        );
        assert_eq!(hook_busy("sid-a"), Some(true));
        fresh.insert("event".to_string(), Value::from("session.start"));
        fresh.insert("seq".to_string(), Value::from(0));
        post(&ctx, &serde_json::to_vec(&fresh).unwrap());
        assert_eq!(
            hook_busy("sid-a"),
            Some(false),
            "the old epoch's turn does not survive a new engine"
        );
        // the old epoch's late events are stale from now on
        let (_, answer) = post(
            &ctx,
            &body_at(4, &[("event", "turn.start"), ("turnId", "t3")]),
        );
        assert_eq!(answer.get("stale"), Some(&Value::Bool(true)));
        assert_eq!(hook_busy("sid-a"), Some(false));
    }

    #[test]
    fn test_hook_subagent_turns_and_duplicates_are_acknowledged_not_counted() {
        let ctx = ctx_with(roster(), "");
        post(&ctx, &body_at(1, &[("event", "session.start")]));
        let (status, answer) = post(
            &ctx,
            &body_at(
                2,
                &[
                    ("event", "turn.start"),
                    ("turnId", "sub-1"),
                    ("agentId", "agent-9"),
                ],
            ),
        );
        assert_eq!(status, 200);
        assert_eq!(answer.get("ignored"), Some(&Value::from("subagent")));
        assert_eq!(hook_busy("sid-a"), Some(false));
        let first = post(
            &ctx,
            &body_at(
                3,
                &[("event", "turn.start"), ("turnId", "t1"), ("eventId", "e1")],
            ),
        );
        assert!(first.1.get("duplicate").is_none());
        post(
            &ctx,
            &body_at(
                4,
                &[
                    ("event", "turn.complete"),
                    ("turnId", "t1"),
                    ("eventId", "e2"),
                ],
            ),
        );
        let again = post(
            &ctx,
            &body_at(
                3,
                &[("event", "turn.start"), ("turnId", "t1"), ("eventId", "e1")],
            ),
        );
        assert_eq!(again.1.get("duplicate"), Some(&Value::Bool(true)));
        assert_eq!(
            hook_busy("sid-a"),
            Some(false),
            "a replayed start does not reopen the turn"
        );
        // past the replay window the same replay is still stale by order
        for n in 0..(RECENT_EVENT_IDS as u64 + 1) {
            post(
                &ctx,
                &body_at(
                    100 + n,
                    &[
                        ("event", "session.start"),
                        ("eventId", &format!("fill-{n}")),
                    ],
                ),
            );
        }
        let old = post(
            &ctx,
            &body_at(
                3,
                &[("event", "turn.start"), ("turnId", "t1"), ("eventId", "e1")],
            ),
        );
        assert_eq!(old.1.get("stale"), Some(&Value::Bool(true)));
        assert_eq!(hook_busy("sid-a"), Some(false));
    }

    #[test]
    fn test_hook_busy_expires_after_the_freshness_window() {
        let ctx = ctx_with(roster(), "");
        post(
            &ctx,
            &body_at(1, &[("event", "turn.start"), ("turnId", "t1")]),
        );
        let now = Instant::now();
        assert_eq!(hook_busy_at("sid-a", now), Some(true));
        let later = now + Duration::from_secs_f64(HOOK_FRESH_SECONDS + 1.0);
        assert_eq!(hook_busy_at("sid-a", later), None);
        assert_eq!(hook_busy_at("", now), None);
    }

    #[test]
    fn test_hook_request_after_close_writes_nothing() {
        let ctx = ctx_with(roster(), "");
        mark_closed(true);
        let (status, _) = post(
            &ctx,
            &body_at(1, &[("event", "turn.start"), ("turnId", "t1")]),
        );
        assert_eq!(status, 503);
        assert!(hook_busy("sid-a").is_none());
        // the flag lives under the store's lock: a request that reached
        // the lock first is taken before close, one after it is refused,
        // and nothing is written in between
        let held = store().lock().unwrap();
        let closer = thread::spawn(|| mark_closed(false));
        thread::sleep(Duration::from_millis(50));
        assert!(!closer.is_finished(), "close waits for the store's lock");
        drop(held);
        closer.join().unwrap();
        let (status, _) = post(
            &ctx,
            &body_at(2, &[("event", "turn.start"), ("turnId", "t1")]),
        );
        assert_eq!(status, 200);
        assert_eq!(hook_busy("sid-a"), Some(true));
    }

    #[test]
    fn test_hook_epochs_register_retire_and_recover() {
        let ctx = ctx_with(roster(), "");
        let at = |epoch: &str, seq: u64, fields: &[(&str, &str)]| {
            let mut map: Map<String, Value> =
                serde_json::from_slice(&body_at(seq, fields)).unwrap();
            map.insert("epoch".to_string(), Value::from(epoch));
            serde_json::to_vec(&map).unwrap()
        };
        // e1 registers and opens a turn
        post(&ctx, &at("e1", 1, &[("event", "session.start")]));
        post(
            &ctx,
            &at("e1", 2, &[("event", "turn.start"), ("turnId", "a")]),
        );
        assert_eq!(hook_busy("sid-a"), Some(true));
        // e2 (a woken engine) registers: e1 retires, the turn is gone
        post(&ctx, &at("e2", 1, &[("event", "session.start")]));
        assert_eq!(hook_busy("sid-a"), Some(false));
        post(
            &ctx,
            &at("e2", 2, &[("event", "turn.start"), ("turnId", "b")]),
        );
        assert_eq!(hook_busy("sid-a"), Some(true));
        // e1's late first session.start is stale: no rollback
        let (_, answer) = post(&ctx, &at("e1", 1, &[("event", "session.start")]));
        assert_eq!(answer.get("stale"), Some(&Value::Bool(true)));
        assert_eq!(hook_busy("sid-a"), Some(true));
        let (_, answer) = post(
            &ctx,
            &at("e2", 3, &[("event", "turn.complete"), ("turnId", "b")]),
        );
        assert!(
            answer.get("stale").is_none(),
            "the current epoch keeps reporting"
        );
        assert_eq!(hook_busy("sid-a"), Some(false));
        // e3's session.start was lost: its turn event is unregistered, not
        // stale, and its registration then admits the resend
        let (_, answer) = post(
            &ctx,
            &at("e3", 2, &[("event", "turn.start"), ("turnId", "c")]),
        );
        assert_eq!(answer.get("unregistered"), Some(&Value::Bool(true)));
        assert_eq!(hook_busy("sid-a"), Some(false));
        post(&ctx, &at("e3", 3, &[("event", "session.start")]));
        let (_, answer) = post(
            &ctx,
            &at("e3", 4, &[("event", "turn.start"), ("turnId", "c")]),
        );
        assert!(answer.get("unregistered").is_none());
        assert_eq!(hook_busy("sid-a"), Some(true));
        // e2 is retired now too
        let (_, answer) = post(&ctx, &at("e2", 9, &[("event", "session.start")]));
        assert_eq!(answer.get("stale"), Some(&Value::Bool(true)));
        assert_eq!(hook_busy("sid-a"), Some(true));
    }

    /// The module's recovery, request by request, as it plays out when the
    /// start's `unregistered` answer is held past the complete's recovery:
    /// the registration is a `session.start` at seq 0 and each resend keeps
    /// its own seq and eventId, so the late start ends up stale.
    #[test]
    fn test_hook_recovery_resends_keep_their_order_and_a_late_start_stays_stale() {
        let ctx = ctx_with(roster(), "");
        let at = |epoch: &str, seq: u64, fields: &[(&str, &str)]| {
            let mut map: Map<String, Value> =
                serde_json::from_slice(&body_at(seq, fields)).unwrap();
            map.insert("epoch".to_string(), Value::from(epoch));
            serde_json::to_vec(&map).unwrap()
        };
        post(&ctx, &at("e1", 1, &[("event", "session.start")]));
        let start = at(
            "e2",
            1,
            &[("event", "turn.start"), ("turnId", "t1"), ("eventId", "s1")],
        );
        let complete = at(
            "e2",
            2,
            &[
                ("event", "turn.complete"),
                ("turnId", "t1"),
                ("eventId", "c1"),
            ],
        );
        let register = at("e2", 0, &[("event", "session.start")]);
        // e2's session.start was lost: both turn events are unregistered
        assert_eq!(
            post(&ctx, &start).1.get("unregistered"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            post(&ctx, &complete).1.get("unregistered"),
            Some(&Value::Bool(true))
        );
        // the complete's report recovers first: register, resend as it was
        assert!(post(&ctx, &register).1.get("stale").is_none());
        let again = post(&ctx, &complete).1;
        assert!(
            again.get("duplicate").is_none() && again.get("stale").is_none(),
            "{again:?}"
        );
        assert_eq!(hook_busy("sid-a"), Some(false));
        // the start's report recovers later: its registration and its
        // resend are both behind the complete, the turn stays closed
        assert_eq!(
            post(&ctx, &register).1.get("stale"),
            Some(&Value::Bool(true))
        );
        assert_eq!(post(&ctx, &start).1.get("stale"), Some(&Value::Bool(true)));
        assert_eq!(hook_busy("sid-a"), Some(false));
        // a taken event is in the replay window; a replay is a duplicate
        assert_eq!(
            post(&ctx, &complete).1.get("duplicate"),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn test_inflight_slots_are_bounded_and_released() {
        let counter = Arc::new(AtomicUsize::new(0));
        let held: Vec<InflightSlot> = (0..MAX_INFLIGHT)
            .map(|_| InflightSlot::take(&counter).expect("a free slot"))
            .collect();
        assert!(
            InflightSlot::take(&counter).is_none(),
            "the cap refuses the next"
        );
        drop(held);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert!(InflightSlot::take(&counter).is_some());
    }

    fn connect(port: u16) -> TcpStream {
        let mut conn = TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let _ = &mut conn;
        conn
    }

    fn request_text(auth: &str, json: &str) -> String {
        format!(
            "POST {HOOKS_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: {auth}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{json}",
            json.len()
        )
    }

    fn read_all(conn: &mut TcpStream) -> String {
        let mut out = String::new();
        let _ = conn.read_to_string(&mut out);
        out
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

        let token = format!("Bearer {}", endpoint.token());
        let mut conn = connect(endpoint.port);
        conn.write_all(
            request_text(
                &token,
                &String::from_utf8(body_at(1, &[("event", "turn.start"), ("turnId", "t1")]))
                    .unwrap(),
            )
            .as_bytes(),
        )
        .unwrap();
        let ok = read_all(&mut conn);
        assert!(ok.starts_with("HTTP/1.1 200 OK\r\n"), "{ok}");
        assert!(ok.contains(r#""member":"alpha""#), "{ok}");
        assert_eq!(hook_busy("sid-a"), Some(true));
        let mut conn = connect(endpoint.port);
        conn.write_all(request_text("Bearer wrong", r#"{"event":"turn.complete"}"#).as_bytes())
            .unwrap();
        let refused = read_all(&mut conn);
        assert!(refused.starts_with("HTTP/1.1 401 "), "{refused}");
        assert_eq!(hook_busy("sid-a"), Some(true));

        let port = endpoint.port;
        endpoint.close();
        assert!(!path.exists(), "the description goes with the endpoint");
        assert!(TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).is_err());
    }

    #[test]
    fn test_hook_endpoint_refuses_a_slow_drip_and_an_oversized_head() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_str().unwrap().to_string();
        let endpoint = open_endpoint_with(ctx_with(roster(), &workspace), "gen").unwrap();
        let token = format!("Bearer {}", endpoint.token());

        // a byte at a time, each inside the per-read timeout, past the
        // request's whole budget: cut off, nothing recorded
        let full = request_text(
            &token,
            &String::from_utf8(body_at(1, &[("event", "turn.start"), ("turnId", "drip")])).unwrap(),
        );
        let mut conn = connect(endpoint.port);
        let started = Instant::now();
        let mut cut = false;
        for byte in full.as_bytes() {
            if conn.write_all(&[*byte]).is_err() {
                cut = true;
                break;
            }
            thread::sleep(Duration::from_millis(60));
            if started.elapsed() > REQUEST_BUDGET + Duration::from_millis(600) {
                break;
            }
        }
        let answer = read_all(&mut conn);
        assert!(
            cut || answer.starts_with("HTTP/1.1 400 ") || answer.is_empty(),
            "{answer}"
        );
        assert!(started.elapsed() < Duration::from_secs(6));
        assert!(
            hook_busy("sid-a").is_none(),
            "a dripped request records nothing"
        );

        // a head over its cap, split across two writes
        let mut conn = connect(endpoint.port);
        let padding = "X".repeat(MAX_HEAD_BYTES);
        let head = format!("POST {HOOKS_PATH} HTTP/1.1\r\nAuthorization: {token}\r\nX-Pad: {padding}\r\nContent-Length: 0\r\n\r\n");
        let (first, second) = head.as_bytes().split_at(5000);
        conn.write_all(first).unwrap();
        thread::sleep(Duration::from_millis(100));
        let _ = conn.write_all(second);
        let answer = read_all(&mut conn);
        assert!(
            answer.starts_with("HTTP/1.1 400 ") || answer.is_empty(),
            "{answer}"
        );
        assert!(hook_busy("sid-a").is_none());
        endpoint.close();
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
