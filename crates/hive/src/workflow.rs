//! hive::workflow — one task on one live member, one turn, its result read
//! off the engine.
//!
//! A workflow node is one task placed on one live member, the way a
//! Claude Code Workflow subagent takes one: spawn a real pane (or reuse a
//! live member of that name), wait until it is between turns, dispatch the
//! task as a `<HIVE>` envelope with no sender as one tracked turn, and wait
//! for that turn to end. The engine says when: codex `turn/completed` on
//! the client that started the turn, grok the `session/prompt` response
//! (ACP returns it when the turn ends) — both reach the hived's own
//! adapter client, which collected the turn's text meanwhile, and the
//! runner polls the hived's `node-result` for it. The result is the last
//! thing the member said in that turn; a member has nothing to run to
//! return, and the task is the turn: a member that stops to ask has ended
//! its turn, and that question is its result. A workflow node runs codex
//! or grok — a claude bg job reports no turn end over any RPC, and Claude
//! Code runs its own subagents natively.
//!
//! One run is one record, `<workspace>/run/workflow/<name>.json`, written
//! pending before the dispatch has any side effect and again at its end,
//! held under the per-member flock `<name>.lock`; a pending record of a
//! live member is another runner's, a pending record of a dead member is
//! stale and replaced. Past the dispatch nothing is an `Err` any more:
//! exit 1 means "not dispatched", so a record write that fails later is
//! logged and the run still ends in a verdict. A dispatch the hived
//! refused is not dispatched; one whose answer was lost may be, is never
//! repeated, and is read back like any other — the hived keeps the turn
//! under the dispatch id whether or not its answer arrived.
//!
//! `WorkflowOp` is the typed vocabulary of one hive interaction, `run_op`
//! executes one, and `run_workflow` (`hive workflow run`) strings them
//! together for an external orchestrator. `WorkflowEnv` is the seam over
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
/// Consecutive polls the hived must answer `unknown` for the dispatch
/// while the member's turn is closed before the run ends `no_result`: the
/// hived holds nothing for the turn (restarted since, or never told of
/// the dispatch) and the turn is not running, so nothing will arrive. A
/// single closed reading can be the gap before the task's turn opens.
const UNKNOWN_CLOSED_POLLS: u32 = 5;
/// Consecutive polls with no answer at all from the hived after the
/// dispatch before the waiter returns `unknown`. Execution is unresolved,
/// so the record continues to own the member.
const UNANSWERED_POLLS: u32 = 120;
const ATTEMPTS: usize = 3;
const RETRY_GAP: f64 = 3.0;

pub const STATUS_PENDING: &str = "pending";
/// The turn ended the engine's normal way (codex `completed`, grok
/// `end_turn`): `body` is the member's last message of the turn.
pub const STATUS_COMPLETED: &str = "completed";
/// The turn was cut short (codex `interrupted`, grok `cancelled`): `body`
/// is what the member had said by then.
pub const STATUS_INTERRUPTED: &str = "interrupted";
/// The engine ended the turn on an error (codex `failed`, grok an error
/// response or `max_tokens`/`refusal`/…): `reason` carries the engine's
/// word, `body` what was said.
pub const STATUS_FAILED: &str = "failed";
/// The turn is not running and nothing can read its end: the hived holds
/// no turn for the dispatch and the member is between turns.
pub const STATUS_NO_RESULT: &str = "no_result";
/// The waiter stopped receiving answers; execution remains unresolved.
pub const STATUS_UNKNOWN: &str = "unknown";
pub const STATUS_MEMBER_GONE: &str = "member_gone";
pub const STATUS_MEMBER_BUSY: &str = "member_busy";

// tmux splits and team registration race each other in-process; spawns
// serialize, everything else stays parallel. (Cross-process, the registry
// name claim inside Team::spawn is the guard.)
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// The run could not reach a dispatch (bad team, a claude member, spawn,
/// ready gate, the dispatch itself refused by the hived). Everything after
/// a dispatch — including one whose answer was lost — is a verdict in the
/// result, never an error.
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

/// The hived's `node-result` answer: the engine's own word on the turn
/// the dispatch became. `Unknown` is the hived holding nothing for the
/// dispatch (restarted since, the engine handed back no id, the adapter
/// client replaced) — not a verdict on the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeResult {
    Running,
    Ended {
        status: String,
        text: String,
        error: Option<String>,
    },
    Unknown(String),
}

impl NodeResult {
    /// The `node-result` payload as the enum; None for an error envelope
    /// or a shape that is none of the three states.
    pub fn from_answer(answer: &Map<String, Value>) -> Option<NodeResult> {
        if answer.get("ok") != Some(&Value::Bool(true)) {
            return None;
        }
        let text = |key: &str| {
            answer
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        match answer.get("state").and_then(Value::as_str) {
            Some("running") => Some(NodeResult::Running),
            Some("ended") => Some(NodeResult::Ended {
                status: text("status"),
                text: text("text"),
                error: answer
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }),
            Some("unknown") => Some(NodeResult::Unknown(text("reason"))),
            _ => None,
        }
    }
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
    fn dispatch(
        &self,
        target: &str,
        body: &str,
        artifact: &str,
        dispatch_id: &str,
    ) -> Result<i64, DispatchFailure>;
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
    /// What became of a dispatch, asked of the hived (`node-result`);
    /// None when the hived gave no answer.
    fn node_result(&self, dispatch_id: &str) -> Option<NodeResult>;
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
// run record and lock
// ---------------------------------------------------------------------------

/// One run's persisted state, `<workspace>/run/workflow/<name>.json`.
///
/// The name is owned while the execution is pending or unknown
/// (`is_pending`), even after its waiter exits. Only a terminal verdict
/// releases it; loss of the result transport is not a terminal verdict.
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
    /// by that dispatch, whether or not its runner is still waiting.
    pub fn is_pending(&self) -> bool {
        matches!(self.status.as_str(), STATUS_PENDING | STATUS_UNKNOWN)
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

fn workflow_dir(workspace: &str) -> PathBuf {
    Path::new(workspace).join("run").join("workflow")
}

pub fn record_path(workspace: &str, name: &str) -> PathBuf {
    workflow_dir(workspace).join(format!("{name}.json"))
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

/// Drop a member's run record: the member is retired (`hive kill`, `hive
/// delete --down`, a run's own dead-row retire before it spawns), so no
/// run can own it any more. The lock file stays: a flock lives on the
/// inode, so unlinking it under a runner that holds it would hand the
/// next runner a fresh file and a second lock on the same member.
pub fn remove_record(workspace: &str, name: &str) {
    let _ = fs::remove_file(record_path(workspace, name));
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
/// the member all the same, so it is not sent again, and the hived keeps
/// its turn under the dispatch id either way.
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
    dispatch_id: &str,
) -> Result<Dispatched, WorkflowError> {
    let mut last = String::new();
    for attempt in 0..ATTEMPTS {
        match env.dispatch(name, body, artifact, dispatch_id) {
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

/// A node runs codex or grok; anything else has no turn-end signal hive
/// can read, and a claude node is Claude Code's own subagent.
fn node_cli(cli: &str) -> Result<(), WorkflowError> {
    match cli {
        "codex" | "grok" => Ok(()),
        "" => Err(WorkflowError(
            "a workflow node runs codex or grok: pass --cli".to_string(),
        )),
        other => Err(WorkflowError(format!(
            "a workflow node runs codex or grok, not {other}; a claude node is Claude Code's own subagent"
        ))),
    }
}

fn ready_gate(env: &dyn WorkflowEnv, name: &str) -> Result<(), WorkflowError> {
    env.ensure_hived().map_err(WorkflowError)?;
    let not_ready = env.wait_ready(&HashSet::from([name.to_string()]));
    if !not_ready.is_empty() {
        return Err(WorkflowError(format!(
            "member '{name}' did not reach ready within the gate; inspect its pane"
        )));
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
        WorkflowOp::Ready { name } => ready_gate(env, name)?,
        WorkflowOp::DispatchTask {
            name,
            prompt,
            dispatch_id,
        } => {
            let artifact = task_artifact(env, name, dispatch_id, prompt)?;
            match dispatch(
                env,
                name,
                &dispatch_body(dispatch_id, prompt),
                &artifact,
                dispatch_id,
            )? {
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

    /// The engine's terminal word mapped onto the run's: codex
    /// `completed` and grok `end_turn` are `completed`; codex
    /// `interrupted` and grok `cancelled` are `interrupted`; everything
    /// else (`failed`, `error`, `max_tokens`, `refusal`, …) is `failed`
    /// with the engine's word and error as the reason. The text is the
    /// body in every case — what the member had said by then.
    fn ended(status: &str, text: String, error: Option<String>) -> Verdict {
        let (status, reason) = match status {
            "completed" | "end_turn" => (STATUS_COMPLETED, None),
            "interrupted" | "cancelled" => (
                STATUS_INTERRUPTED,
                Some(format!("the turn was cut short ({status})")),
            ),
            other => (
                STATUS_FAILED,
                Some(match error {
                    Some(error) => format!("the engine ended the turn: {other} ({error})"),
                    None => format!("the engine ended the turn: {other}"),
                }),
            ),
        };
        Verdict {
            status,
            body: Some(text),
            artifact: None,
            reason,
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

/// Wait for the turn to end. Every poll asks the hived for the dispatch's
/// result first — an `Ended` answer is the verdict, whatever else the poll
/// sees — then the member's liveness. `Unknown` answers are read against
/// the member's turn: `UNKNOWN_CLOSED_POLLS` consecutive unknowns with the
/// turn closed is `no_result` (nothing holds the turn and it is not
/// running); an unknown with the turn open or unanswered keeps waiting,
/// since the hived may be the one that restarted and the turn may still
/// end in front of a client that never saw it start. `UNANSWERED_POLLS`
/// consecutive polls with no answer at all returns `unknown` while
/// retaining the dispatch record and its ownership of the member. A
/// dispatch whose answer was lost is waited on exactly the same way: the
/// hived keeps the turn under the dispatch id whether or not its answer
/// arrived.
fn await_result(env: &dyn WorkflowEnv, name: &str, dispatch_id: &str) -> Verdict {
    let mut unknown_closed = 0u32;
    let mut unanswered = 0u32;
    loop {
        match env.node_result(dispatch_id) {
            Some(NodeResult::Ended {
                status,
                text,
                error,
            }) => return Verdict::ended(&status, text, error),
            Some(NodeResult::Running) => {
                unknown_closed = 0;
                unanswered = 0;
            }
            Some(NodeResult::Unknown(reason)) => {
                unanswered = 0;
                if !env.alive(name) {
                    return gone(name, "before its turn ended");
                }
                unknown_closed = match env.turn_open(name) {
                    Some(false) => unknown_closed + 1,
                    _ => 0,
                };
                if unknown_closed >= UNKNOWN_CLOSED_POLLS {
                    return Verdict::reason(
                        STATUS_NO_RESULT,
                        format!("the turn is not running and nothing holds its result ({reason})"),
                    );
                }
            }
            None => {
                unknown_closed = 0;
                unanswered += 1;
                if !env.alive(name) {
                    return gone(name, "before its turn ended");
                }
                if unanswered >= UNANSWERED_POLLS {
                    return Verdict::reason(
                        STATUS_UNKNOWN,
                        format!("the hived did not answer for {UNANSWERED_POLLS} polls"),
                    );
                }
            }
        }
        if !env.alive(name) {
            return gone(name, "before its turn ended");
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
                "member '{name}' has an unresolved workflow dispatch {}",
                record.dispatch_id
            )));
        }
    }

    let dispatch_id = mint_dispatch_id();
    let reused = env.alive(name);
    let (pane, cli) = if reused {
        let member = env.member(name).unwrap_or_default();
        node_cli(&member.cli)?;
        log(&format!("{name} alive in {}; reusing", member.pane_id));
        (member.pane_id, member.cli)
    } else {
        node_cli(spec.cli.as_deref().unwrap_or_default())?;
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
    // the name owned, never a delivered task with no record. A refused
    // dispatch takes the record back with it; one whose answer was lost
    // keeps it, since the task may be with the member.
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
    match dispatched.get("answerLost").and_then(Value::as_str) {
        Some(reason) => log(&format!(
            "{name} dispatch answer lost ({reason}); the task may have landed, waiting for the turn of {dispatch_id}"
        )),
        None => {
            record.seq = dispatched.get("seq").and_then(Value::as_i64);
            update_record(workspace, name, &record);
            log(&format!(
                "{name} dispatched {dispatch_id}; waiting for its turn to end"
            ));
        }
    }

    let verdict = await_result(env, name, &dispatch_id);
    record.status = verdict.status.to_string();
    record.body = verdict.body.clone();
    record.artifact = verdict.artifact.clone();
    record.reason = verdict.reason.clone();
    update_record(workspace, name, &record);
    log(&format!("{name} {}", verdict.status));
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

    fn dispatch(
        &self,
        target: &str,
        body: &str,
        artifact: &str,
        dispatch_id: &str,
    ) -> Result<i64, DispatchFailure> {
        self.with_ctx(|c| {
            crate::send::request_node_dispatch(
                &c.workspace,
                &c.team,
                target,
                body,
                artifact,
                dispatch_id,
            )
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

    /// One question to the hived (`node-result`): the turn it holds under
    /// the dispatch id, as its adapter client saw it end.
    fn node_result(&self, dispatch_id: &str) -> Option<NodeResult> {
        let ctx = self.context().ok()?;
        NodeResult::from_answer(&crate::hived::request_node_result(
            &ctx.workspace,
            dispatch_id,
        )?)
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
        pub dispatch_id: String,
        /// The env's sleep count when the dispatch was made.
        pub sleeps: u32,
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
    /// and `ending` plays the engine's end of the turn at one. `agents` is
    /// the roster; a member is alive iff it is there. `turn_answers` is
    /// the daemons' `turn_open` answer queue (sticky last value, empty
    /// means no answer); a fresh env scripts one bootstrap turn — open
    /// once, then closed — and `add_live` an idle member. `node_answers`
    /// is the hived's `node-result` answer queue the same way (sticky
    /// last; a fresh env answers `Running`), overridden by `ending` once
    /// its sleep count is reached. Every turn question and sleep notes the
    /// status of the record at `record_path`, so a test can see the
    /// transitions a blocking run wrote along the way.
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
        /// The engine's end of the turn, from that sleep count on.
        pub ending: Mutex<Option<(u32, NodeResult)>>,
        pub node_answers: Mutex<VecDeque<Option<NodeResult>>>,
        pub node_calls: Mutex<Vec<String>>,
        pub turn_answers: Mutex<VecDeque<Option<bool>>>,
        pub turn_calls: AtomicU32,
        /// The engine a reused member reports.
        pub member_cli: Mutex<String>,
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
            ending: Mutex::new(None),
            node_answers: Mutex::new(VecDeque::from([Some(NodeResult::Running)])),
            node_calls: Mutex::new(Vec::new()),
            turn_answers: Mutex::new(VecDeque::from([Some(true), Some(false)])),
            turn_calls: AtomicU32::new(0),
            member_cli: Mutex::new("codex".to_string()),
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

    pub(crate) fn ended(status: &str, text: &str) -> NodeResult {
        NodeResult::Ended {
            status: status.to_string(),
            text: text.to_string(),
            error: None,
        }
    }

    impl FakeEnv {
        /// Put a live, idle member on the roster.
        pub(crate) fn add_live(&self, name: &str) {
            self.agents.lock().unwrap().push(name.to_string());
            *self.turn_answers.lock().unwrap() = VecDeque::from([Some(false)]);
        }

        /// Script the engine's end of the turn at that sleep count: codex
        /// `completed` with that text.
        pub(crate) fn end_at(&self, at_sleep: u32, text: &str) {
            self.end_with_at(at_sleep, ended("completed", text));
        }

        pub(crate) fn end_with_at(&self, at_sleep: u32, result: NodeResult) {
            *self.ending.lock().unwrap() = Some((at_sleep, result));
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
                cli: cli.unwrap_or("codex").to_string(),
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
            dispatch_id: &str,
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
                dispatch_id: dispatch_id.to_string(),
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
                cli: self.member_cli.lock().unwrap().clone(),
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

        fn node_result(&self, dispatch_id: &str) -> Option<NodeResult> {
            self.node_calls
                .lock()
                .unwrap()
                .push(dispatch_id.to_string());
            let sleeps = self.sleeps.load(Ordering::SeqCst);
            if let Some((at, result)) = self.ending.lock().unwrap().clone() {
                if sleeps >= at {
                    return Some(result);
                }
            }
            next(&self.node_answers, None)
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

    /// A codex node spec: the cli every spawn here needs.
    fn codex(name: &str, task: &str) -> WorkflowSpec {
        workflow(name, Some("codex"), task)
    }

    fn pending(dispatch_id: &str) -> WorkflowRecord {
        WorkflowRecord {
            dispatch_id: dispatch_id.into(),
            cli: "codex".into(),
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
    fn test_node_result_from_answer_reads_the_three_states() {
        let answer = |pairs: &[(&str, Value)]| {
            Map::from_iter(pairs.iter().map(|(k, v)| (k.to_string(), v.clone())))
        };
        assert_eq!(
            NodeResult::from_answer(&answer(&[
                ("ok", Value::Bool(true)),
                ("state", Value::from("running")),
            ])),
            Some(NodeResult::Running)
        );
        assert_eq!(
            NodeResult::from_answer(&answer(&[
                ("ok", Value::Bool(true)),
                ("state", Value::from("ended")),
                ("status", Value::from("failed")),
                ("text", Value::from("half")),
                ("error", Value::from("boom")),
            ])),
            Some(NodeResult::Ended {
                status: "failed".into(),
                text: "half".into(),
                error: Some("boom".into()),
            })
        );
        assert_eq!(
            NodeResult::from_answer(&answer(&[
                ("ok", Value::Bool(true)),
                ("state", Value::from("ended")),
                ("status", Value::from("end_turn")),
                ("text", Value::from("done")),
                ("error", Value::Null),
            ])),
            Some(ended("end_turn", "done"))
        );
        assert_eq!(
            NodeResult::from_answer(&answer(&[
                ("ok", Value::Bool(true)),
                ("state", Value::from("unknown")),
                ("reason", Value::from("restarted")),
            ])),
            Some(NodeResult::Unknown("restarted".into()))
        );
        // An error envelope or an unknown shape is no answer.
        assert_eq!(
            NodeResult::from_answer(&answer(&[
                ("ok", Value::Bool(false)),
                ("error", Value::from("unknown action")),
            ])),
            None
        );
        assert_eq!(
            NodeResult::from_answer(&answer(&[
                ("ok", Value::Bool(true)),
                ("state", Value::from("lost")),
            ])),
            None
        );
    }

    #[test]
    fn test_ops_cover_the_whole_workflow_protocol() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());

        let r = run_op(&env, &spawn("impl", Some("grok"))).unwrap();
        assert_eq!(r["pane"], "%1");
        assert_eq!(r["cli"], "grok");
        run_op(
            &env,
            &WorkflowOp::Ready {
                name: "impl".into(),
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
        // The dispatch id is verbatim in the body's first line, in the
        // artifact path the envelope carries, and on the dispatch itself
        // — the hived holds the turn under it.
        assert_eq!(d[0].body, "task nd-0123456789ab\nexplore auth");
        assert!(d[0].artifact.contains("nd-0123456789ab"));
        assert_eq!(d[0].dispatch_id, "nd-0123456789ab");
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
    fn test_ready_gates_every_node() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.ready = false;
        let err = run_op(
            &env,
            &WorkflowOp::Ready {
                name: "impl".into(),
            },
        )
        .unwrap_err();
        assert!(err.0.contains("did not reach ready"), "{err}");
        env.ready = true;
        run_op(
            &env,
            &WorkflowOp::Ready {
                name: "impl".into(),
            },
        )
        .unwrap();
    }

    #[test]
    fn test_node_cli_is_codex_or_grok() {
        node_cli("codex").unwrap();
        node_cli("grok").unwrap();
        let err = node_cli("").unwrap_err();
        assert!(err.0.contains("pass --cli"), "{err}");
        let err = node_cli("claude").unwrap_err();
        assert!(err.0.contains("Claude Code's own subagent"), "{err}");
        assert!(node_cli("bash").is_err());
    }

    #[test]
    fn test_run_workflow_refuses_a_claude_node_before_anything_happens() {
        // A spawn asked for claude, or none: nothing is spawned.
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        let err = run_workflow(&env, &workflow("audit", Some("claude"), "t")).unwrap_err();
        assert!(err.0.contains("Claude Code's own subagent"), "{err}");
        let err = run_workflow(&env, &workflow("audit", None, "t")).unwrap_err();
        assert!(err.0.contains("pass --cli"), "{err}");
        assert!(env.spawns.lock().unwrap().is_empty());
        assert!(env.dispatches.lock().unwrap().is_empty());
        assert!(read_record(&env.workspace_str(), "audit").is_none());

        // A live claude member of that name is not reused as a node, and
        // is left as it is.
        let env = fake_env(tmp.path());
        env.add_live("audit");
        *env.member_cli.lock().unwrap() = "claude".to_string();
        let err = run_workflow(&env, &codex("audit", "t")).unwrap_err();
        assert!(err.0.contains("runs codex or grok, not claude"), "{err}");
        assert!(env.alive("audit"));
        assert!(env.retired.lock().unwrap().is_empty());
        assert!(env.dispatches.lock().unwrap().is_empty());
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
        // handed on for the run to wait on the turn.
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
    fn test_run_workflow_completes_when_the_engine_ends_the_turn() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        let text = "Findings:\n\n- one\n- two\n\n```rs\nfn x() {}\n```\nDone at /tmp/report.md";
        env.end_at(3, text);
        env.watch_record("audit");

        let r = run_workflow(&env, &codex("audit", "review it\nclosely")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["name"], "audit");
        assert_eq!(r["pane"], "%1");
        assert_eq!(r["reused"], false);
        assert_eq!(r["body"], text);
        assert!(r.get("artifact").is_none());
        assert!(r.get("reason").is_none());
        let id = r["dispatchId"].as_str().unwrap();
        assert!(id.starts_with("nd-") && id.len() == 15, "{id}");

        // The hived was asked about this very dispatch, every poll.
        let asked = env.node_calls.lock().unwrap();
        assert!(!asked.is_empty());
        assert!(asked.iter().all(|d| d == id), "{asked:?}");
        // The injected text carries the id, and the artifact holds the task.
        let d = env.dispatches.lock().unwrap();
        assert!(d[0].body.contains(id), "{}", d[0].body);
        assert!(d[0].artifact.contains(id), "{}", d[0].artifact);
        assert_eq!(d[0].dispatch_id, id);
        assert_eq!(
            fs::read_to_string(&d[0].artifact).unwrap(),
            "review it\nclosely"
        );
        // The record moved pending → completed; no turn question was
        // needed once the dispatch was out and the hived answered.
        assert_eq!(
            *env.statuses_seen.lock().unwrap(),
            vec!["(none)", "pending"]
        );
        let ws = env.workspace_str();
        let record = read_record(&ws, "audit").unwrap();
        assert_eq!(record.status, "completed");
        assert_eq!(record.dispatch_id, id);
        assert_eq!(record.body.as_deref(), Some(text));
        assert_eq!(record.artifact, None);
        assert_eq!(record.seq, Some(1));
        assert_eq!(record.cli, "codex");
        assert!(record.started_at > 0);
        assert!(!record.is_pending());
    }

    #[test]
    fn test_run_workflow_maps_each_engines_terminal_word() {
        let run = |result: NodeResult| {
            let tmp = TempDir::new().unwrap();
            let env = fake_env(tmp.path());
            env.end_with_at(1, result);
            let r = run_workflow(&env, &codex("audit", "t")).unwrap();
            let record = read_record(&env.workspace_str(), "audit").unwrap();
            assert_eq!(record.status, r["status"]);
            assert_eq!(record.body.as_deref(), r["body"].as_str());
            assert_eq!(
                record.reason.as_deref(),
                r.get("reason").and_then(Value::as_str)
            );
            r
        };
        // grok's normal end is completed too.
        let r = run(ended("end_turn", "SAGE_FINAL"));
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "SAGE_FINAL");
        assert!(r.get("reason").is_none());

        // Cut short: what was said by then, and why.
        let r = run(ended("interrupted", "half"));
        assert_eq!(r["status"], "interrupted");
        assert_eq!(r["body"], "half");
        assert_eq!(r["reason"], "the turn was cut short (interrupted)");
        let r = run(ended("cancelled", ""));
        assert_eq!(r["status"], "interrupted");
        assert_eq!(r["body"], "");
        assert_eq!(r["reason"], "the turn was cut short (cancelled)");

        // The engine's error is the reason, its word first.
        let r = run(NodeResult::Ended {
            status: "failed".into(),
            text: "so far".into(),
            error: Some("context window exceeded".into()),
        });
        assert_eq!(r["status"], "failed");
        assert_eq!(r["body"], "so far");
        assert_eq!(
            r["reason"],
            "the engine ended the turn: failed (context window exceeded)"
        );
        let r = run(ended("max_tokens", "…"));
        assert_eq!(r["status"], "failed");
        assert_eq!(r["reason"], "the engine ended the turn: max_tokens");
        let r = run(NodeResult::Ended {
            status: "error".into(),
            text: String::new(),
            error: Some("closed".into()),
        });
        assert_eq!(r["status"], "failed");
        assert_eq!(r["reason"], "the engine ended the turn: error (closed)");
    }

    #[test]
    fn test_run_workflow_is_no_result_when_nothing_holds_the_turn_and_it_is_closed() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.watch_record("audit");
        // The hived holds nothing for the dispatch (restarted since); the
        // bootstrap turn closes, the task's turn opens, then closes for good.
        *env.node_answers.lock().unwrap() =
            VecDeque::from([Some(NodeResult::Unknown("restarted".into()))]);
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
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
        assert_eq!(r["status"], "no_result");
        assert_eq!(
            r["reason"],
            "the turn is not running and nothing holds its result (restarted)"
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
    fn test_run_workflow_keeps_waiting_through_unknowns_while_the_turn_runs() {
        // The hived answers unknown while the turn is open (a client that
        // never saw it start), then the turn's end arrives: completed.
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.add_live("audit");
        *env.node_answers.lock().unwrap() = VecDeque::from([
            Some(NodeResult::Unknown("no client".into())),
            Some(NodeResult::Unknown("no client".into())),
            Some(NodeResult::Unknown("no client".into())),
            Some(NodeResult::Running),
            Some(ended("completed", "late but here")),
        ]);
        *env.turn_answers.lock().unwrap() = VecDeque::from([Some(false), Some(true)]);
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "late but here");
        assert_eq!(env.sleeps.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn test_run_workflow_keeps_ownership_when_the_hived_never_answers() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.add_live("audit");
        *env.turn_answers.lock().unwrap() = VecDeque::from([Some(false), Some(true)]);
        *env.node_answers.lock().unwrap() = VecDeque::from([None]);
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
        assert_eq!(r["status"], "unknown");
        assert_eq!(
            r["reason"],
            format!("the hived did not answer for {UNANSWERED_POLLS} polls")
        );
        assert_eq!(env.sleeps.load(Ordering::SeqCst), UNANSWERED_POLLS - 1);
        let record = read_record(&env.workspace_str(), "audit").unwrap();
        assert_eq!(record.status, "unknown");
        assert!(record.is_pending());
        assert_eq!(env.turn_open("audit"), Some(true));
        assert_eq!(record.dispatch_id, r["dispatchId"]);
        // Dropping the waiter lock does not release an unresolved dispatch.
        let again = run_workflow(&env, &codex("audit", "another task")).unwrap();
        assert_eq!(again["status"], "member_busy");
        assert_eq!(env.dispatches.lock().unwrap().len(), 1);
        assert_eq!(read_record(&env.workspace_str(), "audit"), Some(record));

        // An answer in between resets the count.
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.add_live("audit");
        let mut answers: VecDeque<Option<NodeResult>> =
            (0..UNANSWERED_POLLS - 1).map(|_| None).collect();
        answers.push_back(Some(NodeResult::Running));
        answers.push_back(None);
        *env.node_answers.lock().unwrap() = answers;
        env.end_at(UNANSWERED_POLLS + 10, "eventually");
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "eventually");
    }

    #[test]
    fn test_run_workflow_ends_as_member_gone_when_the_member_dies_waiting() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.die_after_sleeps = Some(3);
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
        assert_eq!(r["status"], "member_gone");
        assert!(r["reason"]
            .as_str()
            .unwrap()
            .contains("before its turn ended"));
        assert_eq!(
            read_record(&env.workspace_str(), "audit").unwrap().status,
            "member_gone"
        );

        // A death under unknown answers is member_gone too, not no_result.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.add_live("audit");
        *env.node_answers.lock().unwrap() =
            VecDeque::from([Some(NodeResult::Unknown("restarted".into()))]);
        *env.turn_answers.lock().unwrap() = VecDeque::from([Some(false), Some(true)]);
        env.die_after_sleeps = Some(2);
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
        assert_eq!(r["status"], "member_gone");

        // The turn's end read on the poll the member dies is still the
        // result.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.die_after_sleeps = Some(1);
        env.end_at(1, "just made it");
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
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
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
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
        env.end_at(1, "next");
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_ne!(r["dispatchId"], "nd-aaaaaaaaaaaa");
    }

    #[test]
    fn test_run_workflow_replaces_a_dead_members_stale_pending_record() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        let ws = env.workspace_str();
        write_record(&ws, "audit", &pending("nd-aaaaaaaaaaaa")).unwrap();
        env.end_at(2, "fresh");
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "fresh");
        let record = read_record(&ws, "audit").unwrap();
        assert_ne!(record.dispatch_id, "nd-aaaaaaaaaaaa");
        assert_eq!(record.dispatch_id, r["dispatchId"]);
    }

    #[test]
    fn test_run_workflow_is_busy_when_the_member_lock_is_held() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.add_live("audit");
        let ws = env.workspace_str();
        let held = try_lock(&ws, "audit").unwrap().expect("first lock");
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
        assert_eq!(r["status"], "member_busy");
        assert!(r["reason"].as_str().unwrap().contains("lock"));
        assert_eq!(r["dispatchId"], "");
        assert!(env.dispatches.lock().unwrap().is_empty());
        assert!(read_record(&ws, "audit").is_none());
        drop(held);
        env.end_at(1, "now");
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
        assert_eq!(r["status"], "completed");
        // The run's own lock is released with it.
        assert!(try_lock(&ws, "audit").unwrap().is_some());
    }

    #[test]
    fn test_remove_record_drops_the_record_and_keeps_the_lock_file() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.end_at(1, "x");
        run_workflow(&env, &codex("audit", "t")).unwrap();
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
        env.end_at(1, "again");
        // A reused member keeps its own engine; the spec's cli is not
        // consulted.
        let r = run_workflow(&env, &workflow("audit", None, "follow-up task")).unwrap();
        assert_eq!(r["reused"], true);
        assert_eq!(r["status"], "completed");
        // a reused member reports the pane it already sits in
        assert_eq!(r["pane"], "%audit");
        assert!(env.spawns.lock().unwrap().is_empty());
        assert_eq!(env.dispatches.lock().unwrap().len(), 1);
        assert_eq!(
            read_record(&env.workspace_str(), "audit").unwrap().cli,
            "codex"
        );
    }

    #[test]
    fn test_run_workflow_rolls_back_its_own_spawn_on_failure() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.ready = false; // the gate after the spawn registered
        let err = run_workflow(&env, &codex("audit", "t")).unwrap_err();
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
        let err = run_workflow(&env, &codex("audit", "t")).unwrap_err();
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
        env.end_at(3, "ok");
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
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
        env.end_at(3, "ok");
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
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
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
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
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
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
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
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
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
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
        env.end_at(1, "ok");
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
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
        let err = run_workflow(&env, &codex("audit", "t")).unwrap_err();
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
        let err = run_workflow(&env, &codex("audit", "t")).unwrap_err();
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
        // One retry gap sleeps before the delivery; the end follows it.
        env.end_at(2, "ok");
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
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
        // and the hived's own word on the turn — held under the dispatch
        // id whether or not its answer arrived — ends the run like any
        // delivered dispatch.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.lose_answer = true;
        env.add_live("audit");
        env.watch_record("audit");
        env.end_at(3, "ok");
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "ok");
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

        // The answer lost and the hived holding nothing (it never got the
        // dispatch, or restarted): no_result once the turn is closed, and
        // the name is free again.
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.lose_answer = true;
        env.add_live("audit");
        *env.node_answers.lock().unwrap() =
            VecDeque::from([Some(NodeResult::Unknown("nothing held".into()))]);
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
        assert_eq!(r["status"], "no_result");
        assert_eq!(env.send_calls.load(Ordering::SeqCst), 1);
        let record = read_record(&env.workspace_str(), "audit").unwrap();
        assert_eq!(record.status, "no_result");
        assert!(!record.is_pending());
        env.end_at(0, "second");
        *env.node_answers.lock().unwrap() = VecDeque::from([Some(NodeResult::Running)]);
        let r2 = run_workflow(&env, &codex("audit", "t")).unwrap();
        assert_eq!(r2["status"], "completed");
        assert_ne!(r2["dispatchId"], r["dispatchId"]);
    }

    #[test]
    fn test_run_workflow_keeps_its_verdict_when_the_record_fails_after_the_dispatch() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.break_records_on_dispatch = true;
        env.end_at(2, "done anyway");
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
        // The task is with the member: the seq backfill and the terminal
        // write both fail, and the run still ends in its verdict, never an
        // Err.
        assert_eq!(r["status"], "completed");
        assert_eq!(r["body"], "done anyway");
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
        env.end_at(5, "ok");
        let r = run_workflow(&env, &codex("audit", "t")).unwrap();
        assert_eq!(r["status"], "completed");
        assert_eq!(env.dispatches.lock().unwrap()[0].sleeps, 4);
    }

    // RealEnv over a hived on a real socket: the only place the production
    // seams are exercised end to end. The socket and the ledger are real;
    // the transport hand-off (`agent_dispatch_turn`) and the engine's word
    // on the turn (`cas_turn_result`) answer through the hived test hook,
    // and tmux is the fake `team/mod.rs` uses in test builds.

    #[test]
    fn test_real_env_dispatches_without_a_sender_and_reads_the_turn_back() {
        use crate::adapters::codex_app_server::TurnResult;
        use crate::agent::TurnHandle;
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
        // itself and hands the envelope to the hooked transport as one
        // tracked turn; node-result reads that turn back.
        let handed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let handed_sink = Arc::clone(&handed);
        let turn_ended = Arc::new(Mutex::new(false));
        let turn_ended_hook = Arc::clone(&turn_ended);
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
            agent_dispatch_turn: Some(Arc::new(move |_agent, text| {
                handed_sink.lock().unwrap().push(text.to_string());
                Ok(TurnHandle::Codex {
                    thread_id: "thr-1".to_string(),
                    turn_id: "turn-9".to_string(),
                })
            })),
            // The turn as the shared codex client saw it: running until
            // the test flips it, then completed with its messages.
            cas_turn_result: Some(Arc::new(move |turn_id| {
                assert_eq!(turn_id, "turn-9");
                let ended = *turn_ended_hook.lock().unwrap();
                Some(TurnResult {
                    thread_id: "thr-1".to_string(),
                    status: ended.then(|| "completed".to_string()),
                    error: None,
                    messages: vec!["looking".to_string(), "done: /tmp/out.md".to_string()],
                })
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

        // Before any dispatch the hived holds nothing for the id.
        assert_eq!(
            env.node_result("nd-0123456789ab"),
            Some(NodeResult::Unknown(
                "this hived holds no turn for dispatch nd-0123456789ab".to_string()
            ))
        );

        let artifact = format!("{workspace}/artifacts/tasks/b-nd-0123456789ab.md");
        let seq = env
            .dispatch(
                "b",
                "task nd-0123456789ab\nfirst task",
                &artifact,
                "nd-0123456789ab",
            )
            .unwrap();
        assert!(seq > 0);
        let events = crate::bus::read_all_events(&workspace).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, seq);
        // No sender on the ledger row, no `from` on the envelope.
        assert_eq!(events[0].from, "");
        assert_eq!(events[0].to, "b");
        assert_eq!(events[0].body, "task nd-0123456789ab\nfirst task");
        assert_eq!(events[0].artifact, artifact);
        {
            let handed = handed.lock().unwrap();
            assert_eq!(handed.len(), 1);
            assert_eq!(
                handed[0],
                format!(
                    "<HIVE to=b artifact={artifact}>\ntask nd-0123456789ab\nfirst task\n</HIVE>"
                )
            );
        }

        // The turn is read back under the dispatch id: running, then the
        // engine's end with the last message as the text.
        assert_eq!(
            env.node_result("nd-0123456789ab"),
            Some(NodeResult::Running)
        );
        *turn_ended.lock().unwrap() = true;
        assert_eq!(
            env.node_result("nd-0123456789ab"),
            Some(ended("completed", "done: /tmp/out.md"))
        );

        assert!(
            real_tmux_argv.borrow().is_empty(),
            "real tmux reached: {:?}",
            real_tmux_argv.borrow()
        );

        let uncertain_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = uncertain_calls.clone();
        crate::hived::testhook::update(|h| {
            h.agent_dispatch_turn = Some(Arc::new(move |_agent, _text| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(TurnHandle::Unknown("turn/start timed out".to_string()))
            }));
        });
        let result = dispatch(&env, "b", "task", "", "nd-abcdef012345").unwrap();
        assert!(matches!(result, Dispatched::AnswerLost(reason) if reason.contains("timed out")));
        assert_eq!(uncertain_calls.load(Ordering::SeqCst), 1);
        assert!(
            matches!(env.node_result("nd-abcdef012345"), Some(NodeResult::Unknown(reason)) if reason.contains("may have taken"))
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
