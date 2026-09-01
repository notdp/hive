//! hive::flow — deterministic orchestration over live members.
//!
//! A flow script is written per task and run with `hive flow run`. Its
//! `agent()` spawns a real member pane, waits until it is ready, dispatches
//! the task as its first `<HIVE>` message, then blocks until the member
//! replies — the visible counterpart of a headless subagent call.
//!
//! The runner never owns a pane: it dispatches as the reserved `flow`
//! address, whose delivery is the durable bus row itself (the hived's
//! mailbox branch), and reads replies straight off the bus. Members answer
//! with an ordinary `hive send flow` — auto-anchoring threads it back to
//! the dispatch, no new addressing concepts.
//!
//! This module is the op core: the `FlowEnv` seam over cli/bus/layout and
//! the `run_op` dispatch that the script engine calls once per hive
//! interaction. The script surface — the embedded JavaScript dialect, its
//! prelude, and the resume journal — lives in `crate::flow_script`.
//! Deliberately not here: token budgets, progress UI — the panes are the
//! progress display.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use serde_json::{Map, Value};

use crate::bus::Event;

pub const FLOW_SENDER: &str = "flow.run";
const REPLY_POLL_SECONDS: f64 = 2.0;

// tmux splits and team registration race each other; spawns serialize,
// waiting and reply-polling stay parallel.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

const DISPATCH_ATTEMPTS: usize = 3;
const DISPATCH_RETRY_GAP: f64 = 3.0;

/// A flow step failed loudly: spawn, ready gate, or dispatch.
#[derive(Debug)]
pub struct FlowError(pub String);

impl fmt::Display for FlowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FlowError {}

/// The resolved team context. The live `Team` object stays inside the env
/// implementation; flow logic only needs these fields.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub team_name: String,
    pub workspace: String,
    pub tmux_window: String,
}

/// What flow reads off a freshly spawned agent.
#[derive(Debug, Clone)]
pub struct SpawnedAgent {
    pub pane_id: String,
    pub cli: String,
}

/// The seams flow reaches through (`hive::cli::*`, `hive::bus::find_reply_to`,
/// `hive::layout::apply_adaptive`). `Err(String)` on spawn/send is the
/// transient failure the retry loops absorb.
pub trait FlowEnv: Send + Sync {
    /// Resolve (and cache) the scoped team context.
    fn context(&self) -> Result<Ctx, FlowError>;
    fn spawn_team_agent(
        &self,
        team_name: &str,
        agent_name: &str,
        model: &str,
        prompt: &str,
        skill: &str,
        cli_name: Option<&str>,
    ) -> Result<SpawnedAgent, String>;
    fn ensure_team_hived(&self, workspace: &str);
    /// Returns the set of agents still not ready when the gate expires.
    fn wait_for_peer_ready(
        &self,
        workspace: &str,
        team_name: &str,
        agents: &HashSet<String>,
    ) -> HashSet<String>;
    #[allow(clippy::too_many_arguments)]
    fn request_send_payload(
        &self,
        workspace: &str,
        sender_agent: &str,
        target_agent: &str,
        body: &str,
        artifact: &str,
        command_name: &str,
        warn_on_long_body: bool,
    ) -> Result<Map<String, Value>, String>;
    fn find_reply_to(
        &self,
        workspace: &str,
        msg_id: &str,
        from_agent: &str,
    ) -> Result<Option<Event>, FlowError>;
    /// Kill the named member's pane and drop it from the team roster
    /// (no-op when absent).
    fn kill_team_agent(&self, name: &str);
    /// Whether the named member is currently on the team roster — the
    /// resume journal's liveness probe for spawn replay.
    fn member_exists(&self, name: &str) -> bool;
    fn apply_adaptive(&self, window: &str);
    fn sleep(&self, seconds: f64);
}

fn log(message: &str) {
    println!("[flow] {message}");
    let _ = std::io::stdout().flush();
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

/// Send with bounded retries: a cloud-backed transport (grok leader RPC,
/// codex daemon) can refuse transiently under provider throttling, and a
/// single blip must not kill a whole orchestration. Still loud on exhaustion.
fn dispatch(
    env: &dyn FlowEnv,
    name: &str,
    body: &str,
    artifact: &str,
) -> Result<String, FlowError> {
    let ctx = env.context()?;
    let mut last = String::new();
    for attempt in 0..DISPATCH_ATTEMPTS {
        match env.request_send_payload(
            &ctx.workspace,
            FLOW_SENDER,
            name,
            body,
            artifact,
            "flow-dispatch",
            false,
        ) {
            Ok(payload) => {
                let msg_id = match payload.get("msgId") {
                    Some(Value::String(s)) => s.clone(),
                    _ => String::new(),
                };
                return Ok(msg_id);
            }
            Err(exc) => {
                last = exc;
                if attempt + 1 < DISPATCH_ATTEMPTS {
                    log(&format!(
                        "{name} dispatch refused ({last}); retry {}/{DISPATCH_ATTEMPTS}",
                        attempt + 2
                    ));
                    env.sleep(DISPATCH_RETRY_GAP);
                }
            }
        }
    }
    Err(FlowError(format!(
        "dispatch to '{name}' failed after {DISPATCH_ATTEMPTS} attempts: {last}"
    )))
}

/// Block until a reply from `name` anchored to `msg_id` lands on the bus.
///
/// Scoped to `name`: a row anchored to the dispatch by anyone else — a
/// bystander touching the thread — is not this member's deliverable.
///
/// No timeout by design: the members are visible panes and the human is
/// the supervisor — interrupt the flow run to stop waiting.
fn await_reply(env: &dyn FlowEnv, name: &str, msg_id: &str) -> Result<Event, FlowError> {
    let ctx = env.context()?;
    loop {
        if let Some(row) = env.find_reply_to(&ctx.workspace, msg_id, name)? {
            return Ok(row);
        }
        env.sleep(REPLY_POLL_SECONDS);
    }
}

/// The spawn phase of `agent()`: flow/flow.* name guard plus the bounded
/// retry loop, each attempt serialized under the process-local spawn lock.
fn spawn_member(
    env: &dyn FlowEnv,
    name: &str,
    cli: Option<&str>,
    model: &str,
) -> Result<SpawnedAgent, FlowError> {
    if name == "flow" || name.starts_with("flow.") {
        return Err(FlowError(format!(
            "'{name}' collides with the flow runner's mailbox address kind ({FLOW_SENDER}); pick another member name"
        )));
    }
    let ctx = env.context()?;
    let mut last = String::new();
    for attempt in 0..DISPATCH_ATTEMPTS {
        let result = {
            let _guard = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
            env.spawn_team_agent(&ctx.team_name, name, model, "", "hive:hive", cli)
        };
        match result {
            Ok(agent) => return Ok(agent),
            Err(exc) => {
                // A cloud transport (codex mint, grok leader) fails fast under
                // provider throttling; absorb blips here instead of widening
                // its RPC timeout — each retry is visible, the total bounded.
                last = exc;
            }
        }
        if attempt + 1 < DISPATCH_ATTEMPTS {
            log(&format!(
                "{name} spawn failed ({last}); retry {}/{DISPATCH_ATTEMPTS}",
                attempt + 2
            ));
            env.sleep(DISPATCH_RETRY_GAP);
        }
    }
    Err(FlowError(format!(
        "spawn '{name}' failed after {DISPATCH_ATTEMPTS} attempts: {last}"
    )))
}

/// The post-spawn phase of `agent()`: hived convergence plus the ready gate.
fn ready_gate(env: &dyn FlowEnv, name: &str, cli: &str) -> Result<(), FlowError> {
    let ctx = env.context()?;
    env.ensure_team_hived(&ctx.workspace);
    if cli != "claude" {
        // claude inboxes queue; only TUI-injected CLIs need the ready gate.
        let mut agents = HashSet::new();
        agents.insert(name.to_string());
        let not_ready = env.wait_for_peer_ready(&ctx.workspace, &ctx.team_name, &agents);
        if !not_ready.is_empty() {
            return Err(FlowError(format!(
                "member '{name}' did not reach ready within the gate; inspect its pane"
            )));
        }
    }
    Ok(())
}

/// Retire a member's pane and re-tile the window.
fn kill_member(env: &dyn FlowEnv, name: &str) -> Result<(), FlowError> {
    let ctx = env.context()?;
    let _guard = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    env.kill_team_agent(name);
    if !ctx.tmux_window.is_empty() {
        env.apply_adaptive(&ctx.tmux_window);
    }
    Ok(())
}

/// One hive interaction on behalf of the script engine. Each `agent()` in a
/// flow script decomposes into spawn → ready → dispatch-task → wait-reply;
/// `Member.ask()` into dispatch-ask → wait-reply. The journal in
/// `flow_script` caches these results by (op, args) — keep every op's args
/// free of run-relative values (paths with counters, timestamps) so replay
/// keys stay stable across resumes.
pub(crate) fn run_op(
    env: &dyn FlowEnv,
    op: &str,
    args: &Value,
) -> Result<Map<String, Value>, FlowError> {
    let str_arg = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let mut result = Map::new();
    match op {
        "context" => {
            let ctx = env.context()?;
            result.insert("teamName".to_string(), Value::String(ctx.team_name));
            result.insert("workspace".to_string(), Value::String(ctx.workspace));
        }
        "spawn" => {
            let cli = args
                .get("cli")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            let spawned = spawn_member(env, &str_arg("name"), cli, &str_arg("model"))?;
            result.insert("pane".to_string(), Value::String(spawned.pane_id));
            result.insert("cli".to_string(), Value::String(spawned.cli));
        }
        "ready" => ready_gate(env, &str_arg("name"), &str_arg("cli"))?,
        "dispatch-task" => {
            // The prompt is the whole contract — the script writes it
            // self-contained. It rides a task artifact; the body is the
            // same atomic skeleton as `hive spawn --task`.
            let name = str_arg("name");
            let artifact = task_artifact(env, &name, &str_arg("prompt"))?;
            let artifact_name = Path::new(&artifact)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let msg_id = dispatch(
                env,
                &name,
                &format!(
                    "flow-mailbox dispatch: {artifact_name} (not a member; hive send flow.run, then stop)"
                ),
                &artifact,
            )?;
            result.insert("msgId".to_string(), Value::String(msg_id));
            result.insert("artifact".to_string(), Value::String(artifact));
        }
        "dispatch-ask" => {
            // Follow-up (question, rework order): short single-line prompts
            // ride the body, anything longer rides an artifact.
            let name = str_arg("name");
            let prompt = str_arg("prompt");
            let (body, artifact) = if prompt.contains('\n') || prompt.chars().count() > 200 {
                let artifact = task_artifact(env, &format!("{name}-ask"), &prompt)?;
                ("follow-up: see artifact".to_string(), artifact)
            } else {
                (prompt, String::new())
            };
            let msg_id = dispatch(env, &name, &body, &artifact)?;
            result.insert("msgId".to_string(), Value::String(msg_id));
        }
        "wait-reply" => {
            let row = await_reply(env, &str_arg("name"), &str_arg("msgId"))?;
            result.insert("body".to_string(), Value::String(row.body));
            result.insert("artifact".to_string(), Value::String(row.artifact));
            result.insert("msgId".to_string(), Value::String(row.msg_id));
        }
        "kill" => kill_member(env, &str_arg("name"))?,
        _ => return Err(FlowError(format!("unknown flow op '{op}'"))),
    }
    Ok(result)
}

/// `hive flow node start`: spawn + ready + dispatch-task as one blocking
/// verb. This is the seam an external orchestrator (a Claude Code workflow's
/// hive-node proxy agent) drives: it runs this once, then polls `node_wait`
/// in bounded slices, because its shell tool has a hard per-call timeout.
pub fn node_start(
    env: &dyn FlowEnv,
    name: &str,
    cli: Option<&str>,
    model: &str,
    task: &str,
) -> Result<Map<String, Value>, FlowError> {
    let spawned = run_op(
        env,
        "spawn",
        &serde_json::json!({"name": name, "cli": cli, "model": model}),
    )?;
    let cli_resolved = spawned
        .get("cli")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    run_op(
        env,
        "ready",
        &serde_json::json!({"name": name, "cli": cli_resolved}),
    )?;
    let mut result = run_op(
        env,
        "dispatch-task",
        &serde_json::json!({"name": name, "prompt": task}),
    )?;
    result.insert(
        "pane".to_string(),
        spawned.get("pane").cloned().unwrap_or(Value::Null),
    );
    result.insert("cli".to_string(), Value::String(cli_resolved));
    Ok(result)
}

/// Bounded reply poll for `hive flow node wait`: `status: "replied"` with
/// the reply row, or `status: "pending"` at the deadline — a timeout is not
/// an error, the caller loops.
pub fn node_wait(
    env: &dyn FlowEnv,
    name: &str,
    msg_id: &str,
    timeout_seconds: f64,
) -> Result<Map<String, Value>, FlowError> {
    let ctx = env.context()?;
    let mut waited = 0.0;
    loop {
        if let Some(row) = env.find_reply_to(&ctx.workspace, msg_id, name)? {
            let mut result = Map::new();
            result.insert("status".to_string(), Value::String("replied".into()));
            result.insert("body".to_string(), Value::String(row.body));
            result.insert("artifact".to_string(), Value::String(row.artifact));
            result.insert("msgId".to_string(), Value::String(row.msg_id));
            return Ok(result);
        }
        if waited >= timeout_seconds {
            let mut result = Map::new();
            result.insert("status".to_string(), Value::String("pending".into()));
            return Ok(result);
        }
        env.sleep(REPLY_POLL_SECONDS);
        waited += REPLY_POLL_SECONDS;
    }
}

// ---------------------------------------------------------------------------
// Live wiring.
// ---------------------------------------------------------------------------

struct RealCtx {
    team_name: String,
    workspace: String,
    team: crate::team::Team,
}

/// Production `FlowEnv`: resolves the scoped team once and forwards every
/// seam to the cli/bus/layout modules.
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
    /// — the `--team` lane for callers outside tmux (a desktop CCD session,
    /// a workflow proxy subagent).
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
                crate::cli::resolve_scoped_team(self.team_arg.as_deref(), true)
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
}

impl FlowEnv for RealEnv {
    fn context(&self) -> Result<Ctx, FlowError> {
        self.with_ctx(|c| Ctx {
            team_name: c.team_name.clone(),
            workspace: c.workspace.clone(),
            tmux_window: c.team.tmux_window.clone(),
        })
    }

    fn spawn_team_agent(
        &self,
        team_name: &str,
        agent_name: &str,
        model: &str,
        prompt: &str,
        skill: &str,
        cli_name: Option<&str>,
    ) -> Result<SpawnedAgent, String> {
        self.with_ctx(|c| {
            crate::cli::spawn_team_agent(
                &mut c.team,
                team_name,
                agent_name,
                model,
                prompt,
                "",
                skill,
                &[],
                cli_name,
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

    fn ensure_team_hived(&self, workspace: &str) {
        let _ = self.with_ctx(|c| {
            crate::cli::ensure_team_hived(&c.team, Path::new(workspace));
        });
    }

    fn wait_for_peer_ready(
        &self,
        workspace: &str,
        team_name: &str,
        agents: &HashSet<String>,
    ) -> HashSet<String> {
        crate::cli::wait_for_peer_ready(workspace, team_name, agents, 30.0, 0.5)
    }

    fn request_send_payload(
        &self,
        workspace: &str,
        sender_agent: &str,
        target_agent: &str,
        body: &str,
        artifact: &str,
        command_name: &str,
        warn_on_long_body: bool,
    ) -> Result<Map<String, Value>, String> {
        self.with_ctx(|c| {
            crate::cli::request_send_payload(
                workspace,
                &c.team,
                sender_agent,
                target_agent,
                body,
                artifact,
                "",
                command_name,
                warn_on_long_body,
            )
            .map_err(|e| e.to_string())
        })
        .map_err(|e| e.0)
        .and_then(|inner| inner)
    }

    fn find_reply_to(
        &self,
        workspace: &str,
        msg_id: &str,
        from_agent: &str,
    ) -> Result<Option<Event>, FlowError> {
        crate::bus::find_reply_to(workspace, msg_id, from_agent)
            .map_err(|e| FlowError(e.to_string()))
    }

    fn kill_team_agent(&self, name: &str) {
        let _ = self.with_ctx(|c| {
            if let Some(pos) = c.team.agents.iter().position(|a| a.name == name) {
                c.team.agents[pos].kill();
                c.team.agents.remove(pos);
            }
        });
    }

    fn member_exists(&self, name: &str) -> bool {
        self.with_ctx(|c| c.team.agents.iter().any(|a| a.name == name))
            .unwrap_or(false)
    }

    fn apply_adaptive(&self, window: &str) {
        let _ = crate::layout::apply_adaptive(window);
    }

    fn sleep(&self, seconds: f64) {
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
    }
}

// ---------------------------------------------------------------------------
// Shared test env: the pytest `_wire` seams as one fake, used by this
// module's op tests and by the script-engine tests in `flow_script`.
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_env {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Debug)]
    pub(crate) struct SpawnCall {
        pub agent_name: String,
        pub prompt: String,
        pub skill: String,
    }

    #[derive(Debug)]
    pub(crate) struct DispatchCall {
        pub sender_agent: String,
        pub target_agent: String,
        pub body: String,
        pub artifact: String,
    }

    /// Failure knobs replace the per-test monkeypatched flaky seams; `sleep`
    /// is a no-op, standing in for the poll/retry-gap monkeypatches.
    pub(crate) struct FakeEnv {
        pub workspace: PathBuf,
        pub tmux_window: String,
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
        pub killed: Mutex<Vec<String>>,
    }

    pub(crate) fn fake_env(tmp: &Path) -> FakeEnv {
        let ws = tmp.join("ws");
        fs::create_dir_all(&ws).unwrap();
        FakeEnv {
            workspace: ws,
            tmux_window: "dev:0".to_string(),
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
            killed: Mutex::new(Vec::new()),
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
                tmux_window: self.tmux_window.clone(),
            })
        }

        fn spawn_team_agent(
            &self,
            _team_name: &str,
            agent_name: &str,
            _model: &str,
            prompt: &str,
            skill: &str,
            cli_name: Option<&str>,
        ) -> Result<SpawnedAgent, String> {
            let n = self.spawn_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= self.spawn_fail_first {
                return Err(self.spawn_err.clone());
            }
            let mut spawns = self.spawns.lock().unwrap();
            spawns.push(SpawnCall {
                agent_name: agent_name.to_string(),
                prompt: prompt.to_string(),
                skill: skill.to_string(),
            });
            self.agents.lock().unwrap().push(agent_name.to_string());
            Ok(SpawnedAgent {
                pane_id: format!("%{}", spawns.len()),
                cli: cli_name.unwrap_or("claude").to_string(),
            })
        }

        fn ensure_team_hived(&self, _workspace: &str) {}

        fn wait_for_peer_ready(
            &self,
            _workspace: &str,
            _team_name: &str,
            agents: &HashSet<String>,
        ) -> HashSet<String> {
            if self.ready {
                HashSet::new()
            } else {
                agents.clone()
            }
        }

        fn request_send_payload(
            &self,
            _workspace: &str,
            sender_agent: &str,
            target_agent: &str,
            body: &str,
            artifact: &str,
            _command_name: &str,
            _warn_on_long_body: bool,
        ) -> Result<Map<String, Value>, String> {
            let n = self.send_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= self.dispatch_fail_first {
                return Err(self.dispatch_err.clone());
            }
            let msg_id = format!("m{}", self.msg_seq.fetch_add(1, Ordering::SeqCst) + 1);
            self.dispatches.lock().unwrap().push(DispatchCall {
                sender_agent: sender_agent.to_string(),
                target_agent: target_agent.to_string(),
                body: body.to_string(),
                artifact: artifact.to_string(),
            });
            let mut payload = Map::new();
            payload.insert("msgId".to_string(), Value::String(msg_id));
            Ok(payload)
        }

        fn find_reply_to(
            &self,
            _workspace: &str,
            msg_id: &str,
            from_agent: &str,
        ) -> Result<Option<Event>, FlowError> {
            self.awaits
                .lock()
                .unwrap()
                .push((from_agent.to_string(), msg_id.to_string()));
            if self.reply_any {
                return Ok(Some(reply_row(
                    &format!("done-{msg_id}"),
                    "",
                    &format!("r-{msg_id}"),
                )));
            }
            Ok(self.replies.lock().unwrap().get(msg_id).cloned())
        }

        fn kill_team_agent(&self, name: &str) {
            let mut agents = self.agents.lock().unwrap();
            if let Some(pos) = agents.iter().position(|a| a == name) {
                self.killed.lock().unwrap().push(name.to_string());
                agents.remove(pos);
            }
        }

        fn member_exists(&self, name: &str) -> bool {
            self.agents.lock().unwrap().iter().any(|a| a == name)
        }

        fn apply_adaptive(&self, window: &str) {
            self.killed.lock().unwrap().push(format!("layout {window}"));
        }

        fn sleep(&self, _seconds: f64) {}
    }
}

#[cfg(test)]
mod tests {
    use super::test_env::*;
    use super::*;
    use serde_json::json;
    use std::sync::atomic::Ordering;
    use tempfile::TempDir;

    #[test]
    fn test_run_op_covers_the_script_engine_protocol() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());

        let r = run_op(&env, "context", &json!({})).unwrap();
        assert_eq!(r.get("teamName"), Some(&Value::String("t-x".into())));
        assert_eq!(
            r.get("workspace"),
            Some(&Value::String(env.workspace.to_string_lossy().into_owned()))
        );

        let r = run_op(
            &env,
            "spawn",
            &json!({"name": "impl", "cli": null, "model": ""}),
        )
        .unwrap();
        assert_eq!(r.get("pane"), Some(&Value::String("%1".into())));
        assert_eq!(r.get("cli"), Some(&Value::String("claude".into())));
        // spawn boots the member-contract plugin skill, no prose prompt
        {
            let spawns = env.spawns.lock().unwrap();
            assert_eq!(spawns[0].agent_name, "impl");
            assert_eq!(spawns[0].prompt, "");
            assert_eq!(spawns[0].skill, "hive:hive");
        }

        run_op(&env, "ready", &json!({"name": "impl", "cli": "claude"})).unwrap();

        let r = run_op(
            &env,
            "dispatch-task",
            &json!({"name": "impl", "prompt": "explore auth\nwrite findings"}),
        )
        .unwrap();
        assert_eq!(r.get("msgId"), Some(&Value::String("m1".into())));
        let artifact = r.get("artifact").and_then(Value::as_str).unwrap();
        // dispatch rode an artifact carrying the full prompt, from the flow sender
        assert_eq!(
            fs::read_to_string(artifact).unwrap(),
            "explore auth\nwrite findings"
        );
        {
            let dispatches = env.dispatches.lock().unwrap();
            assert_eq!(dispatches[0].sender_agent, "flow.run");
            assert_eq!(dispatches[0].target_agent, "impl");
            assert_eq!(dispatches[0].artifact, artifact);
            assert!(dispatches[0].body.starts_with("flow-mailbox dispatch: "));
        }

        env.replies
            .lock()
            .unwrap()
            .insert("m1".to_string(), reply_row("done", "/tmp/f.md", "r1"));
        let r = run_op(&env, "wait-reply", &json!({"name": "impl", "msgId": "m1"})).unwrap();
        assert_eq!(r.get("body"), Some(&Value::String("done".into())));
        assert_eq!(r.get("artifact"), Some(&Value::String("/tmp/f.md".into())));
        assert_eq!(r.get("msgId"), Some(&Value::String("r1".into())));
        // the wait is scoped to the member: a row anchored to m1 by anyone
        // else is not the reply
        assert_eq!(
            *env.awaits.lock().unwrap(),
            vec![("impl".to_string(), "m1".to_string())]
        );

        run_op(&env, "kill", &json!({"name": "impl"})).unwrap();
        assert_eq!(
            *env.killed.lock().unwrap(),
            vec!["impl".to_string(), "layout dev:0".to_string()]
        );
        assert!(!env.member_exists("impl"));
    }

    #[test]
    fn test_dispatch_ask_short_prompt_rides_the_body() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        run_op(
            &env,
            "dispatch-ask",
            &json!({"name": "impl", "prompt": "rework: handle the null case"}),
        )
        .unwrap();
        let dispatches = env.dispatches.lock().unwrap();
        assert_eq!(dispatches[0].body, "rework: handle the null case");
        assert_eq!(dispatches[0].artifact, "");
    }

    #[test]
    fn test_dispatch_ask_long_prompt_rides_an_artifact() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        run_op(
            &env,
            "dispatch-ask",
            &json!({"name": "impl", "prompt": "line one\nline two of a long rework order"}),
        )
        .unwrap();
        let dispatches = env.dispatches.lock().unwrap();
        assert_eq!(dispatches[0].body, "follow-up: see artifact");
        assert!(fs::read_to_string(&dispatches[0].artifact)
            .unwrap()
            .starts_with("line one"));
    }

    #[test]
    fn test_run_op_ready_gates_non_claude_and_skips_claude() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.ready = false;
        run_op(&env, "ready", &json!({"name": "impl", "cli": "claude"})).unwrap();
        let err = run_op(&env, "ready", &json!({"name": "impl", "cli": "codex"})).unwrap_err();
        assert!(err.0.contains("did not reach ready"), "{err}");
    }

    #[test]
    fn test_run_op_spawn_rejects_the_mailbox_name_family() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        for name in ["flow", "flow.run", "flow.anything"] {
            let err = run_op(&env, "spawn", &json!({"name": name, "model": ""})).unwrap_err();
            assert!(err.0.contains("mailbox address kind"), "{err}");
        }
        assert!(env.spawns.lock().unwrap().is_empty());
    }

    #[test]
    fn test_spawn_retries_transient_failure_then_succeeds() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.spawn_fail_first = 1;
        env.spawn_err = "codex app-server refused to mint a thread".to_string();
        let r = run_op(&env, "spawn", &json!({"name": "impl", "model": ""})).unwrap();
        assert_eq!(r.get("pane"), Some(&Value::String("%1".into())));
        assert_eq!(env.spawn_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_spawn_exhaustion_stays_loud() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.spawn_fail_first = u32::MAX;
        let err = run_op(&env, "spawn", &json!({"name": "impl", "model": ""})).unwrap_err();
        assert!(err.0.contains("after 3 attempts"), "{err}");
    }

    #[test]
    fn test_dispatch_retries_transient_refusal_then_succeeds() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.dispatch_fail_first = 1;
        env.dispatch_err = "transport refused: leader RPC blip".to_string();
        let r = run_op(
            &env,
            "dispatch-task",
            &json!({"name": "impl", "prompt": "task"}),
        )
        .unwrap();
        assert_eq!(r.get("msgId"), Some(&Value::String("m1".into())));
        assert_eq!(env.send_calls.load(Ordering::SeqCst), 2); // one blip absorbed
    }

    #[test]
    fn test_dispatch_exhaustion_stays_loud() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.dispatch_fail_first = u32::MAX;
        let err = run_op(
            &env,
            "dispatch-task",
            &json!({"name": "impl", "prompt": "task"}),
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

    #[test]
    fn test_node_start_spawns_gates_and_dispatches_atomically() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        let r = node_start(&env, "impl", Some("codex"), "", "build the thing\nwith detail").unwrap();
        assert_eq!(r.get("pane"), Some(&Value::String("%1".into())));
        assert_eq!(r.get("cli"), Some(&Value::String("codex".into())));
        assert_eq!(r.get("msgId"), Some(&Value::String("m1".into())));
        let artifact = r.get("artifact").and_then(Value::as_str).unwrap();
        assert_eq!(
            fs::read_to_string(artifact).unwrap(),
            "build the thing\nwith detail"
        );
        assert_eq!(env.spawns.lock().unwrap().len(), 1);
        assert_eq!(env.dispatches.lock().unwrap()[0].sender_agent, "flow.run");
    }

    #[test]
    fn test_node_wait_replied_and_pending() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());

        let r = node_wait(&env, "impl", "m9", 6.0).unwrap();
        assert_eq!(r.get("status"), Some(&Value::String("pending".into())));
        // polled at least once before the deadline
        assert!(!env.awaits.lock().unwrap().is_empty());

        env.replies
            .lock()
            .unwrap()
            .insert("m9".to_string(), reply_row("done", "/tmp/o.md", "r9"));
        let r = node_wait(&env, "impl", "m9", 6.0).unwrap();
        assert_eq!(r.get("status"), Some(&Value::String("replied".into())));
        assert_eq!(r.get("body"), Some(&Value::String("done".into())));
        assert_eq!(r.get("artifact"), Some(&Value::String("/tmp/o.md".into())));
    }

    #[test]
    fn test_run_op_unknown_op_is_loud() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        let err = run_op(&env, "bogus", &json!({})).unwrap_err();
        assert_eq!(err.0, "unknown flow op 'bogus'");
    }
}
