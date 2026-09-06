//! hive::workflow — one task on one live member, returned explicitly.
//!
//! A workflow node is one task placed on one live member, the way a
//! Claude Code Workflow subagent takes one: spawn a real pane (or reuse a
//! live member of that name), wait until it is ready and between turns,
//! dispatch the task as a `<HIVE>` envelope with no sender, and wait for
//! the member's own return. The return is explicit: the member ends its
//! task with `hive workflow done "<summary>" [--artifact FILE]`, which
//! writes `<workspace>/run/workflow/<name>.done.json` against the one
//! dispatch its pending record names. Nothing is inferred from an engine
//! transcript; a member that ends its turn without returning is reported
//! as such (`no_result`), never guessed at.
//!
//! One run is one record, `<workspace>/run/workflow/<name>.json`, written
//! pending before the dispatch has any side effect and again at its end,
//! held under the per-member flock `<name>.lock`; a pending record of a
//! live member is another runner's, a pending record of a dead member is
//! stale and replaced. Past the dispatch nothing is an `Err` any more:
//! exit 1 means "not dispatched", so a record write that fails later is
//! logged and the run still ends in a verdict. A dispatch the hived
//! refused is not dispatched; one whose answer was lost may be, is never
//! repeated, and keeps its pending record until the member returns, ends
//! its turn, or is killed.
//!
//! `WorkflowOp` is the typed vocabulary of one hive interaction, `run_op`
//! executes one, and `run_workflow` (`hive workflow run`) strings them
//! together for an external orchestrator; `record_done` is the member's
//! side (`hive workflow done`). `WorkflowEnv` is the seam over
//! cli/bus/team; tests inject a fake.

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

use crate::send::DispatchFailure;

const POLL_SECONDS: f64 = 1.0;
/// How long a member spawned by this run may go unseen with a turn open:
/// its bootstrap turn is what the idle wait has to outlast, and a fast one
/// can close between two polls, so a spawn never seen in a turn is taken as
/// past it.
const BUSY_SIGHTING_SECONDS: f64 = 60.0;
/// How long a dispatch waits for the member to be between turns; past it
/// the run ends `member_busy` without dispatching.
const IDLE_WAIT_SECONDS: f64 = 600.0;
/// Consecutive polls the daemon must answer "no turn open" after the
/// dispatch, with no return on disk, before the run ends `no_result`: a
/// single closed reading can be the gap before the task's turn opens.
const TURN_CLOSED_POLLS: u32 = 5;
/// How long a dispatch whose answer was lost (`DispatchFailure::Unknown`)
/// may go without a return or a closed turn, counted in polls from the
/// dispatch; past it the run ends `ambiguous` with the record left
/// pending, since the member may be on the task.
const DISPATCH_UNKNOWN_SECONDS: f64 = 120.0;
const ATTEMPTS: usize = 3;
const RETRY_GAP: f64 = 3.0;

pub const STATUS_PENDING: &str = "pending";
pub const STATUS_COMPLETED: &str = "completed";
pub const STATUS_NO_RESULT: &str = "no_result";
pub const STATUS_AMBIGUOUS: &str = "ambiguous";
pub const STATUS_MEMBER_GONE: &str = "member_gone";
pub const STATUS_MEMBER_BUSY: &str = "member_busy";

// tmux splits and team registration race each other in-process; spawns
// serialize, everything else stays parallel. (Cross-process, the registry
// name claim inside Team::spawn is the guard.)
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// The run could not reach a dispatch (bad team, spawn, ready gate, the
/// dispatch itself refused by the hived), or a `hive workflow done` with
/// nothing to return to. Everything after a dispatch — including one whose
/// answer was lost — is a verdict in the result, never an error.
#[derive(Debug)]
pub struct WorkflowError(pub String);

impl fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WorkflowError {}

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

/// A roster row as the runner needs it: where the member sits and which
/// engine it runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberInfo {
    pub pane_id: String,
    pub cli: String,
}

/// The seams a run reaches through. `Err(String)` from `spawn` and
/// `DispatchFailure::Refused` from `dispatch` are transient failures the
/// retry loops absorb; `DispatchFailure::Unknown` is never retried.
pub trait WorkflowEnv: Send + Sync {
    fn context(&self) -> Result<Ctx, WorkflowError>;
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
    fn sleep(&self, seconds: f64);
}

/// Progress goes to stderr so stdout carries only the result (the JSON
/// line of `hive workflow run`).
fn log(message: &str) {
    eprintln!("[workflow] {message}");
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
// run record, done file and lock
// ---------------------------------------------------------------------------

/// One run's persisted state, `<workspace>/run/workflow/<name>.json`.
///
/// The name is owned while the record is pending (`is_pending`) and free
/// under any terminal status. That is a v1 narrowing: a terminal verdict
/// says the runner stopped waiting, not that the member stopped working —
/// after `no_result` the member may still be on the task, and the next run
/// of that name is allowed to dispatch on top of it. The one verdict that
/// leaves the record pending is a dispatch whose answer was lost and that
/// neither returned nor closed its turn (`await_return`): there the run
/// ends `ambiguous` with the name still owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRecord {
    pub dispatch_id: String,
    pub cli: String,
    pub status: String,
    pub body: Option<String>,
    pub artifact: Option<String>,
    pub reason: Option<String>,
    /// Ledger seq of the dispatch row; None when the run ended before it
    /// or the answer that carried it was lost.
    pub seq: Option<i64>,
    pub started_at: u64,
}

impl WorkflowRecord {
    /// A run that has not reached a terminal status: the member is owned
    /// by the runner that wrote it.
    pub fn is_pending(&self) -> bool {
        self.status == STATUS_PENDING
    }

    fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("dispatchId".into(), Value::from(self.dispatch_id.as_str()));
        map.insert("cli".into(), Value::from(self.cli.as_str()));
        map.insert("status".into(), Value::from(self.status.as_str()));
        if let Some(body) = &self.body {
            map.insert("body".into(), Value::from(body.as_str()));
        }
        if let Some(artifact) = &self.artifact {
            map.insert("artifact".into(), Value::from(artifact.as_str()));
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

    fn from_json(value: &Value) -> Option<WorkflowRecord> {
        let map = value.as_object()?;
        let text = |key: &str| {
            map.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let optional = |key: &str| map.get(key).and_then(Value::as_str).map(str::to_string);
        Some(WorkflowRecord {
            dispatch_id: text("dispatchId"),
            cli: text("cli"),
            status: text("status"),
            body: optional("body"),
            artifact: optional("artifact"),
            reason: optional("reason"),
            seq: map.get("seq").and_then(Value::as_i64),
            started_at: map.get("startedAt").and_then(Value::as_u64).unwrap_or(0),
        })
    }
}

/// The member's return, `<workspace>/run/workflow/<name>.done.json`:
/// written by `hive workflow done` against the pending record's dispatch
/// id, consumed by the runner that waits on that id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoneRecord {
    pub dispatch_id: String,
    pub body: String,
    pub artifact: String,
    pub done_at: u64,
}

impl DoneRecord {
    fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("dispatchId".into(), Value::from(self.dispatch_id.as_str()));
        map.insert("body".into(), Value::from(self.body.as_str()));
        map.insert("artifact".into(), Value::from(self.artifact.as_str()));
        map.insert("doneAt".into(), Value::from(self.done_at));
        Value::Object(map)
    }

    fn from_json(value: &Value) -> Option<DoneRecord> {
        let map = value.as_object()?;
        let text = |key: &str| {
            map.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        Some(DoneRecord {
            dispatch_id: text("dispatchId"),
            body: text("body"),
            artifact: text("artifact"),
            done_at: map.get("doneAt").and_then(Value::as_u64).unwrap_or(0),
        })
    }
}

fn workflow_dir(workspace: &str) -> PathBuf {
    Path::new(workspace).join("run").join("workflow")
}

pub fn record_path(workspace: &str, name: &str) -> PathBuf {
    workflow_dir(workspace).join(format!("{name}.json"))
}

pub fn done_path(workspace: &str, name: &str) -> PathBuf {
    workflow_dir(workspace).join(format!("{name}.done.json"))
}

fn lock_path(workspace: &str, name: &str) -> PathBuf {
    workflow_dir(workspace).join(format!("{name}.lock"))
}

fn read_json(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn read_record(workspace: &str, name: &str) -> Option<WorkflowRecord> {
    WorkflowRecord::from_json(&read_json(&record_path(workspace, name))?)
}

pub fn read_done(workspace: &str, name: &str) -> Option<DoneRecord> {
    DoneRecord::from_json(&read_json(&done_path(workspace, name))?)
}

/// Atomic replace: a reader never sees a half-written file.
fn write_atomic(workspace: &str, path: &Path, value: &Value) -> Result<(), WorkflowError> {
    let dir = workflow_dir(workspace);
    fs::create_dir_all(&dir).map_err(|e| WorkflowError(e.to_string()))?;
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{stem}.{}", std::process::id()));
    let text = serde_json::to_string_pretty(value).map_err(|e| WorkflowError(e.to_string()))?;
    fs::write(&tmp, text).map_err(|e| WorkflowError(e.to_string()))?;
    fs::rename(&tmp, path).map_err(|e| WorkflowError(e.to_string()))
}

/// The `Err` is for the pending write before the dispatch; every later
/// write goes through `update_record`.
fn write_record(workspace: &str, name: &str, record: &WorkflowRecord) -> Result<(), WorkflowError> {
    write_atomic(workspace, &record_path(workspace, name), &record.to_json())
}

/// A record write after the dispatch: the task is with the member, so a
/// failure here is logged and the run goes on to its verdict.
fn update_record(workspace: &str, name: &str, record: &WorkflowRecord) {
    if let Err(err) = write_record(workspace, name, record) {
        log(&format!(
            "{name} record not updated to {} ({err}); the run goes on",
            record.status
        ));
    }
}

fn remove_done(workspace: &str, name: &str) {
    let _ = fs::remove_file(done_path(workspace, name));
}

/// Drop a member's run record and any return it left: the member is
/// retired (`hive kill`, `hive delete --down`, a run's own dead-row retire
/// before it spawns), so no run can own it any more. The lock file stays:
/// a flock lives on the inode, so unlinking it under a runner that holds
/// it would hand the next runner a fresh file and a second lock on the
/// same member.
pub fn remove_record(workspace: &str, name: &str) {
    let _ = fs::remove_file(record_path(workspace, name));
    remove_done(workspace, name);
}

/// `hive workflow done`: the member's return statement. The one pending
/// dispatch of the member's record is what it answers; with no pending
/// record there is nothing to return to, and a return already on disk for
/// that dispatch is not overwritten.
pub fn record_done(
    workspace: &str,
    name: &str,
    body: &str,
    artifact: &str,
) -> Result<DoneRecord, WorkflowError> {
    let record = read_record(workspace, name)
        .filter(WorkflowRecord::is_pending)
        .ok_or_else(|| WorkflowError(format!("no workflow task is waiting for {name}")))?;
    if read_done(workspace, name).is_some_and(|done| done.dispatch_id == record.dispatch_id) {
        return Err(WorkflowError(format!(
            "{name} already returned for {}",
            record.dispatch_id
        )));
    }
    let done = DoneRecord {
        dispatch_id: record.dispatch_id,
        body: body.to_string(),
        artifact: artifact.to_string(),
        done_at: epoch_seconds(),
    };
    write_atomic(workspace, &done_path(workspace, name), &done.to_json())?;
    Ok(done)
}

/// The per-member run lock, held for the whole `run_workflow`; dropping it
/// releases the flock.
pub struct WorkflowLock {
    file: fs::File,
}

impl Drop for WorkflowLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// `Ok(None)` when another process holds the member's lock.
pub fn try_lock(workspace: &str, name: &str) -> Result<Option<WorkflowLock>, WorkflowError> {
    let dir = workflow_dir(workspace);
    fs::create_dir_all(&dir).map_err(|e| WorkflowError(e.to_string()))?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path(workspace, name))
        .map_err(|e| WorkflowError(e.to_string()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(WorkflowLock { file }));
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(None);
    }
    Err(WorkflowError(format!("workflow lock for '{name}': {err}")))
}

// ---------------------------------------------------------------------------
// ops
// ---------------------------------------------------------------------------

/// One hive interaction.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowOp {
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
    env: &dyn WorkflowEnv,
    name: &str,
    dispatch_id: &str,
    text: &str,
) -> Result<String, WorkflowError> {
    let ctx = env.context()?;
    let tasks_dir = Path::new(&ctx.workspace).join("artifacts").join("tasks");
    fs::create_dir_all(&tasks_dir).map_err(|e| WorkflowError(e.to_string()))?;
    let path = tasks_dir.join(format!("{name}-{dispatch_id}.md"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| WorkflowError(format!("task artifact {}: {e}", path.display())))?;
    file.write_all(text.as_bytes())
        .map_err(|e| WorkflowError(e.to_string()))?;
    Ok(path.to_string_lossy().into_owned())
}

/// The envelope body: the dispatch id first, then the task's first line
/// as the summary.
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
/// worse than one whose delivery the member's return has to confirm.
fn dispatch(
    env: &dyn WorkflowEnv,
    name: &str,
    body: &str,
    artifact: &str,
) -> Result<Dispatched, WorkflowError> {
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
    Err(WorkflowError(format!(
        "dispatch to '{name}' failed after {ATTEMPTS} attempts: {last}"
    )))
}

fn spawn_member(
    env: &dyn WorkflowEnv,
    name: &str,
    cli: Option<&str>,
    model: &str,
) -> Result<SpawnedAgent, WorkflowError> {
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
    Err(WorkflowError(format!(
        "spawn '{name}' failed after {ATTEMPTS} attempts: {last}"
    )))
}

fn ready_gate(env: &dyn WorkflowEnv, name: &str, cli: &str) -> Result<(), WorkflowError> {
    env.ensure_hived().map_err(WorkflowError)?;
    if cli != "claude" {
        // claude inboxes queue; only TUI-injected CLIs need the ready gate.
        let not_ready = env.wait_ready(&HashSet::from([name.to_string()]));
        if !not_ready.is_empty() {
            return Err(WorkflowError(format!(
                "member '{name}' did not reach ready within the gate; inspect its pane"
            )));
        }
    }
    Ok(())
}

/// Execute one op against the live seams.
pub fn run_op(env: &dyn WorkflowEnv, op: &WorkflowOp) -> Result<Map<String, Value>, WorkflowError> {
    let mut result = Map::new();
    match op {
        WorkflowOp::Spawn { name, cli, model } => {
            let spawned = spawn_member(env, name, cli.as_deref(), model)?;
            result.insert("pane".to_string(), Value::String(spawned.pane_id));
            result.insert("cli".to_string(), Value::String(spawned.cli));
        }
        WorkflowOp::Ready { name, cli } => ready_gate(env, name, cli)?,
        WorkflowOp::DispatchTask {
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
// run: one task on one member, as a single blocking call
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct WorkflowSpec {
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
    artifact: Option<String>,
    reason: Option<String>,
}

impl Verdict {
    fn reason(status: &'static str, reason: impl Into<String>) -> Verdict {
        Verdict {
            status,
            body: None,
            artifact: None,
            reason: Some(reason.into()),
        }
    }

    fn completed(done: DoneRecord) -> Verdict {
        Verdict {
            status: STATUS_COMPLETED,
            body: Some(done.body),
            artifact: Some(done.artifact),
            reason: None,
        }
    }
}

/// The JSON line of `hive workflow run`.
struct WorkflowResult<'a> {
    name: &'a str,
    pane: String,
    reused: bool,
    dispatch_id: String,
    verdict: Verdict,
}

impl WorkflowResult<'_> {
    fn into_map(self) -> Map<String, Value> {
        let mut result = Map::new();
        result.insert("status".to_string(), Value::from(self.verdict.status));
        result.insert("name".to_string(), Value::from(self.name));
        result.insert("pane".to_string(), Value::String(self.pane));
        result.insert("reused".to_string(), Value::Bool(self.reused));
        result.insert("dispatchId".to_string(), Value::String(self.dispatch_id));
        if let Some(body) = self.verdict.body {
            result.insert("body".to_string(), Value::String(body));
        }
        if let Some(artifact) = self.verdict.artifact {
            result.insert("artifact".to_string(), Value::String(artifact));
        }
        if let Some(reason) = self.verdict.reason {
            result.insert("reason".to_string(), Value::String(reason));
        }
        result
    }
}

fn gone(name: &str, phase: &str) -> Verdict {
    Verdict::reason(
        STATUS_MEMBER_GONE,
        format!("member '{name}' is gone {phase}; nothing more will be waited for"),
    )
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
fn wait_turn_closed(env: &dyn WorkflowEnv, name: &str, spawned: bool) -> Result<(), Verdict> {
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

/// How a wait ended: the verdict, and whether it settles the record. The
/// one verdict that does not (`terminal: false`) is a dispatch whose
/// answer was lost and that neither returned nor closed its turn: the
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

/// Wait for the member's return. Every poll reads the done file first — a
/// return for this dispatch wins over anything else that poll sees — then
/// the member's liveness, then its turn: `TURN_CLOSED_POLLS` consecutive
/// "no turn open" answers with no return is `no_result`. A done file for
/// another dispatch is not this run's and is ignored. A dispatch whose
/// answer was lost (`answer_lost`) is waited on the same way, and given
/// up on under `DISPATCH_UNKNOWN_SECONDS` when neither a return nor a
/// closed turn shows — with the record left pending, since the member
/// may be on the task.
fn await_return(
    env: &dyn WorkflowEnv,
    workspace: &str,
    name: &str,
    record: &WorkflowRecord,
    answer_lost: bool,
) -> Ending {
    let unobserved_polls = (DISPATCH_UNKNOWN_SECONDS / POLL_SECONDS) as u32;
    let mut closed = 0u32;
    let mut polls = 0u32;
    loop {
        if let Some(done) = read_done(workspace, name) {
            if done.dispatch_id == record.dispatch_id {
                remove_done(workspace, name);
                return Ending::terminal(Verdict::completed(done));
            }
            log(&format!(
                "{name} has a return for {} on disk, not this run's {}; ignored",
                done.dispatch_id, record.dispatch_id
            ));
        }
        if !env.alive(name) {
            return Ending::terminal(gone(name, "before it returned"));
        }
        closed = match env.turn_open(name) {
            Some(false) => closed + 1,
            _ => 0,
        };
        if closed >= TURN_CLOSED_POLLS {
            return Ending::terminal(Verdict::reason(
                STATUS_NO_RESULT,
                "the member ended its turn without hive workflow done",
            ));
        }
        polls += 1;
        if answer_lost && polls >= unobserved_polls {
            return Ending {
                verdict: Verdict::reason(
                    STATUS_AMBIGUOUS,
                    "dispatch answer lost and no return".to_string(),
                ),
                terminal: false,
            };
        }
        env.sleep(POLL_SECONDS);
    }
}

/// `hive workflow run`: the whole node as one blocking call — what an
/// external orchestrator's proxy runs in the background and reads the
/// result of. A member of that name still alive is reused (the task
/// becomes a follow-up to it); a dead roster row is retired first. A spawn
/// made here is rolled back if the run fails before the task is
/// dispatched, so the name never stays occupied by a corpse. Past the
/// dispatch every end is a verdict in the returned map, never an `Err`.
pub fn run_workflow(
    env: &dyn WorkflowEnv,
    spec: &WorkflowSpec,
) -> Result<Map<String, Value>, WorkflowError> {
    let ctx = env.context()?;
    let workspace = ctx.workspace.as_str();
    let name = spec.name.as_str();
    let busy = |reason: String| {
        let existing = read_record(workspace, name);
        let member = env.member(name).unwrap_or_default();
        WorkflowResult {
            name,
            pane: member.pane_id,
            reused: true,
            dispatch_id: existing.map(|r| r.dispatch_id).unwrap_or_default(),
            verdict: Verdict::reason(STATUS_MEMBER_BUSY, reason),
        }
        .into_map()
    };
    let Some(_lock) = try_lock(workspace, name)? else {
        return Ok(busy(format!(
            "the workflow lock for '{name}' is held by another runner"
        )));
    };
    if let Some(record) = read_record(workspace, name) {
        if record.is_pending() && env.alive(name) {
            return Ok(busy(format!(
                "member '{name}' has a pending workflow run {} owned by another runner",
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
            &WorkflowOp::Spawn {
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
            &WorkflowOp::Ready {
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
    let pre_dispatch = |verdict: Verdict| {
        rollback();
        WorkflowResult {
            name,
            pane: pane.clone(),
            reused,
            dispatch_id: dispatch_id.clone(),
            verdict,
        }
        .into_map()
    };

    if let Err(verdict) = wait_turn_closed(env, name, !reused) {
        return Ok(pre_dispatch(verdict));
    }

    // The pending record goes down before the dispatch has any side effect
    // (task artifact, hived delivery): a runner killed in between leaves
    // the name owned, never a delivered task with no record. A return left
    // on disk by an earlier run cannot be this dispatch's — the id is
    // fresh — and goes with the old record. A refused dispatch takes the
    // record back with it; one whose answer was lost keeps it, since the
    // task may be with the member.
    let mut record = WorkflowRecord {
        dispatch_id: dispatch_id.clone(),
        cli,
        status: STATUS_PENDING.to_string(),
        body: None,
        artifact: None,
        reason: None,
        seq: None,
        started_at: epoch_seconds(),
    };
    remove_done(workspace, name);
    if let Err(err) = write_record(workspace, name, &record) {
        rollback();
        return Err(err);
    }
    let dispatched = match run_op(
        env,
        &WorkflowOp::DispatchTask {
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
            "{name} dispatch answer lost ({reason}); the task may have landed, waiting for the return of {dispatch_id}"
        )),
        None => {
            record.seq = dispatched.get("seq").and_then(Value::as_i64);
            update_record(workspace, name, &record);
            log(&format!(
                "{name} dispatched {dispatch_id}; waiting for its return"
            ));
        }
    }

    let Ending { verdict, terminal } =
        await_return(env, workspace, name, &record, answer_lost.is_some());
    if terminal {
        record.status = verdict.status.to_string();
        record.body = verdict.body.clone();
        record.artifact = verdict.artifact.clone();
        record.reason = verdict.reason.clone();
        update_record(workspace, name, &record);
        log(&format!("{name} {}", verdict.status));
    } else {
        log(&format!(
            "{name} {}; the record stays pending and the name owned until `hive kill {name}`",
            verdict.status
        ));
    }
    Ok(WorkflowResult {
        name,
        pane,
        reused,
        dispatch_id,
        verdict,
    }
    .into_map())
}

// ---------------------------------------------------------------------------
// live wiring
// ---------------------------------------------------------------------------

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

/// Production `WorkflowEnv`: resolves the scoped team once and forwards
/// every seam to the team/send modules.
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

    fn with_ctx<R>(&self, f: impl FnOnce(&mut RealCtx) -> R) -> Result<R, WorkflowError> {
        let mut guard = self.ctx.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.is_none() {
            let (team_name, team) =
                crate::team::resolve_scoped_team(self.team_arg.as_deref(), true)
                    .map_err(|e| WorkflowError(e.to_string()))?;
            let team = team.ok_or_else(|| WorkflowError("no team resolved".to_string()))?;
            let workspace = crate::team::resolve_workspace(Some(&team), true)
                .map_err(|e| WorkflowError(e.to_string()))?;
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

impl WorkflowEnv for RealEnv {
    fn context(&self) -> Result<Ctx, WorkflowError> {
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
        Some(MemberInfo {
            pane_id: agent.pane_id.clone(),
            cli: agent.cli.clone(),
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
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU32, Ordering};

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

    /// The member's `hive workflow done`, scripted at a sleep count; with
    /// `dispatch_id` set, a return planted by hand for that id instead —
    /// `record_done` itself always answers the pending record's id.
    #[derive(Debug, Clone)]
    pub(crate) struct ScriptedDone {
        pub at_sleep: u32,
        pub name: String,
        pub body: String,
        pub artifact: String,
        pub dispatch_id: Option<String>,
    }

    fn next<T: Clone>(queue: &Mutex<VecDeque<T>>, default: T) -> T {
        let mut queue = queue.lock().unwrap();
        match queue.len() {
            0 => default,
            1 => queue.front().cloned().unwrap_or(default),
            _ => queue.pop_front().unwrap_or(default),
        }
    }

    /// The run records directory replaced by a plain file: every record
    /// write from here on fails (`create_dir_all` on a non-directory) and
    /// every read answers None.
    fn break_records(workspace: &str) {
        let dir = workflow_dir(workspace);
        fs::remove_dir_all(&dir).unwrap();
        fs::write(&dir, "").unwrap();
    }

    /// Failure knobs replace flaky seams; `sleep` is a no-op that counts,
    /// `die_after_sleeps` drops the member off the roster at that count,
    /// and `done` plays the member's return at one. `agents` is the
    /// roster; a member is alive iff it is there. `turn_answers` is the
    /// daemons' `turn_open` answer queue (sticky last value, empty means
    /// no answer); a fresh env scripts one bootstrap turn — open once,
    /// then closed — and `add_live` an idle member. Every turn question
    /// and sleep notes the status of the record at `record_path`, so a
    /// test can see the transitions a blocking run wrote along the way.
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
        /// Make every later record write fail once a dispatch is delivered.
        pub break_records_on_dispatch: bool,
        pub die_after_sleeps: Option<u32>,
        pub scripted_done: Mutex<Option<ScriptedDone>>,
        /// What the scripted `hive workflow done` answered.
        pub done_results: Mutex<Vec<Result<DoneRecord, String>>>,
        pub turn_answers: Mutex<VecDeque<Option<bool>>>,
        pub turn_calls: AtomicU32,
        pub spawns: Mutex<Vec<SpawnCall>>,
        pub dispatches: Mutex<Vec<DispatchCall>>,
        /// The member's run record as it stood at every dispatch attempt,
        /// delivered or refused.
        pub dispatch_records: Mutex<Vec<Option<WorkflowRecord>>>,
        pub record_path: Mutex<Option<PathBuf>>,
        pub statuses_seen: Mutex<Vec<String>>,
        pub msg_seq: AtomicU32,
        pub spawn_calls: AtomicU32,
        pub send_calls: AtomicU32,
        pub sleeps: AtomicU32,
        pub agents: Mutex<Vec<String>>,
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
            break_records_on_dispatch: false,
            die_after_sleeps: None,
            scripted_done: Mutex::new(None),
            done_results: Mutex::new(Vec::new()),
            turn_answers: Mutex::new(VecDeque::from([Some(true), Some(false)])),
            turn_calls: AtomicU32::new(0),
            spawns: Mutex::new(Vec::new()),
            dispatches: Mutex::new(Vec::new()),
            dispatch_records: Mutex::new(Vec::new()),
            record_path: Mutex::new(None),
            statuses_seen: Mutex::new(Vec::new()),
            msg_seq: AtomicU32::new(0),
            spawn_calls: AtomicU32::new(0),
            send_calls: AtomicU32::new(0),
            sleeps: AtomicU32::new(0),
            agents: Mutex::new(Vec::new()),
            retired: Mutex::new(Vec::new()),
        }
    }

    impl FakeEnv {
        /// Put a live, idle member on the roster.
        pub(crate) fn add_live(&self, name: &str) {
            self.agents.lock().unwrap().push(name.to_string());
            *self.turn_answers.lock().unwrap() = VecDeque::from([Some(false)]);
        }

        /// Script the member's return at that sleep count.
        pub(crate) fn return_at(&self, at_sleep: u32, name: &str, body: &str, artifact: &str) {
            *self.scripted_done.lock().unwrap() = Some(ScriptedDone {
                at_sleep,
                name: name.to_string(),
                body: body.to_string(),
                artifact: artifact.to_string(),
                dispatch_id: None,
            });
        }

        /// Plant a return for another dispatch at that sleep count.
        pub(crate) fn plant_return_at(&self, at_sleep: u32, name: &str, dispatch_id: &str) {
            *self.scripted_done.lock().unwrap() = Some(ScriptedDone {
                at_sleep,
                name: name.to_string(),
                body: "stale".to_string(),
                artifact: String::new(),
                dispatch_id: Some(dispatch_id.to_string()),
            });
        }

        pub(crate) fn watch_record(&self, name: &str) {
            *self.record_path.lock().unwrap() = Some(record_path(&self.workspace_str(), name));
        }

        pub(crate) fn workspace_str(&self) -> String {
            self.workspace.to_string_lossy().into_owned()
        }

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

    impl WorkflowEnv for FakeEnv {
        fn context(&self) -> Result<Ctx, WorkflowError> {
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
            })
        }

        fn turn_open(&self, name: &str) -> Option<bool> {
            self.note_status();
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

        fn sleep(&self, _seconds: f64) {
            self.note_status();
            let n = self.sleeps.fetch_add(1, Ordering::SeqCst) + 1;
            if self.die_after_sleeps == Some(n) {
                self.agents.lock().unwrap().clear();
            }
            let due = self
                .scripted_done
                .lock()
                .unwrap()
                .clone()
                .filter(|d| d.at_sleep == n);
            let Some(done) = due else {
                return;
            };
            let ws = self.workspace_str();
            let result = match done.dispatch_id {
                None => record_done(&ws, &done.name, &done.body, &done.artifact).map_err(|e| e.0),
                Some(dispatch_id) => {
                    let planted = DoneRecord {
                        dispatch_id,
                        body: done.body,
                        artifact: done.artifact,
                        done_at: n.into(),
                    };
                    write_atomic(&ws, &done_path(&ws, &done.name), &planted.to_json())
                        .map(|_| planted)
                        .map_err(|e| e.0)
                }
            };
            self.done_results.lock().unwrap().push(result);
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

    fn spawn(name: &str, cli: Option<&str>) -> WorkflowOp {
        WorkflowOp::Spawn {
            name: name.into(),
            cli: cli.map(str::to_string),
            model: String::new(),
        }
    }

    fn workflow(name: &str, cli: Option<&str>, task: &str) -> WorkflowSpec {
        WorkflowSpec {
            name: name.into(),
            cli: cli.map(str::to_string),
            model: String::new(),
            task: task.into(),
        }
    }

    fn pending(dispatch_id: &str) -> WorkflowRecord {
        WorkflowRecord {
            dispatch_id: dispatch_id.into(),
            cli: "claude".into(),
            status: STATUS_PENDING.into(),
            body: None,
            artifact: None,
            reason: None,
            seq: Some(4),
            started_at: 1,
        }
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
    fn test_ops_cover_the_whole_workflow_protocol() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());

        let r = run_op(&env, &spawn("impl", None)).unwrap();
        assert_eq!(r["pane"], "%1");
        assert_eq!(r["cli"], "claude");
        run_op(
            &env,
            &WorkflowOp::Ready {
                name: "impl".into(),
                cli: "claude".into(),
            },
        )
        .unwrap();

        let r = run_op(
            &env,
            &WorkflowOp::DispatchTask {
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
        // The dispatch id is verbatim in the body's first line and in the
        // artifact path the envelope carries.
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
            &WorkflowOp::Ready {
                name: "impl".into(),
                cli: "claude".into(),
            },
        )
        .unwrap();
        let err = run_op(
            &env,
            &WorkflowOp::Ready {
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
            &WorkflowOp::DispatchTask {
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
            &WorkflowOp::DispatchTask {
                name: "impl".into(),
                prompt: "t".into(),
                dispatch_id: "nd-000000000002".into(),
            },
        )
        .unwrap_err();
        assert!(err.0.contains("after 3 attempts"), "{err}");

        // A lost answer is not a refusal: one attempt, no seq, the reason
        // handed on for the run to wait on the return.
        let mut env = fake_env(tmp.path());
        env.lose_answer = true;
        let r = run_op(
            &env,
            &WorkflowOp::DispatchTask {
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
    fn test_run_workflow_completes_on_the_members_return() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        let text = "Findings:\n\n- one\n- two\n\n```rs\nfn x() {}\n```\nDone at /tmp/report.md";
        env.return_at(3, "audit", text, "/tmp/report.md");
        env.watch_record("audit");

        let r = run_workflow(
            &env,
            &workflow("audit", Some("codex"), "review it\nclosely"),
        )
        .unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["name"], "audit");
        assert_eq!(r["pane"], "%1");
        assert_eq!(r["reused"], false);
        assert_eq!(r["body"], text);
        assert_eq!(r["artifact"], "/tmp/report.md");
        assert!(r.get("reason").is_none());
        assert!(r.get("session").is_none());
        assert!(r.get("turn").is_none());
        let id = r["dispatchId"].as_str().unwrap();
        assert!(id.starts_with("nd-") && id.len() == 15, "{id}");

        // The return went against this very dispatch, and was consumed.
        let ws = env.workspace_str();
        let done = env.done_results.lock().unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].as_ref().unwrap().dispatch_id, id);
        assert!(!done_path(&ws, "audit").exists());
        // The injected text carries the id, and the artifact holds the task.
        let d = env.dispatches.lock().unwrap();
        assert!(d[0].body.contains(id), "{}", d[0].body);
        assert!(d[0].artifact.contains(id), "{}", d[0].artifact);
        assert_eq!(
            fs::read_to_string(&d[0].artifact).unwrap(),
            "review it\nclosely"
        );
        // The record moved pending → completed, and no closed turn was
        // counted against a run that returned.
        assert_eq!(
            *env.statuses_seen.lock().unwrap(),
            vec!["(none)", "pending"]
        );
        let record = read_record(&ws, "audit").unwrap();
        assert_eq!(record.status, "completed");
        assert_eq!(record.dispatch_id, id);
        assert_eq!(record.body.as_deref(), Some(text));
        assert_eq!(record.artifact.as_deref(), Some("/tmp/report.md"));
        assert_eq!(record.seq, Some(1));
        assert_eq!(record.cli, "codex");
        assert!(record.started_at > 0);
        assert!(!record.is_pending());
    }

    #[test]
    fn test_run_workflow_completed_with_no_artifact_is_an_empty_artifact() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.return_at(1, "audit", "done", "");
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "done");
        assert_eq!(r["artifact"], "");
    }

    #[test]
    fn test_run_workflow_is_no_result_after_five_closed_polls_with_no_return() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        // The bootstrap turn closes, the task's turn opens, then closes for
        // good; the member never returned.
        *env.turn_answers.lock().unwrap() = VecDeque::from([
            Some(true),
            Some(false),
            Some(true),
            Some(true),
            Some(false),
            Some(false),
            None,
            Some(false),
        ]);
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "no_result");
        assert_eq!(
            r["reason"],
            "the member ended its turn without hive workflow done"
        );
        assert!(r.get("body").is_none());
        // Two open polls, two closed, a no-answer that reset the count,
        // then five closed in a row.
        assert_eq!(env.sleeps.load(Ordering::SeqCst), 9);
        let record = read_record(&env.workspace_str(), "audit").unwrap();
        assert_eq!(record.status, "no_result");
        assert!(!record.is_pending());
        assert!(env.alive("audit"));
    }

    #[test]
    fn test_run_workflow_ends_as_member_gone_when_the_member_dies_waiting() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        *env.turn_answers.lock().unwrap() = VecDeque::from([Some(true), Some(false), Some(true)]);
        env.die_after_sleeps = Some(2);
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "member_gone");
        assert!(r["reason"].as_str().unwrap().contains("before it returned"));
        assert_eq!(
            read_record(&env.workspace_str(), "audit").unwrap().status,
            "member_gone"
        );

        // A return written before the death is still the result.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.die_after_sleeps = Some(1);
        env.return_at(1, "audit", "just made it", "");
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "just made it");
    }

    #[test]
    fn test_run_workflow_is_busy_on_a_live_members_pending_record() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.add_live("audit");
        let ws = env.workspace_str();
        write_record(&ws, "audit", &pending("nd-aaaaaaaaaaaa")).unwrap();
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "member_busy");
        assert_eq!(r["dispatchId"], "nd-aaaaaaaaaaaa");
        assert_eq!(r["reused"], true);
        assert_eq!(r["pane"], "%audit");
        assert!(r["reason"].as_str().unwrap().contains("nd-aaaaaaaaaaaa"));
        assert!(env.dispatches.lock().unwrap().is_empty());
        // The other runner's record is untouched.
        assert_eq!(read_record(&ws, "audit").unwrap().status, "pending");

        // A terminal record never blocks.
        let mut done = read_record(&ws, "audit").unwrap();
        done.status = STATUS_COMPLETED.into();
        write_record(&ws, "audit", &done).unwrap();
        env.return_at(1, "audit", "next", "");
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_ne!(r["dispatchId"], "nd-aaaaaaaaaaaa");
    }

    #[test]
    fn test_run_workflow_replaces_a_dead_members_stale_pending_record() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        let ws = env.workspace_str();
        write_record(&ws, "audit", &pending("nd-aaaaaaaaaaaa")).unwrap();
        // The dead member's return, never consumed, goes with its record.
        record_done(&ws, "audit", "old news", "").unwrap();
        env.return_at(2, "audit", "fresh", "");
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "fresh");
        let record = read_record(&ws, "audit").unwrap();
        assert_ne!(record.dispatch_id, "nd-aaaaaaaaaaaa");
        assert_eq!(record.dispatch_id, r["dispatchId"]);
        assert!(!done_path(&ws, "audit").exists());
    }

    #[test]
    fn test_run_workflow_ignores_a_return_for_another_dispatch() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        let ws = env.workspace_str();
        // A return with a foreign id lands after the dispatch: it is not
        // this run's, and the run ends no_result on its own closed turn.
        env.plant_return_at(1, "audit", "nd-stale0000000");
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "no_result");
        assert!(r.get("body").is_none());
        assert_eq!(env.sleeps.load(Ordering::SeqCst), 4);
        // The foreign return was left where it was.
        assert_eq!(
            read_done(&ws, "audit").map(|d| d.dispatch_id),
            Some("nd-stale0000000".to_string())
        );
    }

    #[test]
    fn test_record_done_needs_a_pending_record_and_returns_once() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws").to_string_lossy().into_owned();
        // Nothing waiting: no record at all, then a terminal one.
        let err = record_done(&ws, "audit", "x", "").unwrap_err();
        assert_eq!(err.0, "no workflow task is waiting for audit");
        let mut terminal = pending("nd-aaaaaaaaaaaa");
        terminal.status = STATUS_NO_RESULT.into();
        write_record(&ws, "audit", &terminal).unwrap();
        let err = record_done(&ws, "audit", "x", "").unwrap_err();
        assert_eq!(err.0, "no workflow task is waiting for audit");
        assert!(!done_path(&ws, "audit").exists());

        // A pending record: the return names its dispatch, in full.
        write_record(&ws, "audit", &pending("nd-aaaaaaaaaaaa")).unwrap();
        let long =
            "line one\nline two\nline three\n\n# heading\n- a very long return value ".repeat(20);
        let done = record_done(&ws, "audit", &long, "/tmp/out.md").unwrap();
        assert_eq!(done.dispatch_id, "nd-aaaaaaaaaaaa");
        assert!(done.done_at > 0);
        assert_eq!(read_done(&ws, "audit"), Some(done.clone()));
        let json: Value =
            serde_json::from_str(&fs::read_to_string(done_path(&ws, "audit")).unwrap()).unwrap();
        assert_eq!(json["dispatchId"], "nd-aaaaaaaaaaaa");
        assert_eq!(json["body"], long);
        assert_eq!(json["artifact"], "/tmp/out.md");
        assert_eq!(json["doneAt"], done.done_at);

        // Twice for the same dispatch is refused, the first return kept.
        let err = record_done(&ws, "audit", "again", "").unwrap_err();
        assert_eq!(err.0, "audit already returned for nd-aaaaaaaaaaaa");
        assert_eq!(read_done(&ws, "audit"), Some(done));

        // A new pending dispatch takes a new return over the old file.
        write_record(&ws, "audit", &pending("nd-bbbbbbbbbbbb")).unwrap();
        let next = record_done(&ws, "audit", "next", "").unwrap();
        assert_eq!(next.dispatch_id, "nd-bbbbbbbbbbbb");
        assert_eq!(read_done(&ws, "audit"), Some(next));
    }

    #[test]
    fn test_run_workflow_is_busy_when_the_member_lock_is_held() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.add_live("audit");
        let ws = env.workspace_str();
        let held = try_lock(&ws, "audit").unwrap().expect("first lock");
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "member_busy");
        assert!(r["reason"].as_str().unwrap().contains("lock"));
        assert_eq!(r["dispatchId"], "");
        assert!(env.dispatches.lock().unwrap().is_empty());
        assert!(read_record(&ws, "audit").is_none());
        drop(held);
        env.return_at(1, "audit", "now", "");
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        // The run's own lock is released with it.
        assert!(try_lock(&ws, "audit").unwrap().is_some());
    }

    #[test]
    fn test_remove_record_drops_the_record_and_return_and_keeps_the_lock_file() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.return_at(1, "audit", "x", "");
        run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        let ws = env.workspace_str();
        assert!(record_path(&ws, "audit").exists());
        assert!(lock_path(&ws, "audit").exists());
        // A return left unconsumed (the member answered a pending run that
        // nobody collected) goes with the record.
        write_record(&ws, "audit", &pending("nd-aaaaaaaaaaaa")).unwrap();
        record_done(&ws, "audit", "late", "").unwrap();
        remove_record(&ws, "audit");
        assert!(!record_path(&ws, "audit").exists());
        assert!(!done_path(&ws, "audit").exists());
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
        let record = WorkflowRecord {
            dispatch_id: "nd-0123456789ab".into(),
            cli: "grok".into(),
            status: STATUS_COMPLETED.into(),
            body: Some("done".into()),
            artifact: Some("/tmp/a.md".into()),
            reason: None,
            seq: Some(7),
            started_at: 1_700_000_000,
        };
        let json = record.to_json();
        assert_eq!(json["dispatchId"], "nd-0123456789ab");
        assert_eq!(json["artifact"], "/tmp/a.md");
        assert_eq!(json["startedAt"], 1_700_000_000u64);
        assert!(json.get("reason").is_none());
        assert!(json.get("session").is_none());
        assert_eq!(WorkflowRecord::from_json(&json), Some(record));
        let pending = WorkflowRecord {
            status: STATUS_PENDING.into(),
            body: None,
            artifact: None,
            seq: None,
            ..WorkflowRecord::from_json(&json).unwrap()
        };
        let json = pending.to_json();
        assert_eq!(json["seq"], Value::Null);
        assert!(json.get("body").is_none());
        assert_eq!(WorkflowRecord::from_json(&json), Some(pending));
    }

    #[test]
    fn test_run_workflow_reuses_a_living_member_and_retires_a_dead_row() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.add_live("audit");
        env.return_at(1, "audit", "again", "");
        let r = run_workflow(&env, &workflow("audit", None, "follow-up task")).unwrap();
        assert_eq!(r["reused"], true);
        assert_eq!(r["status"], "completed");
        // a reused member reports the pane it already sits in
        assert_eq!(r["pane"], "%audit");
        assert!(env.spawns.lock().unwrap().is_empty());
        assert_eq!(env.dispatches.lock().unwrap().len(), 1);
    }

    #[test]
    fn test_run_workflow_rolls_back_its_own_spawn_on_failure() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.ready = false; // codex hits the gate after the spawn registered
        let err = run_workflow(&env, &workflow("audit", Some("codex"), "t")).unwrap_err();
        assert!(err.0.contains("did not reach ready"), "{err}");
        assert!(!env.alive("audit"));
        assert_eq!(*env.retired.lock().unwrap(), vec!["audit".to_string()]);
        assert!(read_record(&env.workspace_str(), "audit").is_none());
    }

    #[test]
    fn test_run_workflow_does_not_retire_a_reused_member_on_failure() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.add_live("audit");
        env.dispatch_fail_first = u32::MAX;
        let err = run_workflow(&env, &workflow("audit", None, "t")).unwrap_err();
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
    fn test_run_workflow_waits_for_a_fresh_spawns_bootstrap_turn_to_close() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        // No answer yet, then a turn open (the bootstrap turn), then closed.
        *env.turn_answers.lock().unwrap() =
            VecDeque::from([None, Some(true), Some(true), Some(false)]);
        env.return_at(3, "audit", "ok", "");
        let r = run_workflow(&env, &workflow("audit", Some("codex"), "t")).unwrap();
        assert_eq!(r["status"], "completed");
        // One poll to see the turn open, one more while it stays so; the
        // dispatch went out only on the closed answer.
        let d = env.dispatches.lock().unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].sleeps, 2);
    }

    #[test]
    fn test_run_workflow_waits_for_a_reused_member_to_finish_its_turn() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.add_live("audit");
        *env.turn_answers.lock().unwrap() = VecDeque::from([Some(true), Some(true), Some(false)]);
        env.return_at(3, "audit", "ok", "");
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["reused"], true);
        let d = env.dispatches.lock().unwrap();
        assert_eq!(d[0].sleeps, 2);
    }

    #[test]
    fn test_run_workflow_is_busy_when_the_idle_wait_expires() {
        // A reused member that never closes its turn: no dispatch, no
        // record, and the member is left as it was.
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.add_live("audit");
        *env.turn_answers.lock().unwrap() = VecDeque::from([Some(true)]);
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "member_busy");
        assert_eq!(r["reason"], "turn still open after 600s");
        assert_eq!(r["reused"], true);
        assert!(env.dispatches.lock().unwrap().is_empty());
        assert!(read_record(&env.workspace_str(), "audit").is_none());
        assert!(env.alive("audit"));
        assert!(env.retired.lock().unwrap().is_empty());
        assert_eq!(
            env.sleeps.load(Ordering::SeqCst),
            (IDLE_WAIT_SECONDS / POLL_SECONDS) as u32
        );

        // A spawn of this run is rolled back on the same cap.
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        *env.turn_answers.lock().unwrap() = VecDeque::from([Some(true)]);
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "member_busy");
        assert_eq!(r["reused"], false);
        assert!(!env.alive("audit"));
        assert_eq!(*env.retired.lock().unwrap(), vec!["audit".to_string()]);
    }

    #[test]
    fn test_run_workflow_ends_as_member_gone_when_the_member_dies_before_idle() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.add_live("audit");
        *env.turn_answers.lock().unwrap() = VecDeque::from([Some(true)]);
        env.die_after_sleeps = Some(3);
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "member_gone");
        assert!(r["reason"]
            .as_str()
            .unwrap()
            .contains("before the task was dispatched"));
        assert!(env.dispatches.lock().unwrap().is_empty());
        assert!(read_record(&env.workspace_str(), "audit").is_none());
    }

    #[test]
    fn test_run_workflow_keeps_waiting_on_an_unanswered_daemon_until_the_idle_cap() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        *env.turn_answers.lock().unwrap() = VecDeque::from([None]);
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
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
    fn test_run_workflow_records_pending_before_the_dispatch_and_backfills_seq() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.return_at(1, "audit", "ok", "");
        let r = run_workflow(&env, &workflow("audit", Some("codex"), "t")).unwrap();
        assert_eq!(r["status"], "completed");
        // The record the hived saw when the dispatch reached it: already
        // pending, with everything but the seq it was about to mint.
        let at_dispatch = env.dispatch_records.lock().unwrap();
        assert_eq!(at_dispatch.len(), 1);
        let pending = at_dispatch[0].as_ref().expect("record before dispatch");
        assert_eq!(pending.status, "pending");
        assert_eq!(pending.dispatch_id, r["dispatchId"]);
        assert_eq!(pending.cli, "codex");
        assert_eq!(pending.seq, None);
        assert!(pending.started_at > 0);
        assert!(pending.is_pending());
        // The seq is filled in once the hived answered.
        let record = read_record(&env.workspace_str(), "audit").unwrap();
        assert_eq!(record.seq, Some(1));
        assert_eq!(record.started_at, pending.started_at);
    }

    #[test]
    fn test_run_workflow_takes_the_record_back_when_the_dispatch_is_refused() {
        // A reused member: every attempt saw the pending record, the
        // refusal is still an Err, and nothing of the run is left behind.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.add_live("audit");
        env.dispatch_fail_first = u32::MAX;
        let err = run_workflow(&env, &workflow("audit", None, "t")).unwrap_err();
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
        let err = run_workflow(&env, &workflow("audit", None, "t")).unwrap_err();
        assert!(err.0.contains("after 3 attempts"), "{err}");
        assert!(read_record(&env.workspace_str(), "audit").is_none());
        assert!(!env.alive("audit"));
        assert_eq!(*env.retired.lock().unwrap(), vec!["audit".to_string()]);
    }

    #[test]
    fn test_run_workflow_backfills_the_seq_of_a_dispatch_accepted_after_a_refusal() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.add_live("audit");
        env.dispatch_fail_first = 1;
        // One retry gap sleeps before the delivery; the return follows it.
        env.return_at(2, "audit", "ok", "");
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
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
    fn test_run_workflow_never_repeats_a_dispatch_whose_answer_was_lost() {
        // The hived took the task and the answer never came back: the
        // dispatch is not retried, the record stays pending with no seq,
        // and the member's return is the delivery confirmation — the run
        // then ends like any delivered dispatch.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.lose_answer = true;
        env.add_live("audit");
        env.watch_record("audit");
        *env.turn_answers.lock().unwrap() = VecDeque::from([Some(false), Some(true)]);
        env.return_at(3, "audit", "ok", "/tmp/r.md");
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "ok");
        assert_eq!(r["artifact"], "/tmp/r.md");
        assert_eq!(env.send_calls.load(Ordering::SeqCst), 1);
        assert_eq!(env.dispatches.lock().unwrap().len(), 1);
        assert_eq!(
            *env.statuses_seen.lock().unwrap(),
            vec!["(none)".to_string(), "pending".to_string()]
        );
        // No seq was ever learned, and the record says so.
        let record = read_record(&env.workspace_str(), "audit").unwrap();
        assert_eq!(record.seq, None);
        assert_eq!(record.status, "completed");
    }

    #[test]
    fn test_run_workflow_leaves_a_lost_dispatch_pending_when_nothing_shows() {
        // A spawn of this run, the answer lost, the member's turn open
        // and no return: the polls of the unknown-dispatch budget, then
        // ambiguous — with the record still pending and the member not
        // retired, since it may be on the task.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.lose_answer = true;
        *env.turn_answers.lock().unwrap() = VecDeque::from([Some(true), Some(false), Some(true)]);
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "ambiguous");
        assert_eq!(r["reused"], false);
        assert_eq!(r["reason"], "dispatch answer lost and no return");
        assert_eq!(env.send_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            env.sleeps.load(Ordering::SeqCst),
            (DISPATCH_UNKNOWN_SECONDS / POLL_SECONDS) as u32 - 1
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
        let r2 = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
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
    fn test_run_workflow_ends_a_lost_dispatch_on_a_closed_turn_or_a_death() {
        // The unknown-dispatch wait is cut short like any other: a closed
        // turn is no_result, a death is member_gone, and both are terminal.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.lose_answer = true;
        env.add_live("audit");
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "no_result");
        assert_eq!(env.send_calls.load(Ordering::SeqCst), 1);
        let record = read_record(&env.workspace_str(), "audit").unwrap();
        assert_eq!(record.status, "no_result");
        assert!(!record.is_pending());

        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.lose_answer = true;
        env.add_live("audit");
        *env.turn_answers.lock().unwrap() = VecDeque::from([Some(false), Some(true)]);
        env.die_after_sleeps = Some(5);
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "member_gone");
        let record = read_record(&env.workspace_str(), "audit").unwrap();
        assert_eq!(record.status, "member_gone");
        assert!(!record.is_pending());
    }

    #[test]
    fn test_run_workflow_keeps_its_verdict_when_the_record_fails_after_the_dispatch() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.break_records_on_dispatch = true;
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        // The task is with the member: the seq backfill and the terminal
        // write both fail, and the run still ends in its verdict, never an
        // Err. With no record on disk the member cannot return either, so
        // the closed turn ends it.
        assert_eq!(r["status"], "no_result");
        assert_eq!(env.dispatches.lock().unwrap().len(), 1);
        assert!(read_record(&env.workspace_str(), "audit").is_none());
        assert!(env.alive("audit"));
    }

    #[test]
    fn test_run_workflow_does_not_dispatch_on_a_daemon_dropout_mid_turn() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.add_live("audit");
        // Open, then no answer for a while, then closed: the dropout polls
        // never open the dispatch.
        *env.turn_answers.lock().unwrap() =
            VecDeque::from([Some(true), None, None, None, Some(false)]);
        env.return_at(5, "audit", "ok", "");
        let r = run_workflow(&env, &workflow("audit", None, "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(env.dispatches.lock().unwrap()[0].sleeps, 4);
    }

    // RealEnv over a hived on a real socket: the only place the production
    // seams are exercised end to end. The socket and the ledger are real;
    // the transport hand-off (`agent_send`) answers through the hived test
    // hook, and tmux is the fake `team/mod.rs` uses in test builds.

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

        // The roster row as the runner reads it: pane and engine.
        assert_eq!(
            env.member("b"),
            Some(MemberInfo {
                pane_id: String::new(),
                cli: "codex".to_string(),
            })
        );
        assert_eq!(env.member("nobody"), None);
        // Every row's turn is one question to the hived over the socket:
        // the codex row's thread is asked of the app-server, the grok row's
        // of the leader pool, both behind the hived's seams above.
        assert_eq!(env.turn_open("b"), Some(false));
        assert_eq!(env.turn_open("g"), Some(true));
        assert_eq!(env.turn_open("nobody"), None);

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
