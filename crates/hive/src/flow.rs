//! hive::flow — one task on one live member, as one blocking call.
//!
//! A node is one task placed on one live member: spawn a real pane, wait
//! until it is ready, dispatch the task as its first `<HIVE>` message, block
//! until the member replies. The runner never owns a pane: it sends as the
//! reserved `flow.run` address (the hived's mailbox branch keeps the durable
//! bus row) and reads replies straight off the bus; members answer with an
//! ordinary `hive send flow.run`.
//!
//! `FlowOp` is the typed vocabulary of one hive interaction, `run_op`
//! executes one, and `run_node` (`hive node run`) strings them together for
//! an external orchestrator — a Claude Code Workflow through the `hive-node`
//! plugin agent. `FlowEnv` is the seam over cli/bus/team; tests inject a
//! fake.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use serde_json::{Map, Value};

use crate::bus::Event;

pub const FLOW_SENDER: &str = "flow.run";
/// Body prefix of a task dispatch: the member sees the mailbox, not a peer,
/// asked it.
pub const DISPATCH_BODY_PREFIX: &str = "flow-mailbox dispatch: ";
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
    fn spawn(&self, name: &str, cli: Option<&str>, model: &str) -> Result<SpawnedAgent, String>;
    fn ensure_hived(&self);
    /// Agents still not ready when the gate expires.
    fn wait_ready(&self, agents: &HashSet<String>) -> HashSet<String>;
    /// Send as `flow.run`; returns the bus seq of the dispatch row.
    fn send(&self, target: &str, body: &str, artifact: &str) -> Result<i64, String>;
    /// The first `from` → `flow.run` row after `seq`: the reply, by order.
    fn find_reply(&self, seq: i64, from: &str) -> Result<Option<Event>, FlowError>;
    /// Runtime liveness (`Team::member_alive`): can this member still take a
    /// dispatch and answer.
    fn alive(&self, name: &str) -> bool;
    /// Pane of a roster member ("" when it has none).
    fn pane_of(&self, name: &str) -> String;
    /// `Team::retire`: no-op when the member is not on the roster.
    fn retire(&self, name: &str);
    fn sleep(&self, seconds: f64);
}

/// Progress goes to stderr so stdout carries only the result (the JSON
/// line of `hive node run`).
fn log(message: &str) {
    eprintln!("[flow] {message}");
    let _ = std::io::stderr().flush();
}

// ---------------------------------------------------------------------------
// ops
// ---------------------------------------------------------------------------

/// One hive interaction.
#[derive(Debug, Clone, PartialEq)]
pub enum FlowOp {
    Spawn {
        name: String,
        cli: Option<String>,
        model: String,
    },
    Ready {
        name: String,
        cli: String,
    },
    /// The task: the prompt rides a task artifact, the body is the same
    /// atomic skeleton as `hive spawn --task`.
    DispatchTask {
        name: String,
        prompt: String,
    },
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
fn dispatch(env: &dyn FlowEnv, name: &str, body: &str, artifact: &str) -> Result<i64, FlowError> {
    let mut last = String::new();
    for attempt in 0..ATTEMPTS {
        match env.send(name, body, artifact) {
            Ok(seq) => return Ok(seq),
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

/// Block until `name`'s reply — its first send to the mailbox after the
/// dispatch at `seq`; the runner is serial per member, so order is the
/// link — lands on the bus, or the member dies first — a dead member's reply never comes, so that is a
/// terminal error, not a longer wait. No other timeout by design: the
/// members are visible panes and the human is the supervisor.
fn await_reply(env: &dyn FlowEnv, name: &str, seq: i64) -> Result<Event, FlowError> {
    loop {
        // Reply first: a member that replied and then retired still
        // delivered.
        if let Some(row) = env.find_reply(seq, name)? {
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
) -> Result<SpawnedAgent, FlowError> {
    if name == "flow" || name.starts_with("flow.") {
        return Err(FlowError(format!(
            "'{name}' collides with the node runner's mailbox address kind ({FLOW_SENDER}); pick another member name"
        )));
    }
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
    result
}

/// Execute one op against the live seams.
pub fn run_op(env: &dyn FlowEnv, op: &FlowOp) -> Result<Map<String, Value>, FlowError> {
    let mut result = Map::new();
    match op {
        FlowOp::Spawn { name, cli, model } => {
            let spawned = spawn_member(env, name, cli.as_deref(), model)?;
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
            let seq = dispatch(
                env,
                name,
                &format!(
                    "{DISPATCH_BODY_PREFIX}{artifact_name} (not a member; hive send flow.run, then stop)"
                ),
                &artifact,
            )?;
            result.insert("seq".to_string(), Value::from(seq));
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

/// `hive node run`: the whole node as one blocking call — what an external
/// orchestrator's proxy runs in the background and reads the result of. A
/// member of that name still alive is reused (the task becomes a follow-up
/// to it); a dead roster row is retired first. A spawn made here is rolled back if the node fails before
/// the task is dispatched, so the name never stays occupied by a corpse.
pub fn run_node(env: &dyn FlowEnv, spec: &NodeSpec) -> Result<Map<String, Value>, FlowError> {
    let reused = env.alive(&spec.name);
    let pane = if reused {
        let pane = env.pane_of(&spec.name);
        log(&format!("{} alive in {pane}; reusing", spec.name));
        pane
    } else {
        env.retire(&spec.name);
        let spawned = run_op(
            env,
            &FlowOp::Spawn {
                name: spec.name.clone(),
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
        pane
    };
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
    let seq = dispatched.get("seq").and_then(Value::as_i64).unwrap_or(0);
    log(&format!("{} dispatched; waiting for reply", spec.name));
    let mut result = reply_map(await_reply(env, &spec.name, seq)?);
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
/// seam to the team/send/bus modules.
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
            let (team_name, team) =
                crate::team::resolve_scoped_team(self.team_arg.as_deref(), true)
                    .map_err(|e| FlowError(e.to_string()))?;
            let team = team.ok_or_else(|| FlowError("no team resolved".to_string()))?;
            let workspace = crate::team::resolve_workspace(Some(&team), true)
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

    fn ensure_hived(&self) {
        let _ = self.with_ctx(|c| {
            crate::team::ensure_team_hived(&c.team, Path::new(&c.workspace));
        });
    }

    fn wait_ready(&self, agents: &HashSet<String>) -> HashSet<String> {
        match self.context() {
            Ok(ctx) => {
                crate::send::wait_for_peer_ready(&ctx.workspace, &ctx.team_name, agents, 30.0, 0.5)
            }
            Err(_) => agents.clone(),
        }
    }

    fn send(&self, target: &str, body: &str, artifact: &str) -> Result<i64, String> {
        self.with_ctx(|c| {
            crate::send::request_send_payload(
                &c.workspace,
                &c.team,
                FLOW_SENDER,
                target,
                body,
                artifact,
                "flow-dispatch",
                false,
            )
            .map(|payload| payload.get("seq").and_then(Value::as_i64).unwrap_or(0))
            .map_err(|e| e.to_string())
        })
        .map_err(|e| e.0)
        .and_then(|inner| inner)
    }

    fn find_reply(&self, seq: i64, from: &str) -> Result<Option<Event>, FlowError> {
        let ctx = self.context()?;
        crate::bus::first_send_after(&ctx.workspace, seq, from, FLOW_SENDER)
            .map_err(|e| FlowError(e.to_string()))
    }

    fn alive(&self, name: &str) -> bool {
        self.fresh_team()
            .map(|t| t.member_alive(name))
            .unwrap_or(false)
    }

    fn pane_of(&self, name: &str) -> String {
        self.fresh_team()
            .and_then(|t| t.agent_named(name).map(|a| a.pane_id.clone()))
            .unwrap_or_default()
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
    use std::collections::HashMap;
    use std::path::PathBuf;
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
        pub awaits: Mutex<Vec<(String, i64)>>,
        pub replies: Mutex<HashMap<i64, Event>>,
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

    pub(crate) fn reply_row(body: &str, artifact: &str, seq: i64) -> Event {
        Event {
            seq,
            from: String::new(),
            to: String::new(),
            created_at: String::new(),
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

        fn ensure_hived(&self) {}

        fn wait_ready(&self, agents: &HashSet<String>) -> HashSet<String> {
            if self.ready {
                HashSet::new()
            } else {
                agents.clone()
            }
        }

        fn send(&self, target: &str, body: &str, artifact: &str) -> Result<i64, String> {
            let n = self.send_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= self.dispatch_fail_first {
                return Err(self.dispatch_err.clone());
            }
            let seq = i64::from(self.msg_seq.fetch_add(1, Ordering::SeqCst) + 1);
            self.dispatches.lock().unwrap().push(DispatchCall {
                target: target.to_string(),
                body: body.to_string(),
                artifact: artifact.to_string(),
            });
            Ok(seq)
        }

        fn find_reply(&self, seq: i64, from: &str) -> Result<Option<Event>, FlowError> {
            self.awaits.lock().unwrap().push((from.to_string(), seq));
            if self.reply_any {
                return Ok(Some(reply_row(&format!("done-{seq}"), "", seq + 100)));
            }
            Ok(self.replies.lock().unwrap().get(&seq).cloned())
        }

        fn alive(&self, name: &str) -> bool {
            self.agents.lock().unwrap().iter().any(|a| a == name)
        }

        fn pane_of(&self, name: &str) -> String {
            if self.alive(name) {
                format!("%{name}")
            } else {
                String::new()
            }
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
        }
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
        assert_eq!(r["seq"], 1);
        let artifact = r["artifact"].as_str().unwrap();
        assert_eq!(
            fs::read_to_string(artifact).unwrap(),
            "explore auth\nwrite findings"
        );
        {
            let d = env.dispatches.lock().unwrap();
            assert_eq!(d[0].target, "impl");
            assert_eq!(d[0].artifact, artifact);
            assert!(d[0].body.starts_with(DISPATCH_BODY_PREFIX));
        }

        env.replies
            .lock()
            .unwrap()
            .insert(1, reply_row("done", "/tmp/f.md", 2));
        let r = reply_map(await_reply(&env, "impl", 1).unwrap());
        assert_eq!(r["body"], "done");
        assert_eq!(r["artifact"], "/tmp/f.md");
        assert!(r.get("msgId").is_none());
        // the wait is scoped to the member
        assert_eq!(*env.awaits.lock().unwrap(), vec![("impl".to_string(), 1)]);
    }

    #[test]
    fn test_wait_reply_is_terminal_when_the_member_is_gone() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        // not on the roster, no reply: gone, not an eternal poll
        let err = await_reply(&env, "impl", 9).unwrap_err();
        assert!(err.0.contains("gone without replying"), "{err}");
        // replied then retired still delivers
        env.replies
            .lock()
            .unwrap()
            .insert(9, reply_row("late", "", 10));
        let r = reply_map(await_reply(&env, "impl", 9).unwrap());
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
        {
            let spawns = env.spawns.lock().unwrap();
            assert_eq!(spawns[0].name, "impl");
            assert_eq!(spawns[0].cli.as_deref(), Some("codex"));
        }
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
        assert_eq!(env.spawns.lock().unwrap()[0].name, "audit");
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
        // a reused member reports the pane it already sits in
        assert!(
            r["pane"].as_str().is_some_and(|p| p.starts_with('%')),
            "{r:?}"
        );
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

    // -- RealEnv over the live wiring ------------------------------------------
    //
    // The production env resolves the team from the registry, asks the hived
    // over its socket, and reads replies off the bus. Everything below is
    // real except what needs a live member: the hived's member lookup
    // (`resolve_live_agent`), send gate (`check_send_gate`) and transport
    // hand-off (`agent_send`) answer through the hived test hook, and tmux is
    // the fake `team/mod.rs` uses in test builds.

    #[test]
    fn test_real_env_send_and_find_reply_follow_bus_order() {
        use crate::hived::testhook::Hook as HivedHook;
        use crate::hived::HivedServerApi;
        use std::sync::{Arc, Mutex};

        let mut env = crate::testenv::EnvGuard::new();
        let home = TempDir::new().unwrap();
        env.set("HIVE_HOME", home.path().join(".hive"));
        // A short workspace path keeps the hived socket in-tree; an overlong
        // one is relocated under /tmp/hive-<uid>/ and would outlive the TempDir.
        let ws_tmp = tempfile::Builder::new()
            .prefix("hive-fl-")
            .tempdir_in("/tmp")
            .unwrap();
        let workspace = ws_tmp.path().to_string_lossy().to_string();
        crate::bus::init_workspace(&workspace).unwrap();

        // The registry row RealEnv::for_team resolves; the fake tmux answers
        // list-windows with the window that claims it, so the team loads
        // with its window identity and ensure_hived never asks tmux.
        let team = "flowt";
        let member = Map::from_iter([
            ("name".to_string(), Value::from("b")),
            ("cli".to_string(), Value::from("claude")),
        ]);
        assert_eq!(
            crate::registry::record_team(team, &workspace, "1700000000", &[member], "dev:1")
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

        // A hived on a real socket: the send arm writes the bus row itself
        // and hands the envelope to the hooked transport.
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
                    crate::agent::testhook::fake_agent(agent, team_name, "%9", "claude"),
                ))
            })),
            check_send_gate: Some(Arc::new(|_target| Ok(()))),
            agent_send: Some(Arc::new(move |_agent, text, sender| {
                handed_sink
                    .lock()
                    .unwrap()
                    .push((text.to_string(), sender.to_string()));
                Ok("udsWriteAccepted".to_string())
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

        let first = env.send("b", "first task", "/art/1").unwrap();
        let second = env.send("b", "second task", "/art/2").unwrap();
        assert!(first > 0);
        assert_ne!(first, second);
        let events = crate::bus::read_all_events(&workspace).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, first);
        assert_eq!(events[0].from, FLOW_SENDER);
        assert_eq!(events[0].to, "b");
        assert_eq!(events[0].body, "first task");
        assert_eq!(events[0].artifact, "/art/1");
        assert_eq!(events[1].seq, second);
        {
            let handed = handed.lock().unwrap();
            assert_eq!(handed.len(), 2);
            // The envelope carries sender, body and artifact — and no id.
            assert!(handed[0].0.contains("first task"), "{}", handed[0].0);
            assert!(!handed[0].0.contains("msgId"), "{}", handed[0].0);
            assert_eq!(handed[0].1, FLOW_SENDER);
        }

        // No reply yet: neither dispatch resolves.
        assert!(env.find_reply(first, "b").unwrap().is_none());
        assert!(env.find_reply(second, "b").unwrap().is_none());

        // The reply is the member's first send to the mailbox after the
        // dispatch — order, not a link — so a row written now answers both
        // (the runner never has two dispatches open on one member).
        let nonce = format!("nonce-{}-{}", std::process::id(), second);
        let reply = crate::bus::write_send_event(
            &workspace,
            "b",
            FLOW_SENDER,
            &format!("done {nonce}"),
            "",
        )
        .unwrap();
        let found = env
            .find_reply(second, "b")
            .unwrap()
            .expect("reply to second");
        assert_eq!(found.seq, reply);
        assert_eq!(found.from, "b");
        assert_eq!(found.body, format!("done {nonce}"));
        assert_eq!(env.find_reply(first, "b").unwrap().unwrap().seq, reply);
        // A dispatch after the reply is open again.
        let third = env.send("b", "third task", "").unwrap();
        assert!(env.find_reply(third, "b").unwrap().is_none());
        // `from` scopes the match: another member's name finds nothing.
        assert!(env.find_reply(second, "c").unwrap().is_none());

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
