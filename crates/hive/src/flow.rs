//! hive::flow — deterministic orchestration over live members.
//!
//! A flow node is one task placed on one live member: spawn a real pane,
//! wait until it is ready, dispatch the task as its first `<HIVE>` message,
//! block until the member replies. The runner never owns a pane: it sends
//! as the reserved `flow.run` address (the hived's mailbox branch keeps the
//! durable bus row) and reads replies straight off the bus; members answer
//! with an ordinary `hive send flow.run`.
//!
//! This module is the op core. `FlowOp` is the typed vocabulary both
//! consumers speak: the JavaScript engine (`flow_script`) journals one op at
//! a time so a resumed run replays per op, and `hive flow node run`
//! (`run_node`) strings the same ops together for an external orchestrator.
//! `FlowEnv` is the seam over cli/bus/team; tests inject a fake.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::bus::Event;

pub const FLOW_SENDER: &str = "flow.run";
const REPLY_POLL_SECONDS: f64 = 2.0;
const ATTEMPTS: usize = 3;
const RETRY_GAP: f64 = 3.0;

// tmux splits and team registration race each other in-process; spawns
// serialize, everything else stays parallel. (Cross-process, the registry
// name claim inside Team::spawn is the guard.)
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// A flow step failed loudly: spawn, ready gate, dispatch, or a member that
/// died before replying.
#[derive(Debug)]
pub struct FlowError(pub String);

impl fmt::Display for FlowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FlowError {}

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

/// The seams flow reaches through. `Err(String)` from `spawn`/`send` is a
/// transient failure the retry loops absorb.
pub trait FlowEnv: Send + Sync {
    fn context(&self) -> Result<Ctx, FlowError>;
    /// Spawn a member pane; `group` lands on the pane's `hive-group` tag
    /// (the phase, for the board).
    fn spawn(
        &self,
        name: &str,
        cli: Option<&str>,
        model: &str,
        group: &str,
    ) -> Result<SpawnedAgent, String>;
    fn ensure_hived(&self);
    /// Agents still not ready when the gate expires.
    fn wait_ready(&self, agents: &HashSet<String>) -> HashSet<String>;
    /// Send as `flow.run`; returns the msgId.
    fn send(&self, target: &str, body: &str, artifact: &str) -> Result<String, String>;
    fn find_reply(&self, msg_id: &str, from: &str) -> Result<Option<Event>, FlowError>;
    /// Runtime liveness (`Team::member_alive`): can this member still take a
    /// dispatch and answer.
    fn alive(&self, name: &str) -> bool;
    /// `Team::retire`: no-op when the member is not on the roster.
    fn retire(&self, name: &str);
    fn sleep(&self, seconds: f64);
}

/// Progress goes to stderr so stdout carries only results (the JSON line
/// of `hive flow node run`, the return value of `hive flow run`).
fn log(message: &str) {
    eprintln!("[flow] {message}");
    let _ = std::io::stderr().flush();
}

// ---------------------------------------------------------------------------
// ops
// ---------------------------------------------------------------------------

/// One hive interaction. The serialized form (`serde_json::to_string`) is
/// the journal key: Rust field order, not whatever the script built — so
/// keep every field free of run-relative values (paths with counters,
/// timestamps).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum FlowOp {
    Spawn {
        name: String,
        #[serde(default)]
        cli: Option<String>,
        #[serde(default)]
        model: String,
        #[serde(default)]
        group: String,
    },
    Ready {
        name: String,
        cli: String,
    },
    /// First task: the prompt rides a task artifact, the body is the same
    /// atomic skeleton as `hive spawn --task`.
    DispatchTask {
        name: String,
        prompt: String,
    },
    /// Follow-up: short single-line prompts ride the body, anything longer
    /// rides an artifact.
    DispatchAsk {
        name: String,
        prompt: String,
    },
    WaitReply {
        name: String,
        msg_id: String,
    },
    Kill {
        name: String,
    },
}

impl FlowOp {
    pub fn member(&self) -> &str {
        match self {
            FlowOp::Spawn { name, .. }
            | FlowOp::Ready { name, .. }
            | FlowOp::DispatchTask { name, .. }
            | FlowOp::DispatchAsk { name, .. }
            | FlowOp::WaitReply { name, .. }
            | FlowOp::Kill { name } => name,
        }
    }

    /// Kill always runs live: it is cheap and idempotent, and replaying it
    /// would hide a member someone revived between runs.
    pub fn journaled(&self) -> bool {
        !matches!(self, FlowOp::Kill { .. })
    }

    pub fn is_spawn(&self) -> bool {
        matches!(self, FlowOp::Spawn { .. })
    }

    pub fn key(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

fn task_artifact(env: &dyn FlowEnv, name: &str, text: &str) -> Result<String, FlowError> {
    let ctx = env.context()?;
    let tasks_dir = Path::new(&ctx.workspace).join("artifacts").join("tasks");
    fs::create_dir_all(&tasks_dir).map_err(|e| FlowError(e.to_string()))?;
    let mut path = tasks_dir.join(format!("{name}.md"));
    let mut counter = 1u64;
    while path.exists() {
        counter += 1;
        path = tasks_dir.join(format!("{name}-{counter}.md"));
    }
    fs::write(&path, text).map_err(|e| FlowError(e.to_string()))?;
    Ok(path.to_string_lossy().into_owned())
}

/// Send with bounded retries: a cloud-backed transport can refuse
/// transiently under provider throttling, and a single blip must not kill
/// a whole orchestration. Still loud on exhaustion.
fn dispatch(
    env: &dyn FlowEnv,
    name: &str,
    body: &str,
    artifact: &str,
) -> Result<String, FlowError> {
    let mut last = String::new();
    for attempt in 0..ATTEMPTS {
        match env.send(name, body, artifact) {
            Ok(msg_id) => return Ok(msg_id),
            Err(exc) => {
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
    Err(FlowError(format!(
        "dispatch to '{name}' failed after {ATTEMPTS} attempts: {last}"
    )))
}

/// Block until `name`'s reply anchored to `msg_id` lands on the bus, or the
/// member dies first — a dead member's reply never comes, so that is a
/// terminal error, not a longer wait. No other timeout by design: the
/// members are visible panes and the human is the supervisor.
fn await_reply(env: &dyn FlowEnv, name: &str, msg_id: &str) -> Result<Event, FlowError> {
    loop {
        // Reply first: a member that replied and then retired still
        // delivered.
        if let Some(row) = env.find_reply(msg_id, name)? {
            return Ok(row);
        }
        if !env.alive(name) {
            return Err(FlowError(format!(
                "member '{name}' is gone without replying; this dispatch will never resolve"
            )));
        }
        env.sleep(REPLY_POLL_SECONDS);
    }
}

fn spawn_member(
    env: &dyn FlowEnv,
    name: &str,
    cli: Option<&str>,
    model: &str,
    group: &str,
) -> Result<SpawnedAgent, FlowError> {
    if name == "flow" || name.starts_with("flow.") {
        return Err(FlowError(format!(
            "'{name}' collides with the flow runner's mailbox address kind ({FLOW_SENDER}); pick another member name"
        )));
    }
    let mut last = String::new();
    for attempt in 0..ATTEMPTS {
        let result = {
            let _guard = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
            env.spawn(name, cli, model, group)
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
    Err(FlowError(format!(
        "spawn '{name}' failed after {ATTEMPTS} attempts: {last}"
    )))
}

fn ready_gate(env: &dyn FlowEnv, name: &str, cli: &str) -> Result<(), FlowError> {
    env.ensure_hived();
    if cli != "claude" {
        // claude inboxes queue; only TUI-injected CLIs need the ready gate.
        let not_ready = env.wait_ready(&HashSet::from([name.to_string()]));
        if !not_ready.is_empty() {
            return Err(FlowError(format!(
                "member '{name}' did not reach ready within the gate; inspect its pane"
            )));
        }
    }
    Ok(())
}

fn reply_map(row: Event) -> Map<String, Value> {
    let mut result = Map::new();
    result.insert("body".to_string(), Value::String(row.body));
    result.insert("artifact".to_string(), Value::String(row.artifact));
    result.insert("msgId".to_string(), Value::String(row.msg_id));
    result
}

/// Execute one op against the live seams.
pub fn run_op(env: &dyn FlowEnv, op: &FlowOp) -> Result<Map<String, Value>, FlowError> {
    let mut result = Map::new();
    match op {
        FlowOp::Spawn {
            name,
            cli,
            model,
            group,
        } => {
            let spawned = spawn_member(env, name, cli.as_deref(), model, group)?;
            result.insert("pane".to_string(), Value::String(spawned.pane_id));
            result.insert("cli".to_string(), Value::String(spawned.cli));
        }
        FlowOp::Ready { name, cli } => ready_gate(env, name, cli)?,
        FlowOp::DispatchTask { name, prompt } => {
            let artifact = task_artifact(env, name, prompt)?;
            let artifact_name = Path::new(&artifact)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let msg_id = dispatch(
                env,
                name,
                &format!(
                    "flow-mailbox dispatch: {artifact_name} (not a member; hive send flow.run, then stop)"
                ),
                &artifact,
            )?;
            result.insert("msgId".to_string(), Value::String(msg_id));
            result.insert("artifact".to_string(), Value::String(artifact));
        }
        FlowOp::DispatchAsk { name, prompt } => {
            // A follow-up needs the member's context; a retired or dead
            // member has none to answer from.
            if !env.alive(name) {
                return Err(FlowError(format!(
                    "member '{name}' is gone; nothing to ask (spawn it again with agent())"
                )));
            }
            let (body, artifact) = if prompt.contains('\n') || prompt.chars().count() > 200 {
                let artifact = task_artifact(env, &format!("{name}-ask"), prompt)?;
                ("follow-up: see artifact".to_string(), artifact)
            } else {
                (prompt.clone(), String::new())
            };
            let msg_id = dispatch(env, name, &body, &artifact)?;
            result.insert("msgId".to_string(), Value::String(msg_id));
        }
        FlowOp::WaitReply { name, msg_id } => {
            result = reply_map(await_reply(env, name, msg_id)?);
        }
        FlowOp::Kill { name } => env.retire(name),
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
    pub phase: String,
    pub task: String,
}

/// `hive flow node run`: the whole node as one blocking call — what an
/// external orchestrator's proxy runs in the background and reads the
/// result of. A member of that name still alive is reused (the task becomes
/// a follow-up to it, same as a resumed script); a dead roster row is
/// retired first. A spawn made here is rolled back if the node fails before
/// the task is dispatched, so the name never stays occupied by a corpse.
pub fn run_node(env: &dyn FlowEnv, spec: &NodeSpec) -> Result<Map<String, Value>, FlowError> {
    let reused = env.alive(&spec.name);
    let mut pane = String::new();
    if reused {
        log(&format!("{} alive; reusing", spec.name));
    } else {
        env.retire(&spec.name);
        let spawned = run_op(
            env,
            &FlowOp::Spawn {
                name: spec.name.clone(),
                cli: spec.cli.clone(),
                model: spec.model.clone(),
                group: spec.phase.clone(),
            },
        )?;
        pane = spawned
            .get("pane")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let cli = spawned
            .get("cli")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        log(&format!("{} spawned in {pane}", spec.name));
        if let Err(err) = run_op(
            env,
            &FlowOp::Ready {
                name: spec.name.clone(),
                cli,
            },
        ) {
            env.retire(&spec.name);
            return Err(err);
        }
    }
    let dispatched = match run_op(
        env,
        &FlowOp::DispatchTask {
            name: spec.name.clone(),
            prompt: spec.task.clone(),
        },
    ) {
        Ok(d) => d,
        Err(err) => {
            if !reused {
                env.retire(&spec.name);
            }
            return Err(err);
        }
    };
    let msg_id = dispatched
        .get("msgId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    log(&format!(
        "{} dispatched ({msg_id}); waiting for reply",
        spec.name
    ));
    let mut result = reply_map(await_reply(env, &spec.name, &msg_id)?);
    result.insert("status".to_string(), Value::String("replied".into()));
    result.insert("name".to_string(), Value::String(spec.name.clone()));
    result.insert("pane".to_string(), Value::String(pane));
    result.insert("reused".to_string(), Value::Bool(reused));
    Ok(result)
}

// ---------------------------------------------------------------------------
// live wiring
// ---------------------------------------------------------------------------

struct RealCtx {
    team_name: String,
    workspace: String,
    team: crate::team::Team,
}

/// Production `FlowEnv`: resolves the scoped team once and forwards every
/// seam to the cli/bus/team modules.
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

    fn with_ctx<R>(&self, f: impl FnOnce(&mut RealCtx) -> R) -> Result<R, FlowError> {
        let mut guard = self.ctx.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.is_none() {
            let (team_name, team) = crate::cli::resolve_scoped_team(self.team_arg.as_deref(), true)
                .map_err(|e| FlowError(e.to_string()))?;
            let team = team.ok_or_else(|| FlowError("no team resolved".to_string()))?;
            let workspace = crate::cli::resolve_workspace(Some(&team), true)
                .map_err(|e| FlowError(e.to_string()))?;
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

impl FlowEnv for RealEnv {
    fn context(&self) -> Result<Ctx, FlowError> {
        self.with_ctx(|c| Ctx {
            team_name: c.team_name.clone(),
            workspace: c.workspace.clone(),
        })
    }

    fn spawn(
        &self,
        name: &str,
        cli: Option<&str>,
        model: &str,
        group: &str,
    ) -> Result<SpawnedAgent, String> {
        self.with_ctx(|c| {
            let team_name = c.team_name.clone();
            crate::cli::spawn_team_agent(
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
            .map(|a| {
                if !group.is_empty() {
                    crate::tmux::set_pane_option(&a.pane_id, "hive-group", group);
                }
                SpawnedAgent {
                    pane_id: a.pane_id.clone(),
                    cli: a.cli.clone(),
                }
            })
            .map_err(|e| e.to_string())
        })
        .map_err(|e| e.0)
        .and_then(|inner| inner)
    }

    fn ensure_hived(&self) {
        let _ = self.with_ctx(|c| {
            crate::cli::ensure_team_hived(&c.team, Path::new(&c.workspace));
        });
    }

    fn wait_ready(&self, agents: &HashSet<String>) -> HashSet<String> {
        match self.context() {
            Ok(ctx) => {
                crate::cli::wait_for_peer_ready(&ctx.workspace, &ctx.team_name, agents, 30.0, 0.5)
            }
            Err(_) => agents.clone(),
        }
    }

    fn send(&self, target: &str, body: &str, artifact: &str) -> Result<String, String> {
        self.with_ctx(|c| {
            crate::cli::request_send_payload(
                &c.workspace,
                &c.team,
                FLOW_SENDER,
                target,
                body,
                artifact,
                "",
                "flow-dispatch",
                false,
            )
            .map(|payload| match payload.get("msgId") {
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            })
            .map_err(|e| e.to_string())
        })
        .map_err(|e| e.0)
        .and_then(|inner| inner)
    }

    fn find_reply(&self, msg_id: &str, from: &str) -> Result<Option<Event>, FlowError> {
        let ctx = self.context()?;
        crate::bus::find_reply_to(&ctx.workspace, msg_id, from)
            .map_err(|e| FlowError(e.to_string()))
    }

    fn alive(&self, name: &str) -> bool {
        self.fresh_team()
            .map(|t| t.member_alive(name))
            .unwrap_or(false)
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
// shared test env (this module's op tests and the engine tests)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_env {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Debug)]
    pub(crate) struct SpawnCall {
        pub name: String,
        pub cli: Option<String>,
        pub group: String,
    }

    #[derive(Debug)]
    pub(crate) struct DispatchCall {
        pub target: String,
        pub body: String,
        pub artifact: String,
    }

    /// Failure knobs replace flaky seams; `sleep` is a no-op. `agents` is
    /// the roster; a member is alive iff it is there.
    pub(crate) struct FakeEnv {
        pub workspace: PathBuf,
        pub ready: bool,
        pub reply_any: bool,
        pub spawn_fail_first: u32,
        pub dispatch_fail_first: u32,
        pub spawn_err: String,
        pub dispatch_err: String,
        pub spawns: Mutex<Vec<SpawnCall>>,
        pub dispatches: Mutex<Vec<DispatchCall>>,
        pub awaits: Mutex<Vec<(String, String)>>,
        pub replies: Mutex<HashMap<String, Event>>,
        pub msg_seq: AtomicU32,
        pub spawn_calls: AtomicU32,
        pub send_calls: AtomicU32,
        pub agents: Mutex<Vec<String>>,
        pub retired: Mutex<Vec<String>>,
    }

    pub(crate) fn fake_env(tmp: &Path) -> FakeEnv {
        let ws = tmp.join("ws");
        fs::create_dir_all(&ws).unwrap();
        FakeEnv {
            workspace: ws,
            ready: true,
            reply_any: false,
            spawn_fail_first: 0,
            dispatch_fail_first: 0,
            spawn_err: "mint refused".to_string(),
            dispatch_err: "refused".to_string(),
            spawns: Mutex::new(Vec::new()),
            dispatches: Mutex::new(Vec::new()),
            awaits: Mutex::new(Vec::new()),
            replies: Mutex::new(HashMap::new()),
            msg_seq: AtomicU32::new(0),
            spawn_calls: AtomicU32::new(0),
            send_calls: AtomicU32::new(0),
            agents: Mutex::new(Vec::new()),
            retired: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn reply_row(body: &str, artifact: &str, msg_id: &str) -> Event {
        Event {
            from: String::new(),
            to: String::new(),
            intent: "send".to_string(),
            metadata: Map::new(),
            created_at: String::new(),
            msg_id: msg_id.to_string(),
            in_reply_to: String::new(),
            body: body.to_string(),
            artifact: artifact.to_string(),
        }
    }

    impl FlowEnv for FakeEnv {
        fn context(&self) -> Result<Ctx, FlowError> {
            Ok(Ctx {
                team_name: "t-x".to_string(),
                workspace: self.workspace.to_string_lossy().into_owned(),
            })
        }

        fn spawn(
            &self,
            name: &str,
            cli: Option<&str>,
            _model: &str,
            group: &str,
        ) -> Result<SpawnedAgent, String> {
            let n = self.spawn_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= self.spawn_fail_first {
                return Err(self.spawn_err.clone());
            }
            let mut spawns = self.spawns.lock().unwrap();
            spawns.push(SpawnCall {
                name: name.to_string(),
                cli: cli.map(str::to_string),
                group: group.to_string(),
            });
            self.agents.lock().unwrap().push(name.to_string());
            Ok(SpawnedAgent {
                pane_id: format!("%{}", spawns.len()),
                cli: cli.unwrap_or("claude").to_string(),
            })
        }

        fn ensure_hived(&self) {}

        fn wait_ready(&self, agents: &HashSet<String>) -> HashSet<String> {
            if self.ready {
                HashSet::new()
            } else {
                agents.clone()
            }
        }

        fn send(&self, target: &str, body: &str, artifact: &str) -> Result<String, String> {
            let n = self.send_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= self.dispatch_fail_first {
                return Err(self.dispatch_err.clone());
            }
            let msg_id = format!("m{}", self.msg_seq.fetch_add(1, Ordering::SeqCst) + 1);
            self.dispatches.lock().unwrap().push(DispatchCall {
                target: target.to_string(),
                body: body.to_string(),
                artifact: artifact.to_string(),
            });
            Ok(msg_id)
        }

        fn find_reply(&self, msg_id: &str, from: &str) -> Result<Option<Event>, FlowError> {
            self.awaits
                .lock()
                .unwrap()
                .push((from.to_string(), msg_id.to_string()));
            if self.reply_any {
                return Ok(Some(reply_row(
                    &format!("done-{msg_id}"),
                    "",
                    &format!("r-{msg_id}"),
                )));
            }
            Ok(self.replies.lock().unwrap().get(msg_id).cloned())
        }

        fn alive(&self, name: &str) -> bool {
            self.agents.lock().unwrap().iter().any(|a| a == name)
        }

        fn retire(&self, name: &str) {
            let mut agents = self.agents.lock().unwrap();
            if let Some(pos) = agents.iter().position(|a| a == name) {
                agents.remove(pos);
                self.retired.lock().unwrap().push(name.to_string());
            }
        }

        fn sleep(&self, _seconds: f64) {}
    }
}

#[cfg(test)]
mod tests {
    use super::test_env::*;
    use super::*;
    use std::sync::atomic::Ordering;
    use tempfile::TempDir;

    fn spawn(name: &str, cli: Option<&str>) -> FlowOp {
        FlowOp::Spawn {
            name: name.into(),
            cli: cli.map(str::to_string),
            model: String::new(),
            group: String::new(),
        }
    }

    #[test]
    fn test_op_keys_are_canonical_and_tagged() {
        let op = spawn("impl", Some("codex"));
        let key = op.key();
        assert!(key.starts_with("{\"op\":\"spawn\""), "{key}");
        // the wire form the script sends round-trips to the same key
        let parsed: FlowOp =
            serde_json::from_str(r#"{"model":"","name":"impl","op":"spawn","cli":"codex"}"#)
                .unwrap();
        assert_eq!(parsed.key(), key);
        assert!(!FlowOp::Kill { name: "x".into() }.journaled());
        assert_eq!(
            FlowOp::WaitReply {
                name: "w".into(),
                msg_id: "m".into()
            }
            .member(),
            "w"
        );
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
            &FlowOp::Ready {
                name: "impl".into(),
                cli: "claude".into(),
            },
        )
        .unwrap();

        let r = run_op(
            &env,
            &FlowOp::DispatchTask {
                name: "impl".into(),
                prompt: "explore auth\nwrite findings".into(),
            },
        )
        .unwrap();
        assert_eq!(r["msgId"], "m1");
        let artifact = r["artifact"].as_str().unwrap();
        assert_eq!(
            fs::read_to_string(artifact).unwrap(),
            "explore auth\nwrite findings"
        );
        {
            let d = env.dispatches.lock().unwrap();
            assert_eq!(d[0].target, "impl");
            assert_eq!(d[0].artifact, artifact);
            assert!(d[0].body.starts_with("flow-mailbox dispatch: "));
        }

        env.replies
            .lock()
            .unwrap()
            .insert("m1".into(), reply_row("done", "/tmp/f.md", "r1"));
        let r = run_op(
            &env,
            &FlowOp::WaitReply {
                name: "impl".into(),
                msg_id: "m1".into(),
            },
        )
        .unwrap();
        assert_eq!(r["body"], "done");
        assert_eq!(r["artifact"], "/tmp/f.md");
        assert_eq!(r["msgId"], "r1");
        // the wait is scoped to the member
        assert_eq!(
            *env.awaits.lock().unwrap(),
            vec![("impl".to_string(), "m1".to_string())]
        );

        run_op(
            &env,
            &FlowOp::Kill {
                name: "impl".into(),
            },
        )
        .unwrap();
        assert!(!env.alive("impl"));
        assert_eq!(*env.retired.lock().unwrap(), vec!["impl".to_string()]);
    }

    #[test]
    fn test_dispatch_ask_short_rides_body_long_rides_artifact() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        // a follow-up to a member that is not alive is refused up front
        let err = run_op(
            &env,
            &FlowOp::DispatchAsk {
                name: "impl".into(),
                prompt: "hello?".into(),
            },
        )
        .unwrap_err();
        assert!(err.0.contains("gone"), "{err}");
        assert!(env.dispatches.lock().unwrap().is_empty());
        env.agents.lock().unwrap().push("impl".to_string());
        run_op(
            &env,
            &FlowOp::DispatchAsk {
                name: "impl".into(),
                prompt: "rework: null case".into(),
            },
        )
        .unwrap();
        run_op(
            &env,
            &FlowOp::DispatchAsk {
                name: "impl".into(),
                prompt: "line one\nline two of a long rework".into(),
            },
        )
        .unwrap();
        let d = env.dispatches.lock().unwrap();
        assert_eq!(d[0].body, "rework: null case");
        assert_eq!(d[0].artifact, "");
        assert_eq!(d[1].body, "follow-up: see artifact");
        assert!(fs::read_to_string(&d[1].artifact)
            .unwrap()
            .starts_with("line one"));
    }

    #[test]
    fn test_wait_reply_is_terminal_when_the_member_is_gone() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        // not on the roster, no reply: gone, not an eternal poll
        let err = run_op(
            &env,
            &FlowOp::WaitReply {
                name: "impl".into(),
                msg_id: "m9".into(),
            },
        )
        .unwrap_err();
        assert!(err.0.contains("gone without replying"), "{err}");
        // replied then retired still delivers
        env.replies
            .lock()
            .unwrap()
            .insert("m9".into(), reply_row("late", "", "r9"));
        let r = run_op(
            &env,
            &FlowOp::WaitReply {
                name: "impl".into(),
                msg_id: "m9".into(),
            },
        )
        .unwrap();
        assert_eq!(r["body"], "late");
    }

    #[test]
    fn test_ready_gates_non_claude_and_skips_claude() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.ready = false;
        run_op(
            &env,
            &FlowOp::Ready {
                name: "impl".into(),
                cli: "claude".into(),
            },
        )
        .unwrap();
        let err = run_op(
            &env,
            &FlowOp::Ready {
                name: "impl".into(),
                cli: "codex".into(),
            },
        )
        .unwrap_err();
        assert!(err.0.contains("did not reach ready"), "{err}");
    }

    #[test]
    fn test_spawn_rejects_the_mailbox_name_family() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        for name in ["flow", "flow.run", "flow.anything"] {
            let err = run_op(&env, &spawn(name, None)).unwrap_err();
            assert!(err.0.contains("mailbox address kind"), "{err}");
        }
        assert!(env.spawns.lock().unwrap().is_empty());
    }

    #[test]
    fn test_spawn_and_dispatch_retry_transient_failures_then_stay_loud() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.spawn_fail_first = 1;
        env.dispatch_fail_first = 1;
        run_op(&env, &spawn("impl", Some("codex"))).unwrap();
        assert_eq!(env.spawn_calls.load(Ordering::SeqCst), 2);
        run_op(
            &env,
            &FlowOp::DispatchTask {
                name: "impl".into(),
                prompt: "t".into(),
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
            &FlowOp::DispatchTask {
                name: "impl".into(),
                prompt: "t".into(),
            },
        )
        .unwrap_err();
        assert!(err.0.contains("after 3 attempts"), "{err}");
    }

    #[test]
    fn test_spawn_carries_the_phase_as_the_pane_group() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        run_op(
            &env,
            &FlowOp::Spawn {
                name: "a".into(),
                cli: None,
                model: String::new(),
                group: "Review".into(),
            },
        )
        .unwrap();
        assert_eq!(env.spawns.lock().unwrap()[0].group, "Review");
    }

    #[test]
    fn test_task_artifact_never_clobbers() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        let p1 = task_artifact(&env, "explore", "one").unwrap();
        let p2 = task_artifact(&env, "explore", "two").unwrap();
        assert_ne!(p1, p2);
        assert_eq!(fs::read_to_string(&p1).unwrap(), "one");
        assert_eq!(fs::read_to_string(&p2).unwrap(), "two");
    }

    fn node(name: &str, cli: Option<&str>, task: &str) -> NodeSpec {
        NodeSpec {
            name: name.into(),
            cli: cli.map(str::to_string),
            model: String::new(),
            phase: "Review".into(),
            task: task.into(),
        }
    }

    #[test]
    fn test_run_node_spawns_dispatches_and_returns_the_reply() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.reply_any = true;
        let r = run_node(&env, &node("audit", Some("codex"), "review it\nclosely")).unwrap();
        assert_eq!(r["status"], "replied");
        assert_eq!(r["name"], "audit");
        assert_eq!(r["pane"], "%1");
        assert_eq!(r["reused"], false);
        assert!(r["body"].as_str().unwrap().starts_with("done-"));
        assert_eq!(env.spawns.lock().unwrap()[0].group, "Review");
        let d = env.dispatches.lock().unwrap();
        assert_eq!(
            fs::read_to_string(&d[0].artifact).unwrap(),
            "review it\nclosely"
        );
    }

    #[test]
    fn test_run_node_reuses_a_living_member_and_retires_a_dead_row() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.reply_any = true;
        env.agents.lock().unwrap().push("audit".to_string());
        let r = run_node(&env, &node("audit", None, "follow-up task")).unwrap();
        assert_eq!(r["reused"], true);
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
    }

    #[test]
    fn test_run_node_does_not_retire_a_reused_member_on_failure() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.agents.lock().unwrap().push("audit".to_string());
        env.dispatch_fail_first = u32::MAX;
        let err = run_node(&env, &node("audit", None, "t")).unwrap_err();
        assert!(err.0.contains("after 3 attempts"), "{err}");
        assert!(env.alive("audit"));
    }
}
