//! hive::node — one task on one live member, as one blocking call.
//!
//! A node is one task placed on one live member, the way a Claude Code
//! Workflow subagent takes one: spawn a real pane (or reuse a live member
//! of that name), wait until it is ready and between turns, dispatch the
//! task as a `<HIVE>` envelope with no sender, and read the member's own
//! turn. The member is never asked to send anything back — the final
//! assistant message of the turn its task started is the node's result,
//! read from the engine's transcript through `adapters::turn`.
//!
//! The anchor is input identity, never time: the runner mints a dispatch
//! id, puts it verbatim in the text the member receives (the envelope body
//! and the task artifact name), takes the transcript cursor before
//! dispatching, and asks the reader to bind the turn that consumed that
//! exact input past that cursor. A turn the reader cannot attribute is
//! reported (`ambiguous`), never guessed at.
//!
//! One run is one record, `<workspace>/run/nodes/<name>.json`, written
//! pending before the dispatch has any side effect and again at every
//! transition (input_bound → terminal), held under the per-member flock
//! `<name>.lock`; a pending record of a live member is another runner's, a
//! pending record of a dead member is stale and replaced. Past the
//! dispatch nothing is an `Err` any more: exit 1 means "not dispatched",
//! so a record write that fails later is logged and the run still ends in
//! a verdict. A dispatch the hived refused is not dispatched; one whose
//! answer was lost may be, is never repeated, and keeps its pending
//! record until the transcript shows the task or the member is killed.
//! `NodeOp` is the typed vocabulary of one hive interaction,
//! `run_op` executes one, and `run_node` (`hive node run`) strings them
//! together for an external orchestrator. `NodeEnv` is the seam over
//! cli/bus/team/readers; tests inject a fake.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::adapters::turn::{InputBinding, ReadError, TurnAnchor, TurnOutcome, TurnReader};
use crate::send::DispatchFailure;

const POLL_SECONDS: f64 = 1.0;
/// How long the reader may keep failing (`ReadError`) before the node
/// gives up on the transcript.
const READ_ERROR_BUDGET_SECONDS: f64 = 60.0;
/// How long a ready member's roster row may lack a session id: the id is
/// backfilled by the hived, not written by the spawn.
const SESSION_ID_WAIT_SECONDS: f64 = 30.0;
/// How long a member spawned by this run may go unseen with a turn open:
/// its bootstrap turn is what the idle wait has to outlast, and a fast one
/// can close between two polls, so a spawn never seen in a turn is taken as
/// past it.
const BUSY_SIGHTING_SECONDS: f64 = 60.0;
/// How long a dispatch waits for the member to be between turns; past it
/// the run ends `member_busy` without dispatching.
const IDLE_WAIT_SECONDS: f64 = 600.0;
/// How long a closed turn may keep its final message off disk
/// (`TurnOutcome::Flushing`), counted from the first such reading; past it
/// the run ends `ambiguous`.
const FLUSH_BUDGET_SECONDS: f64 = 30.0;
/// How long a dispatch whose answer was lost (`DispatchFailure::Unknown`)
/// may go unseen in the transcript, counted in polls from the dispatch;
/// past it the run ends `ambiguous` with the record left pending, since
/// the member may be on the task.
const DISPATCH_UNKNOWN_SECONDS: f64 = 120.0;
const ATTEMPTS: usize = 3;
const RETRY_GAP: f64 = 3.0;

pub const STATUS_PENDING: &str = "pending";
pub const STATUS_INPUT_BOUND: &str = "input_bound";
pub const STATUS_COMPLETED: &str = "completed";
pub const STATUS_INTERRUPTED: &str = "interrupted";
pub const STATUS_FAILED: &str = "failed";
pub const STATUS_AMBIGUOUS: &str = "ambiguous";
pub const STATUS_SESSION_CHANGED: &str = "session_changed";
pub const STATUS_TRANSCRIPT_UNAVAILABLE: &str = "transcript_unavailable";
pub const STATUS_MEMBER_GONE: &str = "member_gone";
pub const STATUS_MEMBER_BUSY: &str = "member_busy";

// tmux splits and team registration race each other in-process; spawns
// serialize, everything else stays parallel. (Cross-process, the registry
// name claim inside Team::spawn is the guard.)
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// The node could not reach a dispatch: bad team, spawn, ready gate, no
/// reader for the member's CLI, or the dispatch itself refused by the
/// hived. Everything after the dispatch — including one whose answer was
/// lost — is a verdict in the result, never an error.
#[derive(Debug)]
pub struct NodeError(pub String);

impl fmt::Display for NodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NodeError {}

#[derive(Debug, Clone)]
pub struct Ctx {
    pub team_name: String,
    pub workspace: String,
}

#[derive(Debug, Clone)]
pub struct SpawnedAgent {
    pub pane_id: String,
    pub cli: String,
}

/// A roster row as the node needs it: where the member sits, which engine
/// it runs, and the engine's own session id and cwd the reader resolves
/// the transcript through.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberInfo {
    pub pane_id: String,
    pub cli: String,
    pub session_id: Option<String>,
    pub cwd: String,
}

/// The seams a node reaches through. `Err(String)` from `spawn` and
/// `DispatchFailure::Refused` from `dispatch` are transient failures the
/// retry loops absorb; `DispatchFailure::Unknown` is never retried.
pub trait NodeEnv: Send + Sync {
    fn context(&self) -> Result<Ctx, NodeError>;
    fn spawn(&self, name: &str, cli: Option<&str>, model: &str) -> Result<SpawnedAgent, String>;
    /// `Err` is a hived this hive must not touch (`hived::ensure_hived`).
    fn ensure_hived(&self) -> Result<(), String>;
    /// Agents still not ready when the gate expires.
    fn wait_ready(&self, agents: &HashSet<String>) -> HashSet<String>;
    /// Dispatch the task with no sender; returns the ledger seq of the row.
    /// `Refused` means the task is not with the member; `Unknown` means
    /// the hived gave no usable answer and the task may have landed.
    fn dispatch(&self, target: &str, body: &str, artifact: &str) -> Result<i64, DispatchFailure>;
    /// Runtime liveness (`Team::member_alive`): can this member still take a
    /// dispatch and run a turn.
    fn alive(&self, name: &str) -> bool;
    /// A fresh read of the member's roster row; None when it is not on
    /// the roster.
    fn member(&self, name: &str) -> Option<MemberInfo>;
    /// Whether a turn is open on the member right now, asked of its
    /// engine directly by the hived; None when the engine could not be
    /// asked. Never a guess: an unreachable engine is not an idle member.
    fn turn_open(&self, name: &str) -> Option<bool>;
    /// `Team::retire`: no-op when the member is not on the roster.
    fn retire(&self, name: &str);
    /// The transcript reader for a roster `cli` value; None when hive has
    /// no reader for it.
    fn reader(&self, cli: &str) -> Option<Box<dyn TurnReader>>;
    fn sleep(&self, seconds: f64);
}

/// Progress goes to stderr so stdout carries only the result (the JSON
/// line of `hive node run`).
fn log(message: &str) {
    eprintln!("[node] {message}");
    let _ = std::io::stderr().flush();
}

fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A fresh dispatch id, `nd-<12 lowercase hex>`: unique per process and
/// call (time, pid and a counter hashed), never derived from the task.
pub fn mint_dispatch_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    COUNTER.fetch_add(1, Ordering::SeqCst).hash(&mut hasher);
    format!("nd-{:012x}", hasher.finish() & 0xffff_ffff_ffff)
}

// ---------------------------------------------------------------------------
// node record and lock
// ---------------------------------------------------------------------------

/// One run's persisted state, `<workspace>/run/nodes/<name>.json`.
///
/// The name is owned while the record is pending (`is_pending`) and free
/// under any terminal status. That is a v1 narrowing: a terminal verdict
/// says the runner stopped waiting, not that the member stopped working —
/// after `ambiguous`, `transcript_unavailable` or `session_changed` the
/// member may still be on the task, and the next run of that name is
/// allowed to dispatch on top of it. Keeping the occupation open past the
/// verdict needs a resolution step (a kill, or a read that proves the turn
/// ended) that `hive node run` does not have yet. The one verdict that
/// leaves the record pending is a dispatch whose answer was lost and whose
/// input never showed (`await_turn`): there the run ends `ambiguous` with
/// the name still owned, since the member may be on the task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRecord {
    pub dispatch_id: String,
    pub cli: String,
    pub session: String,
    pub cursor: String,
    pub anchor: Option<TurnAnchor>,
    pub status: String,
    pub body: Option<String>,
    pub reason: Option<String>,
    /// Ledger seq of the dispatch row; None when the run ended before it.
    pub seq: Option<i64>,
    pub started_at: u64,
}

impl NodeRecord {
    /// A run that has not reached a terminal status: the member is owned
    /// by the runner that wrote it.
    pub fn is_pending(&self) -> bool {
        self.status == STATUS_PENDING || self.status == STATUS_INPUT_BOUND
    }

    fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("dispatchId".into(), Value::from(self.dispatch_id.as_str()));
        map.insert("cli".into(), Value::from(self.cli.as_str()));
        map.insert("session".into(), Value::from(self.session.as_str()));
        map.insert("cursor".into(), Value::from(self.cursor.as_str()));
        map.insert(
            "anchor".into(),
            match &self.anchor {
                Some(anchor) => {
                    let mut a = Map::new();
                    a.insert("session".into(), Value::from(anchor.session.as_str()));
                    a.insert("turn".into(), Value::from(anchor.turn.as_str()));
                    a.insert("cursor".into(), Value::from(anchor.cursor.as_str()));
                    Value::Object(a)
                }
                None => Value::Null,
            },
        );
        map.insert("status".into(), Value::from(self.status.as_str()));
        if let Some(body) = &self.body {
            map.insert("body".into(), Value::from(body.as_str()));
        }
        if let Some(reason) = &self.reason {
            map.insert("reason".into(), Value::from(reason.as_str()));
        }
        map.insert(
            "seq".into(),
            self.seq.map(Value::from).unwrap_or(Value::Null),
        );
        map.insert("startedAt".into(), Value::from(self.started_at));
        Value::Object(map)
    }

    fn from_json(value: &Value) -> Option<NodeRecord> {
        let map = value.as_object()?;
        let text = |key: &str| {
            map.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let anchor = map.get("anchor").and_then(Value::as_object).map(|a| {
            let field = |key: &str| {
                a.get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            TurnAnchor {
                session: field("session"),
                turn: field("turn"),
                cursor: field("cursor"),
            }
        });
        Some(NodeRecord {
            dispatch_id: text("dispatchId"),
            cli: text("cli"),
            session: text("session"),
            cursor: text("cursor"),
            anchor,
            status: text("status"),
            body: map.get("body").and_then(Value::as_str).map(str::to_string),
            reason: map
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string),
            seq: map.get("seq").and_then(Value::as_i64),
            started_at: map.get("startedAt").and_then(Value::as_u64).unwrap_or(0),
        })
    }
}

fn nodes_dir(workspace: &str) -> PathBuf {
    Path::new(workspace).join("run").join("nodes")
}

pub fn record_path(workspace: &str, name: &str) -> PathBuf {
    nodes_dir(workspace).join(format!("{name}.json"))
}

fn lock_path(workspace: &str, name: &str) -> PathBuf {
    nodes_dir(workspace).join(format!("{name}.lock"))
}

pub fn read_record(workspace: &str, name: &str) -> Option<NodeRecord> {
    let text = fs::read_to_string(record_path(workspace, name)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    NodeRecord::from_json(&value)
}

/// Atomic replace: a reader never sees a half-written record. The `Err`
/// is for the pending write before the dispatch; every later write goes
/// through `update_record`.
fn write_record(workspace: &str, name: &str, record: &NodeRecord) -> Result<(), NodeError> {
    let path = record_path(workspace, name);
    let dir = nodes_dir(workspace);
    fs::create_dir_all(&dir).map_err(|e| NodeError(e.to_string()))?;
    let tmp = dir.join(format!(".{name}.json.{}", std::process::id()));
    let text =
        serde_json::to_string_pretty(&record.to_json()).map_err(|e| NodeError(e.to_string()))?;
    fs::write(&tmp, text).map_err(|e| NodeError(e.to_string()))?;
    fs::rename(&tmp, &path).map_err(|e| NodeError(e.to_string()))
}

/// A record write after the dispatch: the task is with the member, so a
/// failure here is logged and the run goes on to its verdict.
fn update_record(workspace: &str, name: &str, record: &NodeRecord) {
    if let Err(err) = write_record(workspace, name, record) {
        log(&format!(
            "{name} record not updated to {} ({err}); the run goes on",
            record.status
        ));
    }
}

/// Drop a member's node record: the member is retired (`hive kill`,
/// `hive delete --down`, a node's own dead-row retire before it spawns),
/// so no run can own it any more. The lock file stays: a flock lives on
/// the inode, so unlinking it under a runner that holds it would hand the
/// next runner a fresh file and a second lock on the same member.
pub fn remove_record(workspace: &str, name: &str) {
    let _ = fs::remove_file(record_path(workspace, name));
}

/// The per-member run lock, held for the whole `run_node`; dropping it
/// releases the flock.
pub struct NodeLock {
    file: fs::File,
}

impl Drop for NodeLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// `Ok(None)` when another process holds the member's lock.
pub fn try_lock(workspace: &str, name: &str) -> Result<Option<NodeLock>, NodeError> {
    let dir = nodes_dir(workspace);
    fs::create_dir_all(&dir).map_err(|e| NodeError(e.to_string()))?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path(workspace, name))
        .map_err(|e| NodeError(e.to_string()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(NodeLock { file }));
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(None);
    }
    Err(NodeError(format!("node lock for '{name}': {err}")))
}

// ---------------------------------------------------------------------------
// ops
// ---------------------------------------------------------------------------

/// One hive interaction.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeOp {
    Spawn {
        name: String,
        cli: Option<String>,
        model: String,
    },
    Ready {
        name: String,
        cli: String,
    },
    /// The task: the prompt rides a task artifact named after the dispatch
    /// id, the body opens with the id, the envelope has no sender.
    DispatchTask {
        name: String,
        prompt: String,
        dispatch_id: String,
    },
}

/// `<workspace>/artifacts/tasks/<name>-<dispatch_id>.md`, created new: the
/// id is fresh per run, so an existing file is a collision to refuse, not
/// a file to overwrite.
fn task_artifact(
    env: &dyn NodeEnv,
    name: &str,
    dispatch_id: &str,
    text: &str,
) -> Result<String, NodeError> {
    let ctx = env.context()?;
    let tasks_dir = Path::new(&ctx.workspace).join("artifacts").join("tasks");
    fs::create_dir_all(&tasks_dir).map_err(|e| NodeError(e.to_string()))?;
    let path = tasks_dir.join(format!("{name}-{dispatch_id}.md"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| NodeError(format!("task artifact {}: {e}", path.display())))?;
    file.write_all(text.as_bytes())
        .map_err(|e| NodeError(e.to_string()))?;
    Ok(path.to_string_lossy().into_owned())
}

/// The envelope body: the dispatch id first (the input marker the reader
/// binds on), then the task's first line as the summary.
fn dispatch_body(dispatch_id: &str, prompt: &str) -> String {
    let first = prompt.lines().next().unwrap_or_default().trim();
    format!("task {dispatch_id}\n{first}")
}

/// What a dispatch left behind: the ledger seq of a delivered task, or
/// the reason the hived's answer never arrived — the task may be with
/// the member all the same, so it is not sent again.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Dispatched {
    Delivered(i64),
    AnswerLost(String),
}

/// Dispatch with bounded retries of a refusal: a cloud-backed transport
/// can refuse transiently under provider throttling, and a single blip
/// must not kill a whole orchestration. Still loud on exhaustion. A lost
/// answer is not a refusal and is never retried: the same task twice is
/// worse than one whose delivery the transcript has to confirm.
fn dispatch(
    env: &dyn NodeEnv,
    name: &str,
    body: &str,
    artifact: &str,
) -> Result<Dispatched, NodeError> {
    let mut last = String::new();
    for attempt in 0..ATTEMPTS {
        match env.dispatch(name, body, artifact) {
            Ok(seq) => return Ok(Dispatched::Delivered(seq)),
            Err(DispatchFailure::Unknown(reason)) => return Ok(Dispatched::AnswerLost(reason)),
            Err(DispatchFailure::Refused(exc)) => {
                last = exc;
                if attempt + 1 < ATTEMPTS {
                    log(&format!(
                        "{name} dispatch refused ({last}); retry {}/{ATTEMPTS}",
                        attempt + 2
                    ));
                    env.sleep(RETRY_GAP);
                }
            }
        }
    }
    Err(NodeError(format!(
        "dispatch to '{name}' failed after {ATTEMPTS} attempts: {last}"
    )))
}

fn spawn_member(
    env: &dyn NodeEnv,
    name: &str,
    cli: Option<&str>,
    model: &str,
) -> Result<SpawnedAgent, NodeError> {
    let mut last = String::new();
    for attempt in 0..ATTEMPTS {
        let result = {
            let _guard = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
            env.spawn(name, cli, model)
        };
        match result {
            Ok(agent) => return Ok(agent),
            // A cloud transport (codex mint, grok leader) fails fast under
            // provider throttling; absorb blips here, each retry visible.
            Err(exc) => last = exc,
        }
        if attempt + 1 < ATTEMPTS {
            log(&format!(
                "{name} spawn failed ({last}); retry {}/{ATTEMPTS}",
                attempt + 2
            ));
            env.sleep(RETRY_GAP);
        }
    }
    Err(NodeError(format!(
        "spawn '{name}' failed after {ATTEMPTS} attempts: {last}"
    )))
}

fn ready_gate(env: &dyn NodeEnv, name: &str, cli: &str) -> Result<(), NodeError> {
    env.ensure_hived().map_err(NodeError)?;
    if cli != "claude" {
        // claude inboxes queue; only TUI-injected CLIs need the ready gate.
        let not_ready = env.wait_ready(&HashSet::from([name.to_string()]));
        if !not_ready.is_empty() {
            return Err(NodeError(format!(
                "member '{name}' did not reach ready within the gate; inspect its pane"
            )));
        }
    }
    Ok(())
}

/// Execute one op against the live seams.
pub fn run_op(env: &dyn NodeEnv, op: &NodeOp) -> Result<Map<String, Value>, NodeError> {
    let mut result = Map::new();
    match op {
        NodeOp::Spawn { name, cli, model } => {
            let spawned = spawn_member(env, name, cli.as_deref(), model)?;
            result.insert("pane".to_string(), Value::String(spawned.pane_id));
            result.insert("cli".to_string(), Value::String(spawned.cli));
        }
        NodeOp::Ready { name, cli } => ready_gate(env, name, cli)?,
        NodeOp::DispatchTask {
            name,
            prompt,
            dispatch_id,
        } => {
            let artifact = task_artifact(env, name, dispatch_id, prompt)?;
            match dispatch(env, name, &dispatch_body(dispatch_id, prompt), &artifact)? {
                Dispatched::Delivered(seq) => {
                    result.insert("seq".to_string(), Value::from(seq));
                }
                // No seq to report: the answer that carried it was lost.
                Dispatched::AnswerLost(reason) => {
                    result.insert("seq".to_string(), Value::Null);
                    result.insert("answerLost".to_string(), Value::String(reason));
                }
            }
            result.insert("artifact".to_string(), Value::String(artifact));
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// node: one task on one member, as a single blocking call
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct NodeSpec {
    pub name: String,
    pub cli: Option<String>,
    pub model: String,
    pub task: String,
}

/// How a run ended: the status word and what goes with it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Verdict {
    status: &'static str,
    body: Option<String>,
    reason: Option<String>,
}

impl Verdict {
    fn reason(status: &'static str, reason: impl Into<String>) -> Verdict {
        Verdict {
            status,
            body: None,
            reason: Some(reason.into()),
        }
    }
}

/// The JSON line of `hive node run`.
struct NodeResult<'a> {
    name: &'a str,
    pane: String,
    reused: bool,
    dispatch_id: String,
    session: String,
    turn: Option<String>,
    verdict: Verdict,
}

impl NodeResult<'_> {
    fn into_map(self) -> Map<String, Value> {
        let mut result = Map::new();
        result.insert("status".to_string(), Value::from(self.verdict.status));
        result.insert("name".to_string(), Value::from(self.name));
        result.insert("pane".to_string(), Value::String(self.pane));
        result.insert("reused".to_string(), Value::Bool(self.reused));
        result.insert("dispatchId".to_string(), Value::String(self.dispatch_id));
        result.insert("session".to_string(), Value::String(self.session));
        result.insert(
            "turn".to_string(),
            self.turn.map(Value::String).unwrap_or(Value::Null),
        );
        if let Some(body) = self.verdict.body {
            result.insert("body".to_string(), Value::String(body));
        }
        if let Some(reason) = self.verdict.reason {
            result.insert("reason".to_string(), Value::String(reason));
        }
        result
    }
}

/// Consecutive reader errors, charged one poll each; the transcript is
/// given up on when they add up to the budget.
#[derive(Default)]
struct ErrorBudget {
    consecutive: u32,
}

impl ErrorBudget {
    fn reset(&mut self) {
        self.consecutive = 0;
    }

    /// Whether this error exhausts the budget.
    fn charge(&mut self, err: &ReadError) -> bool {
        self.consecutive += 1;
        let exhausted = f64::from(self.consecutive) * POLL_SECONDS >= READ_ERROR_BUDGET_SECONDS;
        if self.consecutive == 1 || exhausted {
            log(&format!("transcript read failed: {err}"));
        }
        exhausted
    }
}

/// Polls of a closed turn whose final message is still off disk, charged
/// one poll each from the first `Flushing` reading on; the text is given
/// up on when they add up to the budget.
#[derive(Default)]
struct FlushBudget {
    polls: u32,
    reason: Option<String>,
}

impl FlushBudget {
    /// Whether this poll exhausts the budget; `reason` is the reader's
    /// latest word on what is missing, None for a poll that said nothing.
    fn charge(&mut self, reason: Option<String>) -> bool {
        if reason.is_some() {
            self.reason = reason;
        }
        if self.reason.is_none() {
            return false;
        }
        self.polls += 1;
        f64::from(self.polls) * POLL_SECONDS >= FLUSH_BUDGET_SECONDS
    }

    fn verdict(self) -> Verdict {
        Verdict::reason(STATUS_AMBIGUOUS, self.reason.unwrap_or_default())
    }
}

/// What one poll for the dispatched input said.
enum InputProbe {
    Wait,
    Bound(TurnAnchor),
    Terminal(Verdict),
}

/// What one poll for the bound turn's end said.
enum OutcomeProbe {
    Wait,
    /// The turn is closed, its final message not on disk yet.
    Flushing(String),
    Terminal(Verdict),
}

fn probe_input(
    reader: &dyn TurnReader,
    record: &NodeRecord,
    cwd: Option<&str>,
    budget: &mut ErrorBudget,
) -> InputProbe {
    match reader.find_input(&record.session, cwd, &record.dispatch_id, &record.cursor) {
        Ok(InputBinding::Bound(anchor)) => InputProbe::Bound(anchor),
        Ok(InputBinding::Ambiguous(reason)) => {
            InputProbe::Terminal(Verdict::reason(STATUS_AMBIGUOUS, reason))
        }
        Ok(InputBinding::NotYet) => {
            budget.reset();
            InputProbe::Wait
        }
        Err(err) if budget.charge(&err) => InputProbe::Terminal(Verdict::reason(
            STATUS_TRANSCRIPT_UNAVAILABLE,
            err.to_string(),
        )),
        Err(_) => InputProbe::Wait,
    }
}

fn probe_outcome(
    reader: &dyn TurnReader,
    anchor: &TurnAnchor,
    cwd: Option<&str>,
    budget: &mut ErrorBudget,
) -> OutcomeProbe {
    match reader.outcome(anchor, cwd) {
        Ok(Some(outcome)) => {
            budget.reset();
            outcome_probe(outcome)
        }
        Ok(None) => {
            budget.reset();
            OutcomeProbe::Wait
        }
        Err(err) if budget.charge(&err) => OutcomeProbe::Terminal(Verdict::reason(
            STATUS_TRANSCRIPT_UNAVAILABLE,
            err.to_string(),
        )),
        Err(_) => OutcomeProbe::Wait,
    }
}

fn outcome_probe(outcome: TurnOutcome) -> OutcomeProbe {
    let verdict = match outcome {
        TurnOutcome::Flushing { reason } => return OutcomeProbe::Flushing(reason),
        TurnOutcome::Completed { text } => Verdict {
            status: STATUS_COMPLETED,
            body: Some(text),
            reason: None,
        },
        TurnOutcome::Interrupted { reason } => Verdict::reason(STATUS_INTERRUPTED, reason),
        TurnOutcome::Failed { reason } => Verdict::reason(STATUS_FAILED, reason),
        TurnOutcome::Ambiguous { reason } => Verdict::reason(STATUS_AMBIGUOUS, reason),
        TurnOutcome::SessionChanged { reason } => Verdict::reason(STATUS_SESSION_CHANGED, reason),
    };
    OutcomeProbe::Terminal(verdict)
}

fn gone(name: &str, phase: &str) -> Verdict {
    Verdict::reason(
        STATUS_MEMBER_GONE,
        format!("member '{name}' is gone {phase}; nothing more will be read"),
    )
}

/// Whether the member can still be read for this run: None while it is
/// alive on the session the task was dispatched into, else the verdict
/// that cuts the wait short — `member_gone`, or `session_changed` when its
/// roster row now names a different engine session (`/clear`, a resume
/// into another id). A row whose session id is momentarily missing is not
/// a change; only a different non-empty id is.
fn cut_off(env: &dyn NodeEnv, name: &str, record: &NodeRecord, phase: &str) -> Option<Verdict> {
    if !env.alive(name) {
        return Some(gone(name, phase));
    }
    let current = env.member(name)?.session_id?;
    if current.is_empty() || current == record.session {
        return None;
    }
    Some(Verdict::reason(
        STATUS_SESSION_CHANGED,
        format!(
            "member '{name}' moved from session {} to {current} {phase}; the turn is not readable there",
            record.session
        ),
    ))
}

/// Poll the roster until the member's row carries a session id (the hived
/// backfills it after the engine starts). `Err` names the verdict when it
/// never does or the member dies first.
fn wait_for_session(env: &dyn NodeEnv, name: &str) -> Result<MemberInfo, Verdict> {
    let polls = (SESSION_ID_WAIT_SECONDS / POLL_SECONDS) as u32;
    for _ in 0..=polls {
        if let Some(member) = env.member(name) {
            if member.session_id.is_some() {
                return Ok(member);
            }
        }
        if !env.alive(name) {
            return Err(gone(name, "before its session id was known"));
        }
        env.sleep(POLL_SECONDS);
    }
    Err(Verdict::reason(
        STATUS_TRANSCRIPT_UNAVAILABLE,
        format!("roster row for '{name}' never got a session id"),
    ))
}

/// Hold the dispatch until the member is between turns, so the task starts
/// a turn of its own instead of folding into one already running (a fresh
/// member's bootstrap turn, a reused member's current work). A member
/// spawned by this run is first watched until it has been seen with a turn
/// open once — a fast bootstrap can close between polls, so that sighting
/// is capped and a spawn never seen in a turn is taken as past it — then,
/// like a reused member, until the daemon says the turn is closed. Only
/// the daemon's own "closed" opens the dispatch: no answer says nothing
/// about the turn, and the wait is capped too — past `IDLE_WAIT_SECONDS`
/// the run ends `member_busy` without dispatching, since a task landing
/// mid-turn cannot own a turn. `Err` is the member dying meanwhile or
/// that cap.
fn wait_turn_closed(env: &dyn NodeEnv, name: &str, spawned: bool) -> Result<(), Verdict> {
    let died = || gone(name, "before the task was dispatched");
    if spawned {
        let polls = (BUSY_SIGHTING_SECONDS / POLL_SECONDS) as u32;
        for _ in 0..polls {
            if !env.alive(name) {
                return Err(died());
            }
            if env.turn_open(name) == Some(true) {
                break;
            }
            env.sleep(POLL_SECONDS);
        }
    }
    let polls = (IDLE_WAIT_SECONDS / POLL_SECONDS) as u32;
    let mut logged = false;
    for _ in 0..polls {
        if !env.alive(name) {
            return Err(died());
        }
        // Only the daemon's own "no turn open" opens the dispatch: no answer
        // (daemon unreachable for a poll) says nothing about the turn, and
        // dispatching on it lands the task mid-turn.
        if env.turn_open(name) == Some(false) {
            return Ok(());
        }
        if !logged {
            log(&format!("waiting for {name} to finish its current turn"));
            logged = true;
        }
        env.sleep(POLL_SECONDS);
    }
    Err(Verdict::reason(
        STATUS_MEMBER_BUSY,
        format!("turn still open after {}s", IDLE_WAIT_SECONDS as u64),
    ))
}

/// The transcript cursor before the dispatch, under the read-error budget.
fn take_cursor(
    env: &dyn NodeEnv,
    reader: &dyn TurnReader,
    name: &str,
    session: &str,
    cwd: Option<&str>,
) -> Result<String, Verdict> {
    let mut budget = ErrorBudget::default();
    loop {
        match reader.cursor(session, cwd) {
            Ok(cursor) => return Ok(cursor),
            Err(err) if budget.charge(&err) => {
                return Err(Verdict::reason(
                    STATUS_TRANSCRIPT_UNAVAILABLE,
                    err.to_string(),
                ))
            }
            Err(_) => {}
        }
        if !env.alive(name) {
            return Err(gone(name, "before the task was dispatched"));
        }
        env.sleep(POLL_SECONDS);
    }
}

/// How a wait ended: the verdict, and whether it settles the record. The
/// one verdict that does not (`terminal: false`) is a dispatch whose
/// answer was lost and whose input never showed in the transcript: the
/// member may be on the task, so the record stays pending and the name
/// owned until `hive kill`.
struct Ending {
    verdict: Verdict,
    terminal: bool,
}

impl Ending {
    fn terminal(verdict: Verdict) -> Ending {
        Ending {
            verdict,
            terminal: true,
        }
    }
}

/// Bind the turn the dispatch started, then wait for its end. Every poll
/// re-reads the member: one that dies or moves to another session gets one
/// last read of the phase it was in — a terminal outcome on the old anchor
/// still wins — before the run ends `member_gone` / `session_changed`. A
/// closed turn whose text is still flushing is polled on under
/// `FLUSH_BUDGET_SECONDS`, then `ambiguous`. A dispatch whose answer was
/// lost (`answer_lost`) is bound like any other when its input shows, and
/// given up on under `DISPATCH_UNKNOWN_SECONDS` when it never does — with
/// the record left pending, since the member may be on the task.
fn await_turn(
    env: &dyn NodeEnv,
    reader: &dyn TurnReader,
    workspace: &str,
    name: &str,
    record: &mut NodeRecord,
    cwd: Option<&str>,
    answer_lost: bool,
) -> Ending {
    let mut budget = ErrorBudget::default();
    let unobserved_polls = (DISPATCH_UNKNOWN_SECONDS / POLL_SECONDS) as u32;
    let mut unobserved = 0u32;
    let anchor = loop {
        match probe_input(reader, record, cwd, &mut budget) {
            InputProbe::Bound(anchor) => break anchor,
            InputProbe::Terminal(verdict) => return Ending::terminal(verdict),
            InputProbe::Wait => {}
        }
        if let Some(cut) = cut_off(env, name, record, "before its turn was bound") {
            return Ending::terminal(match probe_input(reader, record, cwd, &mut budget) {
                InputProbe::Terminal(verdict) => verdict,
                InputProbe::Bound(anchor) => {
                    match probe_outcome(reader, &anchor, cwd, &mut budget) {
                        OutcomeProbe::Terminal(verdict) => verdict,
                        _ => cut,
                    }
                }
                InputProbe::Wait => cut,
            });
        }
        if answer_lost {
            unobserved += 1;
            if unobserved >= unobserved_polls {
                return Ending {
                    verdict: Verdict::reason(
                        STATUS_AMBIGUOUS,
                        format!(
                            "dispatch answer lost and the task was not observed in the transcript within {}s",
                            DISPATCH_UNKNOWN_SECONDS as u64
                        ),
                    ),
                    terminal: false,
                };
            }
        }
        env.sleep(POLL_SECONDS);
    };
    log(&format!(
        "{name} took the task in session {} turn {}",
        anchor.session, anchor.turn
    ));
    record.anchor = Some(anchor.clone());
    record.status = STATUS_INPUT_BOUND.to_string();
    update_record(workspace, name, record);
    budget.reset();
    let mut flush = FlushBudget::default();
    loop {
        let flushing = match probe_outcome(reader, &anchor, cwd, &mut budget) {
            OutcomeProbe::Terminal(verdict) => return Ending::terminal(verdict),
            OutcomeProbe::Flushing(reason) => Some(reason),
            OutcomeProbe::Wait => None,
        };
        if flush.charge(flushing) {
            return Ending::terminal(flush.verdict());
        }
        if let Some(cut) = cut_off(env, name, record, "before its turn ended") {
            return Ending::terminal(match probe_outcome(reader, &anchor, cwd, &mut budget) {
                OutcomeProbe::Terminal(verdict) => verdict,
                _ => cut,
            });
        }
        env.sleep(POLL_SECONDS);
    }
}

/// `hive node run`: the whole node as one blocking call — what an external
/// orchestrator's proxy runs in the background and reads the result of. A
/// member of that name still alive is reused (the task becomes a follow-up
/// to it); a dead roster row is retired first. A spawn made here is rolled
/// back if the node fails before the task is dispatched, so the name never
/// stays occupied by a corpse. Past the dispatch every end is a verdict in
/// the returned map, never an `Err`.
pub fn run_node(env: &dyn NodeEnv, spec: &NodeSpec) -> Result<Map<String, Value>, NodeError> {
    let ctx = env.context()?;
    let workspace = ctx.workspace.as_str();
    let name = spec.name.as_str();
    let busy = |reason: String| {
        let existing = read_record(workspace, name);
        let member = env.member(name).unwrap_or_default();
        NodeResult {
            name,
            pane: member.pane_id,
            reused: true,
            dispatch_id: existing
                .as_ref()
                .map(|r| r.dispatch_id.clone())
                .unwrap_or_default(),
            session: existing
                .as_ref()
                .map(|r| r.session.clone())
                .unwrap_or_default(),
            turn: existing.and_then(|r| r.anchor.map(|a| a.turn)),
            verdict: Verdict::reason(STATUS_MEMBER_BUSY, reason),
        }
        .into_map()
    };
    let Some(_lock) = try_lock(workspace, name)? else {
        return Ok(busy(format!(
            "the node lock for '{name}' is held by another runner"
        )));
    };
    if let Some(record) = read_record(workspace, name) {
        if record.is_pending() && env.alive(name) {
            return Ok(busy(format!(
                "member '{name}' has a pending node run {} owned by another runner",
                record.dispatch_id
            )));
        }
    }

    let dispatch_id = mint_dispatch_id();
    let reused = env.alive(name);
    let (pane, cli) = if reused {
        let member = env.member(name).unwrap_or_default();
        log(&format!("{name} alive in {}; reusing", member.pane_id));
        (member.pane_id, member.cli)
    } else {
        env.retire(name);
        let spawned = run_op(
            env,
            &NodeOp::Spawn {
                name: name.to_string(),
                cli: spec.cli.clone(),
                model: spec.model.clone(),
            },
        )?;
        let pane = spawned
            .get("pane")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let cli = spawned
            .get("cli")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        log(&format!("{name} spawned in {pane}"));
        if let Err(err) = run_op(
            env,
            &NodeOp::Ready {
                name: name.to_string(),
                cli: cli.clone(),
            },
        ) {
            env.retire(name);
            return Err(err);
        }
        (pane, cli)
    };
    // Rollback of the spawn made here, for every end before the dispatch.
    let rollback = || {
        if !reused {
            env.retire(name);
        }
    };
    let pre_dispatch = |verdict: Verdict, session: String| {
        rollback();
        NodeResult {
            name,
            pane: pane.clone(),
            reused,
            dispatch_id: dispatch_id.clone(),
            session,
            turn: None,
            verdict,
        }
        .into_map()
    };

    let Some(reader) = env.reader(&cli) else {
        rollback();
        return Err(NodeError(format!(
            "no transcript reader for cli '{cli}'; a node reads its member's turn and cannot run on it"
        )));
    };
    if let Err(verdict) = wait_turn_closed(env, name, !reused) {
        return Ok(pre_dispatch(verdict, String::new()));
    }
    let member = match wait_for_session(env, name) {
        Ok(member) => member,
        Err(verdict) => return Ok(pre_dispatch(verdict, String::new())),
    };
    let session = member.session_id.clone().unwrap_or_default();
    let cwd = if member.cwd.is_empty() {
        None
    } else {
        Some(member.cwd.as_str())
    };
    let cursor = match take_cursor(env, reader.as_ref(), name, &session, cwd) {
        Ok(cursor) => cursor,
        Err(verdict) => return Ok(pre_dispatch(verdict, session)),
    };

    // The pending record goes down before the dispatch has any side effect
    // (task artifact, hived delivery): a runner killed in between leaves
    // the name owned, never a delivered task with no record. A refused
    // dispatch takes the record back with it; one whose answer was lost
    // keeps it, since the task may be with the member.
    let mut record = NodeRecord {
        dispatch_id: dispatch_id.clone(),
        cli,
        session: session.clone(),
        cursor,
        anchor: None,
        status: STATUS_PENDING.to_string(),
        body: None,
        reason: None,
        seq: None,
        started_at: epoch_seconds(),
    };
    if let Err(err) = write_record(workspace, name, &record) {
        rollback();
        return Err(err);
    }
    let dispatched = match run_op(
        env,
        &NodeOp::DispatchTask {
            name: name.to_string(),
            prompt: spec.task.clone(),
            dispatch_id: dispatch_id.clone(),
        },
    ) {
        Ok(d) => d,
        Err(err) => {
            remove_record(workspace, name);
            rollback();
            return Err(err);
        }
    };
    let answer_lost = dispatched.get("answerLost").and_then(Value::as_str);
    match answer_lost {
        Some(reason) => log(&format!(
            "{name} dispatch answer lost ({reason}); the task may have landed, watching the transcript for {dispatch_id}"
        )),
        None => {
            record.seq = dispatched.get("seq").and_then(Value::as_i64);
            update_record(workspace, name, &record);
            log(&format!(
                "{name} dispatched {dispatch_id}; waiting for its turn"
            ));
        }
    }

    let Ending { verdict, terminal } = await_turn(
        env,
        reader.as_ref(),
        workspace,
        name,
        &mut record,
        cwd,
        answer_lost.is_some(),
    );
    if terminal {
        record.status = verdict.status.to_string();
        record.body = verdict.body.clone();
        record.reason = verdict.reason.clone();
        update_record(workspace, name, &record);
        log(&format!("{name} {}", verdict.status));
    } else {
        log(&format!(
            "{name} {}; the record stays pending and the name owned until `hive kill {name}`",
            verdict.status
        ));
    }
    Ok(NodeResult {
        name,
        pane,
        reused,
        dispatch_id,
        session,
        turn: record.anchor.map(|a| a.turn),
        verdict,
    }
    .into_map())
}

// ---------------------------------------------------------------------------
// live wiring
// ---------------------------------------------------------------------------

/// The engine's own session id for a roster row — the id a transcript
/// reader opens. A claude row stores the bg job id (`team/mod.rs` records
/// `job_id_for_pane`), so for claude a job-id shaped value is mapped
/// through the job's engine entry and never handed on as is.
fn engine_session_id(
    cli: &str,
    row_session: Option<&str>,
    job_lookup: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let candidate = row_session.filter(|s| !s.is_empty())?;
    if cli == "claude" && crate::adapters::claude_bg::looks_like_job_id(candidate) {
        return job_lookup(candidate);
    }
    Some(candidate.to_string())
}

/// The `open` field of the hived's `turn-open` answer; None for no answer,
/// an error envelope, or a null `open`.
fn hived_turn_open(answer: Option<Map<String, Value>>) -> Option<bool> {
    let answer = answer?;
    if answer.get("ok") != Some(&Value::Bool(true)) {
        return None;
    }
    answer.get("open").and_then(Value::as_bool)
}

struct RealCtx {
    team_name: String,
    workspace: String,
    team: crate::team::Team,
}

/// Production `NodeEnv`: resolves the scoped team once and forwards every
/// seam to the team/send/adapter modules.
pub struct RealEnv {
    team_arg: Option<String>,
    ctx: Mutex<Option<RealCtx>>,
}

impl Default for RealEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl RealEnv {
    pub fn new() -> Self {
        Self::for_team(None)
    }

    /// Scope to an explicit team name instead of the caller's pane identity
    /// — the `--team` lane for callers outside tmux.
    pub fn for_team(team_arg: Option<String>) -> Self {
        RealEnv {
            team_arg,
            ctx: Mutex::new(None),
        }
    }

    fn with_ctx<R>(&self, f: impl FnOnce(&mut RealCtx) -> R) -> Result<R, NodeError> {
        let mut guard = self.ctx.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.is_none() {
            let (team_name, team) =
                crate::team::resolve_scoped_team(self.team_arg.as_deref(), true)
                    .map_err(|e| NodeError(e.to_string()))?;
            let team = team.ok_or_else(|| NodeError("no team resolved".to_string()))?;
            let workspace = crate::team::resolve_workspace(Some(&team), true)
                .map_err(|e| NodeError(e.to_string()))?;
            *guard = Some(RealCtx {
                team_name: team_name.unwrap_or_default(),
                workspace,
                team,
            });
        }
        Ok(f(guard.as_mut().expect("ctx resolved above")))
    }

    /// A fresh roster read — probes must not trust the snapshot this
    /// process resolved at start.
    fn fresh_team(&self) -> Option<crate::team::Team> {
        let name = self.context().ok()?.team_name;
        crate::team::Team::load(&name, "").ok()
    }
}

impl NodeEnv for RealEnv {
    fn context(&self) -> Result<Ctx, NodeError> {
        self.with_ctx(|c| Ctx {
            team_name: c.team_name.clone(),
            workspace: c.workspace.clone(),
        })
    }

    fn spawn(&self, name: &str, cli: Option<&str>, model: &str) -> Result<SpawnedAgent, String> {
        self.with_ctx(|c| {
            let team_name = c.team_name.clone();
            crate::team::spawn_team_agent(
                &mut c.team,
                &team_name,
                name,
                model,
                "",
                "",
                "hive:hive",
                &[],
                cli,
            )
            .map(|a| SpawnedAgent {
                pane_id: a.pane_id.clone(),
                cli: a.cli.clone(),
            })
            .map_err(|e| e.to_string())
        })
        .map_err(|e| e.0)
        .and_then(|inner| inner)
    }

    fn ensure_hived(&self) -> Result<(), String> {
        self.with_ctx(|c| {
            crate::team::ensure_team_hived(&c.team, Path::new(&c.workspace))
                .map_err(|e| e.to_string())
        })
        .map_err(|e| e.0)
        .and_then(|inner| inner)
    }

    fn wait_ready(&self, agents: &HashSet<String>) -> HashSet<String> {
        match self.context() {
            Ok(ctx) => {
                crate::send::wait_for_peer_ready(&ctx.workspace, &ctx.team_name, agents, 30.0, 0.5)
            }
            Err(_) => agents.clone(),
        }
    }

    fn dispatch(&self, target: &str, body: &str, artifact: &str) -> Result<i64, DispatchFailure> {
        self.with_ctx(|c| {
            crate::send::request_node_dispatch(&c.workspace, &c.team, target, body, artifact)
                .map(|payload| payload.get("seq").and_then(Value::as_i64).unwrap_or(0))
        })
        .map_err(|e| DispatchFailure::Refused(e.0))
        .and_then(|inner| inner)
    }

    fn alive(&self, name: &str) -> bool {
        self.fresh_team()
            .map(|t| t.member_alive(name))
            .unwrap_or(false)
    }

    fn member(&self, name: &str) -> Option<MemberInfo> {
        let team = self.fresh_team()?;
        let agent = team.agent_named(name)?;
        let session_id = engine_session_id(&agent.cli, agent.session_id.as_deref(), |job_id| {
            crate::adapters::claude_bg::engine_session_for_job(job_id)
                .map(|engine| engine.session_id)
                .filter(|sid| !sid.is_empty())
        });
        Some(MemberInfo {
            pane_id: agent.pane_id.clone(),
            cli: agent.cli.clone(),
            session_id,
            cwd: agent.cwd.clone(),
        })
    }

    /// One question to the hived (`turn-open`), which asks the member's
    /// engine directly: codex `thread/read`, the claude bg engine record,
    /// the grok leader's push-fed state.
    fn turn_open(&self, name: &str) -> Option<bool> {
        let ctx = self.context().ok()?;
        hived_turn_open(crate::hived::request_turn_open(
            &ctx.workspace,
            &ctx.team_name,
            name,
        ))
    }

    fn retire(&self, name: &str) {
        let _ = self.with_ctx(|c| {
            c.team.retire(name);
        });
    }

    fn reader(&self, cli: &str) -> Option<Box<dyn TurnReader>> {
        crate::adapters::turn::reader_for(cli)
    }

    fn sleep(&self, seconds: f64) {
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
    }
}

// ---------------------------------------------------------------------------
// test env
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_env {
    use super::*;
    use crate::adapters::turn::Cursor;
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[derive(Debug)]
    pub(crate) struct SpawnCall {
        pub name: String,
        pub cli: Option<String>,
    }

    #[derive(Debug)]
    pub(crate) struct DispatchCall {
        pub target: String,
        pub body: String,
        pub artifact: String,
        /// The env's sleep count when the dispatch was made.
        pub sleeps: u32,
    }

    /// One `find_input` call as the reader saw it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct InputCall {
        pub session: String,
        pub cwd: Option<String>,
        pub marker: String,
        pub cursor: String,
    }

    /// Answers are queues the test fills; the last answer of a queue
    /// sticks, an empty queue answers "nothing yet" (`Ok("c0")` /
    /// `NotYet` / `None`). Every call notes the status of the record at
    /// `record_path`, so a test can see the transitions a blocking run
    /// wrote along the way.
    #[derive(Default)]
    pub(crate) struct FakeReader {
        pub cursors: Mutex<VecDeque<Result<Cursor, ReadError>>>,
        pub inputs: Mutex<VecDeque<Result<InputBinding, ReadError>>>,
        pub outcomes: Mutex<VecDeque<Result<Option<TurnOutcome>, ReadError>>>,
        pub input_calls: Mutex<Vec<InputCall>>,
        pub outcome_calls: Mutex<Vec<TurnAnchor>>,
        pub record_path: Mutex<Option<PathBuf>>,
        pub statuses_seen: Mutex<Vec<String>>,
    }

    fn next<T: Clone>(queue: &Mutex<VecDeque<T>>, default: T) -> T {
        let mut queue = queue.lock().unwrap();
        match queue.len() {
            0 => default,
            1 => queue.front().cloned().unwrap_or(default),
            _ => queue.pop_front().unwrap_or(default),
        }
    }

    impl FakeReader {
        fn note_status(&self) {
            let path = self.record_path.lock().unwrap().clone();
            let status = path
                .and_then(|p| fs::read_to_string(p).ok())
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| v.get("status").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| "(none)".to_string());
            let mut seen = self.statuses_seen.lock().unwrap();
            if seen.last() != Some(&status) {
                seen.push(status);
            }
        }
    }

    /// The node records directory replaced by a plain file: every record
    /// write from here on fails (`create_dir_all` on a non-directory) and
    /// every read answers None.
    fn break_records(workspace: &str) {
        let dir = nodes_dir(workspace);
        fs::remove_dir_all(&dir).unwrap();
        fs::write(&dir, "").unwrap();
    }

    /// The env's handle on the shared reader.
    pub(crate) struct SharedReader(pub Arc<FakeReader>);

    impl TurnReader for SharedReader {
        fn cursor(&self, _session_id: &str, _cwd: Option<&str>) -> Result<Cursor, ReadError> {
            self.0.note_status();
            next(&self.0.cursors, Ok("c0".to_string()))
        }

        fn find_input(
            &self,
            session_id: &str,
            cwd: Option<&str>,
            marker: &str,
            cursor: &Cursor,
        ) -> Result<InputBinding, ReadError> {
            self.0.note_status();
            self.0.input_calls.lock().unwrap().push(InputCall {
                session: session_id.to_string(),
                cwd: cwd.map(str::to_string),
                marker: marker.to_string(),
                cursor: cursor.clone(),
            });
            next(&self.0.inputs, Ok(InputBinding::NotYet))
        }

        fn outcome(
            &self,
            anchor: &TurnAnchor,
            _cwd: Option<&str>,
        ) -> Result<Option<TurnOutcome>, ReadError> {
            self.0.note_status();
            self.0.outcome_calls.lock().unwrap().push(anchor.clone());
            next(&self.0.outcomes, Ok(None))
        }
    }

    /// Failure knobs replace flaky seams; `sleep` is a no-op that counts,
    /// and `die_after_sleeps` drops the member off the roster at that
    /// count. `agents` is the roster; a member is alive iff it is there.
    /// `turn_answers` is the daemons' `turn_open` answer queue (sticky
    /// last value, empty means no answer); a fresh env scripts one
    /// bootstrap turn — open once, then closed — and `add_live` an idle
    /// member. `session_changes` rewrites every roster member's session
    /// id at a sleep count (None: the id is momentarily missing).
    pub(crate) struct FakeEnv {
        pub workspace: PathBuf,
        pub ready: bool,
        pub spawn_fail_first: u32,
        /// Refuse that many dispatches first (`DispatchFailure::Refused`).
        pub dispatch_fail_first: u32,
        /// Lose the answer of every delivered dispatch: the task is on the
        /// fake transport (`dispatches`), and the env answers
        /// `DispatchFailure::Unknown` instead of the seq.
        pub lose_answer: bool,
        pub spawn_err: String,
        pub dispatch_err: String,
        pub spawn_without_session: bool,
        pub no_reader: bool,
        /// Make every later record write fail once a dispatch is delivered.
        pub break_records_on_dispatch: bool,
        pub die_after_sleeps: Option<u32>,
        /// Backfill every roster member's session id at that sleep count.
        pub session_after_sleeps: Option<u32>,
        pub session_changes: Mutex<Vec<(u32, Option<String>)>>,
        pub reader: Arc<FakeReader>,
        pub turn_answers: Mutex<VecDeque<Option<bool>>>,
        pub turn_calls: AtomicU32,
        pub spawns: Mutex<Vec<SpawnCall>>,
        pub dispatches: Mutex<Vec<DispatchCall>>,
        /// The member's node record as it stood at every dispatch attempt,
        /// delivered or refused.
        pub dispatch_records: Mutex<Vec<Option<NodeRecord>>>,
        pub msg_seq: AtomicU32,
        pub spawn_calls: AtomicU32,
        pub send_calls: AtomicU32,
        pub sleeps: AtomicU32,
        pub agents: Mutex<Vec<String>>,
        pub sessions: Mutex<HashMap<String, Option<String>>>,
        pub retired: Mutex<Vec<String>>,
    }

    pub(crate) fn fake_env(tmp: &Path) -> FakeEnv {
        let ws = tmp.join("ws");
        fs::create_dir_all(&ws).unwrap();
        FakeEnv {
            workspace: ws,
            ready: true,
            spawn_fail_first: 0,
            dispatch_fail_first: 0,
            lose_answer: false,
            spawn_err: "mint refused".to_string(),
            dispatch_err: "refused".to_string(),
            spawn_without_session: false,
            no_reader: false,
            break_records_on_dispatch: false,
            die_after_sleeps: None,
            session_after_sleeps: None,
            session_changes: Mutex::new(Vec::new()),
            reader: Arc::new(FakeReader::default()),
            turn_answers: Mutex::new(VecDeque::from([Some(true), Some(false)])),
            turn_calls: AtomicU32::new(0),
            spawns: Mutex::new(Vec::new()),
            dispatches: Mutex::new(Vec::new()),
            dispatch_records: Mutex::new(Vec::new()),
            msg_seq: AtomicU32::new(0),
            spawn_calls: AtomicU32::new(0),
            send_calls: AtomicU32::new(0),
            sleeps: AtomicU32::new(0),
            agents: Mutex::new(Vec::new()),
            sessions: Mutex::new(HashMap::new()),
            retired: Mutex::new(Vec::new()),
        }
    }

    impl FakeEnv {
        /// Put a live, idle member on the roster with a known session id.
        pub(crate) fn add_live(&self, name: &str) {
            self.agents.lock().unwrap().push(name.to_string());
            self.sessions
                .lock()
                .unwrap()
                .insert(name.to_string(), Some(format!("sess-{name}")));
            *self.turn_answers.lock().unwrap() = VecDeque::from([Some(false)]);
        }

        pub(crate) fn workspace_str(&self) -> String {
            self.workspace.to_string_lossy().into_owned()
        }
    }

    impl NodeEnv for FakeEnv {
        fn context(&self) -> Result<Ctx, NodeError> {
            Ok(Ctx {
                team_name: "t-x".to_string(),
                workspace: self.workspace_str(),
            })
        }

        fn spawn(
            &self,
            name: &str,
            cli: Option<&str>,
            _model: &str,
        ) -> Result<SpawnedAgent, String> {
            let n = self.spawn_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= self.spawn_fail_first {
                return Err(self.spawn_err.clone());
            }
            let mut spawns = self.spawns.lock().unwrap();
            spawns.push(SpawnCall {
                name: name.to_string(),
                cli: cli.map(str::to_string),
            });
            self.agents.lock().unwrap().push(name.to_string());
            self.sessions.lock().unwrap().insert(
                name.to_string(),
                if self.spawn_without_session {
                    None
                } else {
                    Some(format!("sess-{name}"))
                },
            );
            Ok(SpawnedAgent {
                pane_id: format!("%{}", spawns.len()),
                cli: cli.unwrap_or("claude").to_string(),
            })
        }

        fn ensure_hived(&self) -> Result<(), String> {
            Ok(())
        }

        fn wait_ready(&self, agents: &HashSet<String>) -> HashSet<String> {
            if self.ready {
                HashSet::new()
            } else {
                agents.clone()
            }
        }

        fn dispatch(
            &self,
            target: &str,
            body: &str,
            artifact: &str,
        ) -> Result<i64, DispatchFailure> {
            self.dispatch_records
                .lock()
                .unwrap()
                .push(read_record(&self.workspace_str(), target));
            let n = self.send_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= self.dispatch_fail_first {
                return Err(DispatchFailure::Refused(self.dispatch_err.clone()));
            }
            let seq = i64::from(self.msg_seq.fetch_add(1, Ordering::SeqCst) + 1);
            self.dispatches.lock().unwrap().push(DispatchCall {
                target: target.to_string(),
                body: body.to_string(),
                artifact: artifact.to_string(),
                sleeps: self.sleeps.load(Ordering::SeqCst),
            });
            if self.break_records_on_dispatch {
                break_records(&self.workspace_str());
            }
            if self.lose_answer {
                return Err(DispatchFailure::Unknown("read timed out".to_string()));
            }
            Ok(seq)
        }

        fn alive(&self, name: &str) -> bool {
            self.agents.lock().unwrap().iter().any(|a| a == name)
        }

        fn member(&self, name: &str) -> Option<MemberInfo> {
            if !self.alive(name) {
                return None;
            }
            Some(MemberInfo {
                pane_id: format!("%{name}"),
                cli: "claude".to_string(),
                session_id: self.sessions.lock().unwrap().get(name).cloned().flatten(),
                cwd: "/repo".to_string(),
            })
        }

        fn turn_open(&self, name: &str) -> Option<bool> {
            if !self.alive(name) {
                return None;
            }
            self.turn_calls.fetch_add(1, Ordering::SeqCst);
            next(&self.turn_answers, None)
        }

        fn retire(&self, name: &str) {
            let mut agents = self.agents.lock().unwrap();
            if let Some(pos) = agents.iter().position(|a| a == name) {
                agents.remove(pos);
                self.retired.lock().unwrap().push(name.to_string());
            }
            remove_record(&self.workspace_str(), name);
        }

        fn reader(&self, _cli: &str) -> Option<Box<dyn TurnReader>> {
            if self.no_reader {
                return None;
            }
            Some(Box::new(SharedReader(Arc::clone(&self.reader))))
        }

        fn sleep(&self, _seconds: f64) {
            let n = self.sleeps.fetch_add(1, Ordering::SeqCst) + 1;
            if self.die_after_sleeps == Some(n) {
                self.agents.lock().unwrap().clear();
            }
            if self.session_after_sleeps == Some(n) {
                let mut sessions = self.sessions.lock().unwrap();
                for name in self.agents.lock().unwrap().iter() {
                    sessions.insert(name.clone(), Some(format!("sess-{name}")));
                }
            }
            let due: Vec<Option<String>> = self
                .session_changes
                .lock()
                .unwrap()
                .iter()
                .filter(|(at, _)| *at == n)
                .map(|(_, session)| session.clone())
                .collect();
            for session in due {
                let mut sessions = self.sessions.lock().unwrap();
                for name in self.agents.lock().unwrap().iter() {
                    sessions.insert(name.clone(), session.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_env::*;
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::Ordering;
    use tempfile::TempDir;

    fn spawn(name: &str, cli: Option<&str>) -> NodeOp {
        NodeOp::Spawn {
            name: name.into(),
            cli: cli.map(str::to_string),
            model: String::new(),
        }
    }

    fn node(name: &str, cli: Option<&str>, task: &str) -> NodeSpec {
        NodeSpec {
            name: name.into(),
            cli: cli.map(str::to_string),
            model: String::new(),
            task: task.into(),
        }
    }

    fn anchor(session: &str, turn: &str) -> TurnAnchor {
        TurnAnchor {
            session: session.into(),
            turn: turn.into(),
            cursor: "c1".into(),
        }
    }

    /// A reader scripted for one bound turn that ends with `outcome`.
    fn script_turn(env: &FakeEnv, outcome: TurnOutcome) {
        env.reader.inputs.lock().unwrap().extend([
            Ok(InputBinding::NotYet),
            Ok(InputBinding::Bound(anchor("sess-audit", "u-1"))),
        ]);
        env.reader
            .outcomes
            .lock()
            .unwrap()
            .extend([Ok(None), Ok(Some(outcome))]);
    }

    fn watch_record(env: &FakeEnv, name: &str) {
        *env.reader.record_path.lock().unwrap() = Some(record_path(&env.workspace_str(), name));
    }

    #[test]
    fn test_mint_dispatch_id_shape_and_uniqueness() {
        let a = mint_dispatch_id();
        let b = mint_dispatch_id();
        for id in [&a, &b] {
            assert_eq!(id.len(), 15, "{id}");
            assert!(id.starts_with("nd-"), "{id}");
            assert!(
                id[3..]
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{id}"
            );
        }
        assert_ne!(a, b);
    }

    #[test]
    fn test_ops_cover_the_whole_node_protocol() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());

        let r = run_op(&env, &spawn("impl", None)).unwrap();
        assert_eq!(r["pane"], "%1");
        assert_eq!(r["cli"], "claude");
        run_op(
            &env,
            &NodeOp::Ready {
                name: "impl".into(),
                cli: "claude".into(),
            },
        )
        .unwrap();

        let r = run_op(
            &env,
            &NodeOp::DispatchTask {
                name: "impl".into(),
                prompt: "explore auth\nwrite findings".into(),
                dispatch_id: "nd-0123456789ab".into(),
            },
        )
        .unwrap();
        assert_eq!(r["seq"], 1);
        let artifact = r["artifact"].as_str().unwrap();
        assert_eq!(
            fs::read_to_string(artifact).unwrap(),
            "explore auth\nwrite findings"
        );
        assert!(
            artifact.ends_with("/artifacts/tasks/impl-nd-0123456789ab.md"),
            "{artifact}"
        );
        let d = env.dispatches.lock().unwrap();
        assert_eq!(d[0].target, "impl");
        assert_eq!(d[0].artifact, artifact);
        // The dispatch id is the input marker: verbatim in the body's
        // first line and in the artifact path the envelope carries.
        assert_eq!(d[0].body, "task nd-0123456789ab\nexplore auth");
        assert!(d[0].artifact.contains("nd-0123456789ab"));
    }

    #[test]
    fn test_task_artifact_never_clobbers() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        let p1 = task_artifact(&env, "explore", "nd-000000000001", "one").unwrap();
        let p2 = task_artifact(&env, "explore", "nd-000000000002", "two").unwrap();
        assert_ne!(p1, p2);
        assert_eq!(fs::read_to_string(&p1).unwrap(), "one");
        assert_eq!(fs::read_to_string(&p2).unwrap(), "two");
        // The same id twice is a collision, refused rather than overwritten.
        let err = task_artifact(&env, "explore", "nd-000000000001", "three").unwrap_err();
        assert!(err.0.contains("task artifact"), "{err}");
        assert_eq!(fs::read_to_string(&p1).unwrap(), "one");
    }

    #[test]
    fn test_ready_gates_non_claude_and_skips_claude() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.ready = false;
        run_op(
            &env,
            &NodeOp::Ready {
                name: "impl".into(),
                cli: "claude".into(),
            },
        )
        .unwrap();
        let err = run_op(
            &env,
            &NodeOp::Ready {
                name: "impl".into(),
                cli: "codex".into(),
            },
        )
        .unwrap_err();
        assert!(err.0.contains("did not reach ready"), "{err}");
    }

    #[test]
    fn test_spawn_and_dispatch_retry_transient_failures_then_stay_loud() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.spawn_fail_first = 1;
        env.dispatch_fail_first = 1;
        run_op(&env, &spawn("impl", Some("codex"))).unwrap();
        assert_eq!(env.spawn_calls.load(Ordering::SeqCst), 2);
        {
            let spawns = env.spawns.lock().unwrap();
            assert_eq!(spawns[0].name, "impl");
            assert_eq!(spawns[0].cli.as_deref(), Some("codex"));
        }
        run_op(
            &env,
            &NodeOp::DispatchTask {
                name: "impl".into(),
                prompt: "t".into(),
                dispatch_id: "nd-000000000001".into(),
            },
        )
        .unwrap();
        assert_eq!(env.send_calls.load(Ordering::SeqCst), 2);

        let mut env = fake_env(tmp.path());
        env.spawn_fail_first = u32::MAX;
        let err = run_op(&env, &spawn("impl", None)).unwrap_err();
        assert!(err.0.contains("after 3 attempts"), "{err}");
        env.dispatch_fail_first = u32::MAX;
        let err = run_op(
            &env,
            &NodeOp::DispatchTask {
                name: "impl".into(),
                prompt: "t".into(),
                dispatch_id: "nd-000000000002".into(),
            },
        )
        .unwrap_err();
        assert!(err.0.contains("after 3 attempts"), "{err}");

        // A lost answer is not a refusal: one attempt, no seq, the reason
        // handed on for the run to watch the transcript.
        let mut env = fake_env(tmp.path());
        env.lose_answer = true;
        let r = run_op(
            &env,
            &NodeOp::DispatchTask {
                name: "impl".into(),
                prompt: "t".into(),
                dispatch_id: "nd-000000000003".into(),
            },
        )
        .unwrap();
        assert_eq!(env.send_calls.load(Ordering::SeqCst), 1);
        assert_eq!(env.dispatches.lock().unwrap().len(), 1);
        assert_eq!(r["seq"], Value::Null);
        assert_eq!(r["answerLost"], "read timed out");
    }

    #[test]
    fn test_run_node_returns_the_turns_final_message_in_full() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        let text = "Findings:\n\n- one\n- two\n\n```rs\nfn x() {}\n```\nDone at /tmp/report.md";
        script_turn(
            &env,
            TurnOutcome::Completed {
                text: text.to_string(),
            },
        );
        watch_record(&env, "audit");

        let r = run_node(&env, &node("audit", Some("codex"), "review it\nclosely")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["name"], "audit");
        assert_eq!(r["pane"], "%1");
        assert_eq!(r["reused"], false);
        assert_eq!(r["session"], "sess-audit");
        assert_eq!(r["turn"], "u-1");
        assert_eq!(r["body"], text);
        assert!(r.get("reason").is_none());
        let id = r["dispatchId"].as_str().unwrap();
        assert!(id.starts_with("nd-") && id.len() == 15, "{id}");

        // The reader was asked for exactly this dispatch, past the cursor
        // taken before the dispatch, in the member's own session and cwd.
        let calls = env.reader.input_calls.lock().unwrap();
        assert_eq!(calls[0].marker, id);
        assert_eq!(calls[0].cursor, "c0");
        assert_eq!(calls[0].session, "sess-audit");
        assert_eq!(calls[0].cwd.as_deref(), Some("/repo"));
        assert_eq!(
            env.reader.outcome_calls.lock().unwrap()[0],
            anchor("sess-audit", "u-1")
        );
        // The injected text carries the id, and the artifact holds the task.
        let d = env.dispatches.lock().unwrap();
        assert!(d[0].body.contains(id), "{}", d[0].body);
        assert!(d[0].artifact.contains(id), "{}", d[0].artifact);
        assert_eq!(
            fs::read_to_string(&d[0].artifact).unwrap(),
            "review it\nclosely"
        );
        // The record moved pending → input_bound → completed.
        assert_eq!(
            *env.reader.statuses_seen.lock().unwrap(),
            vec!["(none)", "pending", "input_bound"]
        );
        let record = read_record(&env.workspace_str(), "audit").unwrap();
        assert_eq!(record.status, "completed");
        assert_eq!(record.dispatch_id, id);
        assert_eq!(record.body.as_deref(), Some(text));
        assert_eq!(record.anchor, Some(anchor("sess-audit", "u-1")));
        assert_eq!(record.seq, Some(1));
        assert_eq!(record.cursor, "c0");
        assert_eq!(record.cli, "codex");
        assert!(record.started_at > 0);
        assert!(!record.is_pending());
    }

    #[test]
    fn test_run_node_completed_with_no_text_is_an_empty_body() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        script_turn(
            &env,
            TurnOutcome::Completed {
                text: String::new(),
            },
        );
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "");
    }

    #[test]
    fn test_run_node_reports_the_engines_own_end_labels() {
        for (outcome, status, reason) in [
            (
                TurnOutcome::Interrupted {
                    reason: "user_cancelled".into(),
                },
                "interrupted",
                "user_cancelled",
            ),
            (
                TurnOutcome::Failed {
                    reason: "api error".into(),
                },
                "failed",
                "api error",
            ),
            (
                TurnOutcome::Ambiguous {
                    reason: "compaction rewrote the branch".into(),
                },
                "ambiguous",
                "compaction rewrote the branch",
            ),
            (
                TurnOutcome::SessionChanged {
                    reason: "session id changed".into(),
                },
                "session_changed",
                "session id changed",
            ),
        ] {
            let tmp = TempDir::new().unwrap();
            let env = fake_env(tmp.path());
            script_turn(&env, outcome);
            let r = run_node(&env, &node("audit", None, "t")).unwrap();
            assert_eq!(r["status"], status);
            assert_eq!(r["reason"], reason);
            assert_eq!(r["turn"], "u-1");
            assert!(r.get("body").is_none());
            let record = read_record(&env.workspace_str(), "audit").unwrap();
            assert_eq!(record.status, status);
            assert_eq!(record.reason.as_deref(), Some(reason));
        }
    }

    #[test]
    fn test_run_node_ambiguous_input_is_terminal_before_any_turn_is_bound() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.reader
            .inputs
            .lock()
            .unwrap()
            .push_back(Ok(InputBinding::Ambiguous(
                "folded into a running turn".into(),
            )));
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "ambiguous");
        assert_eq!(r["reason"], "folded into a running turn");
        assert_eq!(r["turn"], Value::Null);
        assert!(env.reader.outcome_calls.lock().unwrap().is_empty());
        let record = read_record(&env.workspace_str(), "audit").unwrap();
        assert_eq!(record.status, "ambiguous");
        assert_eq!(record.anchor, None);
    }

    #[test]
    fn test_run_node_gives_up_on_a_transcript_that_keeps_failing() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.reader
            .inputs
            .lock()
            .unwrap()
            .push_back(Err(ReadError::Unavailable("no file".into())));
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "transcript_unavailable");
        assert_eq!(r["reason"], "transcript unavailable: no file");
        // 60s of 1s polls, then the verdict — not one error, not forever.
        assert_eq!(env.reader.input_calls.lock().unwrap().len(), 60);
        assert_eq!(
            read_record(&env.workspace_str(), "audit").unwrap().status,
            "transcript_unavailable"
        );

        // A transient error is absorbed: the budget resets on every read.
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.reader.inputs.lock().unwrap().extend([
            Err(ReadError::UnsupportedSchema("half line".into())),
            Ok(InputBinding::NotYet),
            Err(ReadError::Unavailable("blip".into())),
            Ok(InputBinding::Bound(anchor("sess-audit", "u-1"))),
        ]);
        env.reader.outcomes.lock().unwrap().extend([
            Err(ReadError::Unavailable("blip".into())),
            Ok(Some(TurnOutcome::Completed { text: "ok".into() })),
        ]);
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "ok");
    }

    #[test]
    fn test_run_node_needs_a_cursor_before_it_dispatches() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.reader
            .cursors
            .lock()
            .unwrap()
            .push_back(Err(ReadError::Unavailable("not written yet".into())));
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "transcript_unavailable");
        assert_eq!(r["reused"], false);
        assert!(env.dispatches.lock().unwrap().is_empty());
        // The spawn made here is rolled back, like every pre-dispatch end.
        assert_eq!(*env.retired.lock().unwrap(), vec!["audit".to_string()]);
    }

    #[test]
    fn test_run_node_needs_the_members_session_id() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.spawn_without_session = true;
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "transcript_unavailable");
        assert!(r["reason"]
            .as_str()
            .unwrap()
            .contains("never got a session id"));
        assert_eq!(r["session"], "");
        assert!(env.dispatches.lock().unwrap().is_empty());
        assert_eq!(*env.retired.lock().unwrap(), vec!["audit".to_string()]);
        // The id is backfilled: a row that gets one while polled proceeds.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.spawn_without_session = true;
        env.session_after_sleeps = Some(3);
        script_turn(
            &env,
            TurnOutcome::Completed {
                text: "late".into(),
            },
        );
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["session"], "sess-audit");
        assert_eq!(r["body"], "late");
    }

    #[test]
    fn test_run_node_ends_as_member_gone_after_one_last_read() {
        // Dies while the input is still awaited.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.die_after_sleeps = Some(2);
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "member_gone");
        assert!(r["reason"]
            .as_str()
            .unwrap()
            .contains("before its turn was bound"));
        assert_eq!(
            read_record(&env.workspace_str(), "audit").unwrap().status,
            "member_gone"
        );

        // Dies while the outcome is awaited, but the last read has it: the
        // turn's end wins over the death.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.die_after_sleeps = Some(1);
        env.reader
            .inputs
            .lock()
            .unwrap()
            .push_back(Ok(InputBinding::Bound(anchor("sess-audit", "u-1"))));
        env.reader.outcomes.lock().unwrap().extend([
            Ok(None),
            Ok(None),
            Ok(Some(TurnOutcome::Completed {
                text: "just made it".into(),
            })),
        ]);
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "just made it");

        // Dies with the turn bound and no end readable.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.die_after_sleeps = Some(1);
        env.reader
            .inputs
            .lock()
            .unwrap()
            .push_back(Ok(InputBinding::Bound(anchor("sess-audit", "u-1"))));
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "member_gone");
        assert_eq!(r["turn"], "u-1");
    }

    #[test]
    fn test_run_node_is_busy_on_a_live_members_pending_record() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.add_live("audit");
        let ws = env.workspace_str();
        write_record(
            &ws,
            "audit",
            &NodeRecord {
                dispatch_id: "nd-aaaaaaaaaaaa".into(),
                cli: "claude".into(),
                session: "sess-audit".into(),
                cursor: "c0".into(),
                anchor: Some(anchor("sess-audit", "u-9")),
                status: STATUS_INPUT_BOUND.into(),
                body: None,
                reason: None,
                seq: Some(4),
                started_at: 1,
            },
        )
        .unwrap();
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "member_busy");
        assert_eq!(r["dispatchId"], "nd-aaaaaaaaaaaa");
        assert_eq!(r["session"], "sess-audit");
        assert_eq!(r["turn"], "u-9");
        assert_eq!(r["reused"], true);
        assert_eq!(r["pane"], "%audit");
        assert!(r["reason"].as_str().unwrap().contains("nd-aaaaaaaaaaaa"));
        assert!(env.dispatches.lock().unwrap().is_empty());
        // The other runner's record is untouched.
        assert_eq!(read_record(&ws, "audit").unwrap().status, "input_bound");

        // A terminal record never blocks.
        let mut done = read_record(&ws, "audit").unwrap();
        done.status = STATUS_COMPLETED.into();
        write_record(&ws, "audit", &done).unwrap();
        script_turn(
            &env,
            TurnOutcome::Completed {
                text: "next".into(),
            },
        );
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_ne!(r["dispatchId"], "nd-aaaaaaaaaaaa");
    }

    #[test]
    fn test_run_node_replaces_a_dead_members_stale_pending_record() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        let ws = env.workspace_str();
        write_record(
            &ws,
            "audit",
            &NodeRecord {
                dispatch_id: "nd-aaaaaaaaaaaa".into(),
                cli: "claude".into(),
                session: "old".into(),
                cursor: "c0".into(),
                anchor: None,
                status: STATUS_PENDING.into(),
                body: None,
                reason: None,
                seq: Some(4),
                started_at: 1,
            },
        )
        .unwrap();
        script_turn(
            &env,
            TurnOutcome::Completed {
                text: "fresh".into(),
            },
        );
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        let record = read_record(&ws, "audit").unwrap();
        assert_ne!(record.dispatch_id, "nd-aaaaaaaaaaaa");
        assert_eq!(record.dispatch_id, r["dispatchId"]);
        assert_eq!(record.session, "sess-audit");
    }

    #[test]
    fn test_run_node_is_busy_when_the_member_lock_is_held() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.add_live("audit");
        let ws = env.workspace_str();
        let held = try_lock(&ws, "audit").unwrap().expect("first lock");
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "member_busy");
        assert!(r["reason"].as_str().unwrap().contains("lock"));
        assert_eq!(r["dispatchId"], "");
        assert!(env.dispatches.lock().unwrap().is_empty());
        assert!(read_record(&ws, "audit").is_none());
        drop(held);
        script_turn(&env, TurnOutcome::Completed { text: "now".into() });
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        // The run's own lock is released with it.
        assert!(try_lock(&ws, "audit").unwrap().is_some());
    }

    #[test]
    fn test_remove_record_drops_the_record_and_keeps_the_lock_file() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        script_turn(&env, TurnOutcome::Completed { text: "x".into() });
        run_node(&env, &node("audit", None, "t")).unwrap();
        let ws = env.workspace_str();
        assert!(record_path(&ws, "audit").exists());
        assert!(lock_path(&ws, "audit").exists());
        remove_record(&ws, "audit");
        assert!(!record_path(&ws, "audit").exists());
        assert!(lock_path(&ws, "audit").exists());
        // Idempotent.
        remove_record(&ws, "audit");

        // The lock survives a retire under a running node: a second runner
        // is still refused while the first holds it.
        let held = try_lock(&ws, "audit").unwrap().expect("lock");
        remove_record(&ws, "audit");
        assert!(try_lock(&ws, "audit").unwrap().is_none());
        drop(held);
        assert!(try_lock(&ws, "audit").unwrap().is_some());
    }

    #[test]
    fn test_record_round_trips_through_json() {
        let record = NodeRecord {
            dispatch_id: "nd-0123456789ab".into(),
            cli: "grok".into(),
            session: "s".into(),
            cursor: "events.jsonl:120".into(),
            anchor: Some(anchor("s", "s/3")),
            status: STATUS_COMPLETED.into(),
            body: Some("done".into()),
            reason: None,
            seq: Some(7),
            started_at: 1_700_000_000,
        };
        let json = record.to_json();
        assert_eq!(json["dispatchId"], "nd-0123456789ab");
        assert_eq!(json["anchor"]["turn"], "s/3");
        assert_eq!(json["startedAt"], 1_700_000_000u64);
        assert!(json.get("reason").is_none());
        assert_eq!(NodeRecord::from_json(&json), Some(record));
        let pending = NodeRecord {
            anchor: None,
            status: STATUS_PENDING.into(),
            body: None,
            seq: None,
            ..NodeRecord::from_json(&json).unwrap()
        };
        let json = pending.to_json();
        assert_eq!(json["anchor"], Value::Null);
        assert_eq!(json["seq"], Value::Null);
        assert_eq!(NodeRecord::from_json(&json), Some(pending));
    }

    #[test]
    fn test_run_node_reuses_a_living_member_and_retires_a_dead_row() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.add_live("audit");
        script_turn(
            &env,
            TurnOutcome::Completed {
                text: "again".into(),
            },
        );
        let r = run_node(&env, &node("audit", None, "follow-up task")).unwrap();
        assert_eq!(r["reused"], true);
        assert_eq!(r["status"], "completed");
        // a reused member reports the pane it already sits in
        assert_eq!(r["pane"], "%audit");
        assert!(env.spawns.lock().unwrap().is_empty());
        assert_eq!(env.dispatches.lock().unwrap().len(), 1);
    }

    #[test]
    fn test_run_node_rolls_back_its_own_spawn_on_failure() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.ready = false; // codex hits the gate after the spawn registered
        let err = run_node(&env, &node("audit", Some("codex"), "t")).unwrap_err();
        assert!(err.0.contains("did not reach ready"), "{err}");
        assert!(!env.alive("audit"));
        assert_eq!(*env.retired.lock().unwrap(), vec!["audit".to_string()]);

        // No reader for the member's CLI: loud, and the spawn is undone.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.no_reader = true;
        let err = run_node(&env, &node("audit", None, "t")).unwrap_err();
        assert!(err.0.contains("no transcript reader"), "{err}");
        assert_eq!(*env.retired.lock().unwrap(), vec!["audit".to_string()]);
        assert!(read_record(&env.workspace_str(), "audit").is_none());
    }

    #[test]
    fn test_run_node_does_not_retire_a_reused_member_on_failure() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.add_live("audit");
        env.dispatch_fail_first = u32::MAX;
        let err = run_node(&env, &node("audit", None, "t")).unwrap_err();
        assert!(err.0.contains("after 3 attempts"), "{err}");
        assert!(env.alive("audit"));
        // Nothing was dispatched, so nothing is recorded.
        assert!(read_record(&env.workspace_str(), "audit").is_none());
    }

    #[test]
    fn test_hived_turn_open_reads_only_a_bool_from_an_ok_answer() {
        let answer = |ok: bool, open: Value| {
            Some(Map::from_iter([
                ("ok".to_string(), Value::Bool(ok)),
                ("open".to_string(), open),
            ]))
        };
        assert_eq!(hived_turn_open(answer(true, Value::Bool(true))), Some(true));
        assert_eq!(
            hived_turn_open(answer(true, Value::Bool(false))),
            Some(false)
        );
        assert_eq!(hived_turn_open(answer(true, Value::Null)), None);
        assert_eq!(hived_turn_open(answer(false, Value::Bool(false))), None);
        assert_eq!(hived_turn_open(None), None);
    }

    #[test]
    fn test_engine_session_id_never_hands_a_claude_job_id_to_a_reader() {
        let uuid = "0f4e2a9c-6b1d-4e0a-9c3b-1d2e3f4a5b6c";
        let lookup = |job: &str| (job == "b9beb2b8").then(|| uuid.to_string());
        let none = |_: &str| None;
        // The registry row of a claude bg member is the job id.
        assert_eq!(
            engine_session_id("claude", Some("b9beb2b8"), lookup).as_deref(),
            Some(uuid)
        );
        // With no engine entry there is no session, never the job id.
        assert_eq!(engine_session_id("claude", Some("b9beb2b8"), none), None);
        // A joined claude member's row already names the engine session.
        assert_eq!(
            engine_session_id("claude", Some(uuid), none).as_deref(),
            Some(uuid)
        );
        // Other engines' rows pass through, whatever their shape.
        assert_eq!(
            engine_session_id("codex", Some("thr-1"), none).as_deref(),
            Some("thr-1")
        );
        assert_eq!(
            engine_session_id("codex", Some("abcdef12"), none).as_deref(),
            Some("abcdef12")
        );
        assert_eq!(engine_session_id("codex", None, none), None);
        assert_eq!(engine_session_id("codex", Some(""), none), None);
    }

    #[test]
    fn test_run_node_waits_for_a_fresh_spawns_bootstrap_turn_to_close() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        // No answer yet, then a turn open (the bootstrap turn), then closed.
        *env.turn_answers.lock().unwrap() =
            VecDeque::from([None, Some(true), Some(true), Some(false)]);
        script_turn(&env, TurnOutcome::Completed { text: "ok".into() });
        let r = run_node(&env, &node("audit", Some("codex"), "t")).unwrap();
        assert_eq!(r["status"], "completed");
        // One poll to see the turn open, one more while it stays so; the
        // dispatch went out only on the closed answer.
        let d = env.dispatches.lock().unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].sleeps, 2);
        assert_eq!(env.turn_calls.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn test_run_node_waits_for_a_reused_member_to_finish_its_turn() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.add_live("audit");
        *env.turn_answers.lock().unwrap() = VecDeque::from([Some(true), Some(true), Some(false)]);
        script_turn(&env, TurnOutcome::Completed { text: "ok".into() });
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["reused"], true);
        let d = env.dispatches.lock().unwrap();
        assert_eq!(d[0].sleeps, 2);
        assert_eq!(env.turn_calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_run_node_is_busy_when_the_idle_wait_expires() {
        // A reused member that never closes its turn: no dispatch, no
        // record, and the member is left as it was.
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.add_live("audit");
        *env.turn_answers.lock().unwrap() = VecDeque::from([Some(true)]);
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "member_busy");
        assert_eq!(r["reason"], "turn still open after 600s");
        assert_eq!(r["reused"], true);
        assert_eq!(r["session"], "");
        assert!(env.dispatches.lock().unwrap().is_empty());
        assert!(env.dispatch_records.lock().unwrap().is_empty());
        assert!(read_record(&env.workspace_str(), "audit").is_none());
        assert!(env.alive("audit"));
        assert!(env.retired.lock().unwrap().is_empty());
        assert_eq!(
            env.sleeps.load(Ordering::SeqCst),
            (IDLE_WAIT_SECONDS / POLL_SECONDS) as u32
        );

        // A member spawned by this run is rolled back like every other
        // pre-dispatch end.
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        *env.turn_answers.lock().unwrap() = VecDeque::from([Some(true)]);
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "member_busy");
        assert_eq!(r["reason"], "turn still open after 600s");
        assert_eq!(r["reused"], false);
        assert!(env.dispatches.lock().unwrap().is_empty());
        assert!(!env.alive("audit"));
        assert_eq!(*env.retired.lock().unwrap(), vec!["audit".to_string()]);
        assert!(read_record(&env.workspace_str(), "audit").is_none());
    }

    #[test]
    fn test_run_node_ends_as_member_gone_when_the_member_dies_before_idle() {
        // A spawn of this run dies mid-bootstrap: no dispatch, no record.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.die_after_sleeps = Some(3);
        *env.turn_answers.lock().unwrap() = VecDeque::from([Some(true)]);
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "member_gone");
        assert_eq!(r["reused"], false);
        assert!(r["reason"]
            .as_str()
            .unwrap()
            .contains("before the task was dispatched"));
        assert!(env.dispatches.lock().unwrap().is_empty());
        assert!(read_record(&env.workspace_str(), "audit").is_none());
        assert!(!env.alive("audit"));

        // A reused member dies while its turn is awaited: same verdict,
        // and nothing of it is touched.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.add_live("audit");
        env.die_after_sleeps = Some(1);
        *env.turn_answers.lock().unwrap() = VecDeque::from([Some(true)]);
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "member_gone");
        assert_eq!(r["reused"], true);
        assert!(env.dispatches.lock().unwrap().is_empty());
        assert!(env.retired.lock().unwrap().is_empty());
    }

    #[test]
    fn test_run_node_keeps_waiting_on_an_unanswered_daemon_until_the_idle_cap() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        *env.turn_answers.lock().unwrap() = VecDeque::from([None]);
        script_turn(&env, TurnOutcome::Completed { text: "ok".into() });
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        // No answer is never an idle reading: the whole sighting window and
        // the whole idle wait pass, and the run ends without dispatching.
        assert_eq!(r["status"], "member_busy");
        assert_eq!(r["reason"], "turn still open after 600s");
        let sighting = (BUSY_SIGHTING_SECONDS / POLL_SECONDS) as u32;
        let idle = (IDLE_WAIT_SECONDS / POLL_SECONDS) as u32;
        assert_eq!(env.sleeps.load(Ordering::SeqCst), sighting + idle);
        assert!(env.dispatches.lock().unwrap().is_empty());
        assert_eq!(*env.retired.lock().unwrap(), vec!["audit".to_string()]);
    }

    #[test]
    fn test_run_node_records_pending_before_the_dispatch_and_backfills_seq() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        script_turn(&env, TurnOutcome::Completed { text: "ok".into() });
        let r = run_node(&env, &node("audit", Some("codex"), "t")).unwrap();
        assert_eq!(r["status"], "completed");
        // The record the hived saw when the dispatch reached it: already
        // pending, with everything but the seq it was about to mint.
        let at_dispatch = env.dispatch_records.lock().unwrap();
        assert_eq!(at_dispatch.len(), 1);
        let pending = at_dispatch[0].as_ref().expect("record before dispatch");
        assert_eq!(pending.status, "pending");
        assert_eq!(pending.dispatch_id, r["dispatchId"]);
        assert_eq!(pending.cli, "codex");
        assert_eq!(pending.session, "sess-audit");
        assert_eq!(pending.cursor, "c0");
        assert_eq!(pending.anchor, None);
        assert_eq!(pending.seq, None);
        assert!(pending.started_at > 0);
        assert!(pending.is_pending());
        // The seq is filled in once the hived answered.
        let record = read_record(&env.workspace_str(), "audit").unwrap();
        assert_eq!(record.seq, Some(1));
        assert_eq!(record.started_at, pending.started_at);
    }

    #[test]
    fn test_run_node_takes_the_record_back_when_the_dispatch_is_refused() {
        // A reused member: every attempt saw the pending record, the
        // refusal is still an Err, and nothing of the run is left behind.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.add_live("audit");
        env.dispatch_fail_first = u32::MAX;
        let err = run_node(&env, &node("audit", None, "t")).unwrap_err();
        assert!(err.0.contains("after 3 attempts"), "{err}");
        let at_dispatch = env.dispatch_records.lock().unwrap();
        assert_eq!(at_dispatch.len(), 3);
        assert!(at_dispatch
            .iter()
            .all(|r| r.as_ref().map(|r| r.status.as_str()) == Some("pending")));
        assert!(read_record(&env.workspace_str(), "audit").is_none());
        assert!(env.alive("audit"));
        assert!(env.retired.lock().unwrap().is_empty());

        // A spawn of this run is rolled back with the record.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.dispatch_fail_first = u32::MAX;
        let err = run_node(&env, &node("audit", None, "t")).unwrap_err();
        assert!(err.0.contains("after 3 attempts"), "{err}");
        assert!(read_record(&env.workspace_str(), "audit").is_none());
        assert!(!env.alive("audit"));
        assert_eq!(*env.retired.lock().unwrap(), vec!["audit".to_string()]);
    }

    #[test]
    fn test_run_node_backfills_the_seq_of_a_dispatch_accepted_after_a_refusal() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.add_live("audit");
        env.dispatch_fail_first = 1;
        script_turn(&env, TurnOutcome::Completed { text: "ok".into() });
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        // Two attempts, one delivery, and the seq of that one delivery.
        assert_eq!(env.send_calls.load(Ordering::SeqCst), 2);
        assert_eq!(env.dispatches.lock().unwrap().len(), 1);
        let at_dispatch = env.dispatch_records.lock().unwrap();
        assert_eq!(at_dispatch.len(), 2);
        assert!(at_dispatch
            .iter()
            .all(|r| r.as_ref().map(|r| r.status.as_str()) == Some("pending")));
        assert!(at_dispatch
            .iter()
            .all(|r| r.as_ref().unwrap().seq.is_none()));
        let record = read_record(&env.workspace_str(), "audit").unwrap();
        assert_eq!(record.seq, Some(1));
        assert_eq!(record.status, "completed");
    }

    #[test]
    fn test_run_node_never_repeats_a_dispatch_whose_answer_was_lost() {
        // The hived took the task and the answer never came back: the
        // dispatch is not retried, the record stays pending with no seq,
        // and the marker binding later is the delivery confirmation — the
        // run then ends like any delivered dispatch.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.lose_answer = true;
        env.add_live("audit");
        watch_record(&env, "audit");
        env.reader.inputs.lock().unwrap().extend([
            Ok(InputBinding::NotYet),
            Ok(InputBinding::NotYet),
            Ok(InputBinding::Bound(anchor("sess-audit", "u-1"))),
        ]);
        env.reader
            .outcomes
            .lock()
            .unwrap()
            .push_back(Ok(Some(TurnOutcome::Completed { text: "ok".into() })));
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "ok");
        assert_eq!(r["turn"], "u-1");
        assert_eq!(env.send_calls.load(Ordering::SeqCst), 1);
        assert_eq!(env.dispatches.lock().unwrap().len(), 1);
        assert_eq!(env.reader.input_calls.lock().unwrap().len(), 3);
        // No record at the cursor read, pending through the whole input
        // wait, input_bound for the outcome read.
        assert_eq!(
            *env.reader.statuses_seen.lock().unwrap(),
            vec![
                "(none)".to_string(),
                "pending".to_string(),
                "input_bound".to_string()
            ]
        );
        // No seq was ever learned, and the record says so.
        let record = read_record(&env.workspace_str(), "audit").unwrap();
        assert_eq!(record.seq, None);
        assert_eq!(record.status, "completed");
    }

    #[test]
    fn test_run_node_leaves_a_lost_dispatch_pending_when_its_input_never_shows() {
        // A spawn of this run, the answer lost, the marker never in the
        // transcript: the polls of the unknown-dispatch budget, then
        // ambiguous — with the record still pending and the member not
        // retired, since it may be on the task.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.lose_answer = true;
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "ambiguous");
        assert_eq!(r["reused"], false);
        assert_eq!(r["turn"], Value::Null);
        assert_eq!(
            r["reason"],
            "dispatch answer lost and the task was not observed in the transcript within 120s"
        );
        assert_eq!(env.send_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            env.reader.input_calls.lock().unwrap().len(),
            (DISPATCH_UNKNOWN_SECONDS / POLL_SECONDS) as usize
        );
        assert!(env.alive("audit"));
        assert!(env.retired.lock().unwrap().is_empty());
        let record = read_record(&env.workspace_str(), "audit").unwrap();
        assert_eq!(record.status, "pending");
        assert_eq!(record.seq, None);
        assert_eq!(record.dispatch_id, r["dispatchId"]);
        assert!(record.is_pending());

        // The name stays owned: the next run of it is member_busy on that
        // very record, and dispatches nothing.
        let r2 = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r2["status"], "member_busy");
        assert_eq!(r2["dispatchId"], r["dispatchId"]);
        assert_eq!(r2["reused"], true);
        assert_eq!(env.send_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            read_record(&env.workspace_str(), "audit").unwrap().status,
            "pending"
        );
    }

    #[test]
    fn test_run_node_ends_a_lost_dispatch_as_member_gone_when_the_member_dies() {
        // The unknown-dispatch wait is cut short like any other: a member
        // that dies during it ends the run member_gone, and that verdict
        // is terminal.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.lose_answer = true;
        env.add_live("audit");
        env.die_after_sleeps = Some(5);
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "member_gone");
        assert_eq!(env.send_calls.load(Ordering::SeqCst), 1);
        let record = read_record(&env.workspace_str(), "audit").unwrap();
        assert_eq!(record.status, "member_gone");
        assert!(!record.is_pending());
    }

    #[test]
    fn test_run_node_keeps_its_verdict_when_the_record_fails_after_the_dispatch() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.break_records_on_dispatch = true;
        script_turn(&env, TurnOutcome::Completed { text: "ok".into() });
        // The task is with the member: the seq backfill, the input_bound
        // and the terminal write all fail, and the run still ends in its
        // verdict, never an Err.
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "ok");
        assert_eq!(r["turn"], "u-1");
        assert_eq!(env.dispatches.lock().unwrap().len(), 1);
        assert!(read_record(&env.workspace_str(), "audit").is_none());
        assert!(env.alive("audit"));
    }

    #[test]
    fn test_run_node_ends_as_session_changed_when_the_member_moves_session() {
        // The input is still awaited when the member's session changes:
        // one last look at the old session, then the verdict names both.
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.session_changes
            .lock()
            .unwrap()
            .push((2, Some("sess-new".into())));
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "session_changed");
        let reason = r["reason"].as_str().unwrap();
        assert!(reason.contains("sess-audit"), "{reason}");
        assert!(reason.contains("sess-new"), "{reason}");
        assert!(reason.contains("before its turn was bound"), "{reason}");
        assert_eq!(r["session"], "sess-audit");
        assert_eq!(r["turn"], Value::Null);
        // Two polls before the change, the poll that saw it, one last read.
        assert_eq!(env.reader.input_calls.lock().unwrap().len(), 4);
        assert!(env
            .reader
            .input_calls
            .lock()
            .unwrap()
            .iter()
            .all(|c| c.session == "sess-audit"));
        let record = read_record(&env.workspace_str(), "audit").unwrap();
        assert_eq!(record.status, "session_changed");
        assert!(env.alive("audit"));
        assert!(env.retired.lock().unwrap().is_empty());

        // Bound, then the session changes, and the last read of the old
        // anchor has the end: the turn's outcome wins.
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.session_changes
            .lock()
            .unwrap()
            .push((1, Some("sess-new".into())));
        env.reader
            .inputs
            .lock()
            .unwrap()
            .push_back(Ok(InputBinding::Bound(anchor("sess-audit", "u-1"))));
        env.reader.outcomes.lock().unwrap().extend([
            Ok(None),
            Ok(None),
            Ok(Some(TurnOutcome::Completed {
                text: "just in time".into(),
            })),
        ]);
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "just in time");
        assert_eq!(env.reader.outcome_calls.lock().unwrap().len(), 3);

        // Bound, the session changes, and the old anchor never ends.
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.session_changes
            .lock()
            .unwrap()
            .push((1, Some("sess-new".into())));
        env.reader
            .inputs
            .lock()
            .unwrap()
            .push_back(Ok(InputBinding::Bound(anchor("sess-audit", "u-1"))));
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "session_changed");
        assert_eq!(r["turn"], "u-1");
        assert!(r["reason"]
            .as_str()
            .unwrap()
            .contains("before its turn ended"));
        assert_eq!(env.reader.outcome_calls.lock().unwrap().len(), 3);
        let record = read_record(&env.workspace_str(), "audit").unwrap();
        assert_eq!(record.status, "session_changed");
        assert_eq!(record.anchor, Some(anchor("sess-audit", "u-1")));
    }

    #[test]
    fn test_run_node_ignores_a_session_id_that_goes_missing() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        // The roster row loses its session id for two polls (a hived
        // backfill in flight), then carries the same id again.
        env.session_changes
            .lock()
            .unwrap()
            .extend([(1, None), (3, Some("sess-audit".into()))]);
        env.reader.inputs.lock().unwrap().extend([
            Ok(InputBinding::NotYet),
            Ok(InputBinding::NotYet),
            Ok(InputBinding::NotYet),
            Ok(InputBinding::NotYet),
            Ok(InputBinding::Bound(anchor("sess-audit", "u-1"))),
        ]);
        env.reader.outcomes.lock().unwrap().extend([
            Ok(None),
            Ok(Some(TurnOutcome::Completed {
                text: "still mine".into(),
            })),
        ]);
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "still mine");
        assert_eq!(env.reader.input_calls.lock().unwrap().len(), 5);
    }

    #[test]
    fn test_run_node_polls_a_flushing_turn_under_the_flush_budget() {
        // The turn closed before its text landed: the reader is polled on,
        // and the text that lands is the result.
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.reader
            .inputs
            .lock()
            .unwrap()
            .push_back(Ok(InputBinding::Bound(anchor("sess-audit", "u-1"))));
        env.reader.outcomes.lock().unwrap().extend([
            Ok(None),
            Ok(Some(TurnOutcome::Flushing {
                reason: "turn_ended, history line not written".into(),
            })),
            Ok(Some(TurnOutcome::Completed {
                text: "landed".into(),
            })),
        ]);
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "landed");
        assert_eq!(env.reader.outcome_calls.lock().unwrap().len(), 3);

        // The text never lands: the budget's worth of polls, then ambiguous
        // with the reader's own reason.
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.reader
            .inputs
            .lock()
            .unwrap()
            .push_back(Ok(InputBinding::Bound(anchor("sess-audit", "u-1"))));
        env.reader
            .outcomes
            .lock()
            .unwrap()
            .push_back(Ok(Some(TurnOutcome::Flushing {
                reason: "turn_ended, history line not written".into(),
            })));
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "ambiguous");
        assert_eq!(r["reason"], "turn_ended, history line not written");
        assert_eq!(r["turn"], "u-1");
        assert_eq!(
            env.reader.outcome_calls.lock().unwrap().len(),
            (FLUSH_BUDGET_SECONDS / POLL_SECONDS) as usize
        );
        assert!(env.alive("audit"));
        let record = read_record(&env.workspace_str(), "audit").unwrap();
        assert_eq!(record.status, "ambiguous");
        assert_eq!(
            record.reason.as_deref(),
            Some("turn_ended, history line not written")
        );

        // The budget runs from the first Flushing reading, not from the
        // last: a reader that falls back to "still running" keeps the
        // clock going and the reason.
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.reader
            .inputs
            .lock()
            .unwrap()
            .push_back(Ok(InputBinding::Bound(anchor("sess-audit", "u-1"))));
        env.reader.outcomes.lock().unwrap().extend([
            Ok(Some(TurnOutcome::Flushing {
                reason: "final block pending".into(),
            })),
            Ok(None),
        ]);
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "ambiguous");
        assert_eq!(r["reason"], "final block pending");
        assert_eq!(
            env.reader.outcome_calls.lock().unwrap().len(),
            (FLUSH_BUDGET_SECONDS / POLL_SECONDS) as usize
        );
    }

    #[test]
    fn test_run_node_does_not_dispatch_on_a_daemon_dropout_mid_turn() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        // The daemon answers, drops out for a poll, answers again: only its
        // own "closed" opens the dispatch.
        *env.turn_answers.lock().unwrap() =
            VecDeque::from([Some(true), None, Some(true), None, Some(false)]);
        script_turn(&env, TurnOutcome::Completed { text: "ok".into() });
        let r = run_node(&env, &node("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        let d = env.dispatches.lock().unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].sleeps, 3);
    }

    // -- RealEnv over the live wiring ------------------------------------------
    //
    // The production env resolves the team from the registry, asks the hived
    // over its socket, and reads the roster back. Everything below is real
    // except what needs a live member: the hived's member lookup
    // (`resolve_live_agent`), send gate (`check_send_gate`) and transport
    // hand-off (`agent_send`) answer through the hived test hook, and tmux is
    // the fake `team/mod.rs` uses in test builds.

    #[test]
    fn test_real_env_dispatches_without_a_sender_and_reads_the_roster() {
        use crate::hived::testhook::Hook as HivedHook;
        use crate::hived::HivedServerApi;
        use std::sync::{Arc, Mutex};

        let mut env = crate::testenv::EnvGuard::new();
        let home = TempDir::new().unwrap();
        env.set("HIVE_HOME", home.path().join(".hive"));
        // A short workspace path keeps the hived socket in-tree; an overlong
        // one is relocated under /tmp/hive-<uid>/ and would outlive the TempDir.
        let ws_tmp = tempfile::Builder::new()
            .prefix("hive-nd-")
            .tempdir_in("/tmp")
            .unwrap();
        let workspace = ws_tmp.path().to_string_lossy().to_string();
        crate::bus::init_workspace(&workspace).unwrap();

        // The registry row RealEnv::for_team resolves; the fake tmux answers
        // list-windows with the window that claims it, so the team loads
        // with its window identity and ensure_hived never asks tmux.
        let team = "nodet";
        let member = Map::from_iter([
            ("name".to_string(), Value::from("b")),
            ("cli".to_string(), Value::from("codex")),
            ("sessionId".to_string(), Value::from("thr-1")),
            ("cwd".to_string(), Value::from("/repo")),
        ]);
        let grok_member = Map::from_iter([
            ("name".to_string(), Value::from("g")),
            ("cli".to_string(), Value::from("grok")),
            ("sessionId".to_string(), Value::from("grk-1")),
            ("cwd".to_string(), Value::from("/repo")),
        ]);
        assert_eq!(
            crate::registry::record_team(
                team,
                &workspace,
                "1700000000",
                &[member, grok_member],
                "dev:1"
            )
            .unwrap(),
            "written"
        );
        let window_row = format!("dev:1\t@7\t{team}\t{workspace}\t\t1700000000\n");
        crate::team::set_fake_tmux_run(move |args, _check| {
            let stdout = if args.first().map(String::as_str) == Some("list-windows") {
                window_row.clone()
            } else {
                String::new()
            };
            Ok(crate::tmux::Run {
                returncode: 0,
                stdout,
                stderr: String::new(),
            })
        });
        // The real tmux module must stay untouched on the RealEnv (this
        // thread's) path; the override is thread-local, so the serve thread
        // is not covered by this recorder.
        let real_tmux_argv: std::rc::Rc<std::cell::RefCell<Vec<Vec<String>>>> = Default::default();
        let recorded = std::rc::Rc::clone(&real_tmux_argv);
        crate::tmux::set_run_override(move |args, _check, _timeout| {
            recorded.borrow_mut().push(args.to_vec());
            Err(crate::tmux::TmuxError::Os(
                "no tmux in this test".to_string(),
            ))
        });

        // A hived on a real socket: the node-dispatch arm writes the bus row
        // itself and hands the envelope to the hooked transport.
        let handed: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let handed_sink = Arc::clone(&handed);
        let ws_hook = workspace.clone();
        let _hived_guard = crate::hived::testhook::install(HivedHook {
            resolve_live_agent: Some(Arc::new(move |team_name, agent| {
                let team = crate::team::Team {
                    name: team_name.to_string(),
                    workspace: ws_hook.clone(),
                    tmux_session: "dev".to_string(),
                    tmux_window: "dev:1".to_string(),
                    tmux_window_id: "@7".to_string(),
                    ..Default::default()
                };
                Ok((
                    team,
                    crate::agent::testhook::fake_agent(agent, team_name, "%9", "codex"),
                ))
            })),
            check_send_gate: Some(Arc::new(|_target| Ok(()))),
            // The hived's team-runtime answer for the headless codex row
            // must not look for a daemon on this machine.
            cas_runtime_for_thread: Some(Arc::new(|_thread| None)),
            // The codex app-server's `thread/read` on the roster thread id,
            // as the hived's `turn-open` asks it for the codex row: no
            // turn open.
            cas_turn_open_for_thread: Some(Arc::new(|thread| {
                assert_eq!(thread, "thr-1");
                Some(false)
            })),
            // The grok leader pool's push-fed turn evidence, as the
            // hived's `turn-open` reads it for the grok row.
            gl_turn_open_for_key: Some(Arc::new(move |key| {
                (key == format!("m-{team}.g")).then_some(Some(true))
            })),
            agent_send: Some(Arc::new(move |_agent, text, sender| {
                handed_sink
                    .lock()
                    .unwrap()
                    .push((text.to_string(), sender.to_string()));
                Ok("accepted".to_string())
            })),
            ..Default::default()
        });
        let server = Arc::new(crate::hived::open_server_socket(&workspace).unwrap());
        let serve_thread = {
            let server = Arc::clone(&server);
            let workspace = workspace.clone();
            std::thread::spawn(move || {
                crate::hived::serve_requests(
                    server.as_ref(),
                    &workspace,
                    team,
                    "dev:1",
                    "@7",
                    "2026-04-17T00:00:00Z",
                    2.0,
                )
            })
        };

        let env = RealEnv::for_team(Some(team.to_string()));
        let ctx = env.context().unwrap();
        assert_eq!(ctx.team_name, team);
        assert_eq!(ctx.workspace, workspace);

        // The roster row as the node reads it: engine, session id, cwd.
        assert_eq!(
            env.member("b"),
            Some(MemberInfo {
                pane_id: String::new(),
                cli: "codex".to_string(),
                session_id: Some("thr-1".to_string()),
                cwd: "/repo".to_string(),
            })
        );
        assert_eq!(env.member("nobody"), None);
        // Every row's turn is one question to the hived over the socket:
        // the codex row's thread is asked of the app-server, the grok row's
        // of the leader pool, both behind the hived's seams above.
        assert_eq!(env.turn_open("b"), Some(false));
        assert_eq!(env.turn_open("g"), Some(true));
        assert_eq!(env.turn_open("nobody"), None);
        assert!(env.reader("codex").is_some());
        assert!(env.reader("bash").is_none());

        let artifact = format!("{workspace}/artifacts/tasks/b-nd-0123456789ab.md");
        let seq = env
            .dispatch("b", "task nd-0123456789ab\nfirst task", &artifact)
            .unwrap();
        assert!(seq > 0);
        let events = crate::bus::read_all_events(&workspace).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, seq);
        // No sender on the ledger row, no `from` on the envelope, and the
        // transport's origin label is the team.
        assert_eq!(events[0].from, "");
        assert_eq!(events[0].to, "b");
        assert_eq!(events[0].body, "task nd-0123456789ab\nfirst task");
        assert_eq!(events[0].artifact, artifact);
        {
            let handed = handed.lock().unwrap();
            assert_eq!(handed.len(), 1);
            assert_eq!(
                handed[0].0,
                format!(
                    "<HIVE to=b artifact={artifact}>\ntask nd-0123456789ab\nfirst task\n</HIVE>"
                )
            );
            assert_eq!(handed[0].1, team);
        }

        assert!(
            real_tmux_argv.borrow().is_empty(),
            "real tmux reached: {:?}",
            real_tmux_argv.borrow()
        );

        let shutdown = Map::from_iter([("action".to_string(), Value::from("shutdown"))]);
        let bye =
            crate::hived::request_hived(&workspace, &shutdown, crate::hived::SOCKET_READY_TIMEOUT);
        assert_eq!(
            bye.and_then(|m| m.get("ok").cloned()),
            Some(Value::Bool(true))
        );
        // The loop is parked in accept: one more client wakes it to notice
        // the shutdown flag instead of waiting out the accept timeout.
        let ping = Map::from_iter([("action".to_string(), Value::from("ping"))]);
        let _ = crate::hived::request_hived(&workspace, &ping, crate::hived::SOCKET_READY_TIMEOUT);
        assert!(!serve_thread.join().unwrap());
        server.close();
        crate::hived::cleanup_socket_impl(&workspace);
    }
}
