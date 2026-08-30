//! hive::flow — deterministic orchestration over live members.
//!
//! A flow script is written per task and run with `hive flow run`. `agent()`
//! spawns a real member pane, waits until it is ready, dispatches the task as
//! its first `<HIVE>` message, then blocks until the member replies — the
//! visible counterpart of a headless subagent call. `parallel()` runs several
//! of those at once.
//!
//! The runner never owns a pane: it dispatches as the reserved `flow`
//! address, whose delivery is the durable bus row itself (the hived's
//! mailbox branch), and reads replies straight off the bus. Members answer
//! with an ordinary `hive send flow` — auto-anchoring threads it back to
//! the dispatch, no new addressing concepts.
//!
//! Deliberately not here: sandboxing (the script author is the orch),
//! schema validation, resume journals, token budgets, progress UI — the
//! panes are the progress display.
//!
//! Python seams (`hive.cli._spawn_team_agent`, `hive.cli._request_send_payload`,
//! …) become the `FlowEnv` trait; `RealEnv` is the live wiring, tests inject a
//! fake — the same seam the pytest suite monkeypatches.

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

/// The resolved team context (Python `_Ctx`). The live `Team` object stays
/// inside the env implementation; flow logic only needs these fields.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub team_name: String,
    pub workspace: String,
    pub tmux_window: String,
}

/// What flow reads off a freshly spawned agent (Python `Agent` attributes).
#[derive(Debug, Clone)]
pub struct SpawnedAgent {
    pub pane_id: String,
    pub cli: String,
}

/// The seams flow.py reaches through (`hive.cli.*`, `hive.bus.find_reply_to`,
/// `hive.layout.apply_adaptive`). `Err(String)` on spawn/send is the transient
/// exception message Python retries on (ValueError/RuntimeError).
pub trait FlowEnv: Sync {
    /// Resolve (and cache) the scoped team context — Python `flow._context()`.
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
    /// (Python: `agents[name].kill(); del agents[name]` — no-op when absent).
    fn kill_team_agent(&self, name: &str);
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

/// A live member the flow dispatched to. Fields hold its latest reply.
#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub pane: String,
    pub summary: String,
    pub artifact: String,
    pub msg_id: String,
    dead: bool,
}

impl Member {
    fn absorb(&mut self, reply: &Event) {
        self.summary = reply.body.clone();
        self.artifact = reply.artifact.clone();
        self.msg_id = reply.msg_id.clone();
    }

    /// Send a follow-up (question, rework order) and block for the answer.
    ///
    /// The member keeps its full context — this is what a dead headless
    /// subagent cannot do.
    pub fn ask(&mut self, env: &dyn FlowEnv, prompt: &str) -> Result<&mut Member, FlowError> {
        if self.dead {
            return Err(FlowError(format!(
                "member '{}' was killed; spawn a new one",
                self.name
            )));
        }
        let (body, artifact) = if prompt.contains('\n') || prompt.chars().count() > 200 {
            let artifact = task_artifact(env, &format!("{}-ask", self.name), prompt)?;
            ("follow-up: see artifact".to_string(), artifact)
        } else {
            (prompt.to_string(), String::new())
        };
        let msg_id = dispatch(env, &self.name, &body, &artifact)?;
        log(&format!("{} asked ({msg_id}); waiting…", self.name));
        let reply = await_reply(env, &self.name, &msg_id)?;
        self.absorb(&reply);
        log(&format!("{} answered ({})", self.name, self.msg_id));
        Ok(self)
    }

    /// Retire the member's pane; the window re-tiles.
    pub fn kill(&mut self, env: &dyn FlowEnv) -> Result<(), FlowError> {
        kill_member(env, &self.name)?;
        self.dead = true;
        log(&format!("{} retired", self.name));
        Ok(())
    }
}

/// The spawn phase of `agent()`: flow/flow.* name guard plus the bounded
/// retry loop, each attempt serialized under the process-local spawn lock
/// (the script client adds cross-process serialization with its own lock).
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

/// Retire a member's pane and re-tile the window (`Member::kill` body).
fn kill_member(env: &dyn FlowEnv, name: &str) -> Result<(), FlowError> {
    let ctx = env.context()?;
    let _guard = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    env.kill_team_agent(name);
    if !ctx.tmux_window.is_empty() {
        env.apply_adaptive(&ctx.tmux_window);
    }
    Ok(())
}

/// Spawn a member, dispatch `prompt` as its task, block for its reply.
///
/// The prompt is the whole contract — write it self-contained (scope,
/// deliverable path, acceptance, material paths). It is written to
/// `<workspace>/artifacts/tasks/<name>.md` and dispatched with the
/// same atomic skeleton as `hive spawn --task`.
pub fn agent(
    env: &dyn FlowEnv,
    prompt: &str,
    name: &str,
    cli: Option<&str>,
    model: &str,
) -> Result<Member, FlowError> {
    let spawned = spawn_member(env, name, cli, model)?;
    log(&format!("{name} spawned in {}", spawned.pane_id));
    ready_gate(env, name, &spawned.cli)?;

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
    log(&format!("{name} dispatched ({msg_id}); waiting for reply…"));
    let mut member = Member {
        name: name.to_string(),
        pane: spawned.pane_id,
        summary: String::new(),
        artifact: String::new(),
        msg_id: String::new(),
        dead: false,
    };
    let reply = await_reply(env, name, &msg_id)?;
    member.absorb(&reply);
    log(&format!("{} replied ({})", member.name, member.msg_id));
    Ok(member)
}

/// Run thunks concurrently; return their results in call order.
///
/// The first error propagates after every thread finishes — no silent
/// partial results. (Python accepts heterogeneous return types; Rust
/// callers unify on one `T`.)
pub fn parallel<'a, T: Send + 'a>(
    thunks: Vec<Box<dyn FnOnce() -> Result<T, FlowError> + Send + 'a>>,
) -> Result<Vec<T>, FlowError> {
    if thunks.is_empty() {
        return Ok(Vec::new());
    }
    let outcomes: Vec<Result<T, FlowError>> = std::thread::scope(|scope| {
        let handles: Vec<_> = thunks.into_iter().map(|thunk| scope.spawn(thunk)).collect();
        handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(outcome) => outcome,
                Err(_) => Err(FlowError("flow thread panicked".to_string())),
            })
            .collect()
    });
    let mut results = Vec::with_capacity(outcomes.len());
    let mut first_error: Option<FlowError> = None;
    for outcome in outcomes {
        match outcome {
            Ok(value) => results.push(value),
            Err(err) => {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
    }
    match first_error {
        Some(err) => Err(err),
        None => Ok(results),
    }
}

// ---------------------------------------------------------------------------
// Live wiring. Cross-module signatures for cli/team/agent are derived from the
// Python definitions (those modules are still being ported); the integration
// pass reconciles the seams. Derived assumptions:
//   crate::cli::resolve_scoped_team(None, true)
//       -> anyhow::Result<(Option<String>, Option<crate::team::Team>)>
//   crate::cli::resolve_workspace(Some(&Team), true) -> anyhow::Result<String>
//   crate::cli::spawn_team_agent(&mut Team, team_name, agent_name, model,
//       prompt, cwd, skill, env_entries, cli_name) -> anyhow::Result<Agent>
//       with Agent { pane_id: String, cli: String, fn kill(&self) }
//   crate::cli::ensure_team_hived(&Team, &Path) (return ignored)
//   crate::cli::wait_for_peer_ready(workspace, team_name, &HashSet<String>,
//       30.0, 0.5) -> HashSet<String>
//   crate::cli::request_send_payload(workspace, &Team, sender, target, body,
//       artifact, reply_to, command_name, warn_on_long_body)
//       -> anyhow::Result<serde_json::Map<String, Value>>
//   crate::team::Team { name, tmux_window: String,
//       agents: HashMap<String, crate::agent::Agent> }
// ---------------------------------------------------------------------------

struct RealCtx {
    team_name: String,
    workspace: String,
    team: crate::team::Team,
}

/// Production `FlowEnv`: resolves the scoped team once (Python module-level
/// `_ctx` singleton) and forwards every seam to the cli/bus/layout modules.
pub struct RealEnv {
    ctx: Mutex<Option<RealCtx>>,
}

impl Default for RealEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl RealEnv {
    pub fn new() -> Self {
        RealEnv {
            ctx: Mutex::new(None),
        }
    }

    fn with_ctx<R>(&self, f: impl FnOnce(&mut RealCtx) -> R) -> Result<R, FlowError> {
        let mut guard = self.ctx.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.is_none() {
            let (team_name, team) = crate::cli::resolve_scoped_team(None, true)
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

    fn apply_adaptive(&self, window: &str) {
        let _ = crate::layout::apply_adaptive(window);
    }

    fn sleep(&self, seconds: f64) {
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
    }
}

// ---------------------------------------------------------------------------
// script bridge: materialized python client + hidden `hive flow-op` surface
// ---------------------------------------------------------------------------

const PYLIB_INIT: &str = include_str!("../assets/pylib/hive/__init__.py");
const PYLIB_FLOW: &str = include_str!("../assets/pylib/hive/flow.py");

/// Write the embedded flow-client python tree under
/// `$HIVE_HOME/core_assets/pylib/` (heal-on-drift, like the cvim assets) and
/// return the directory `hive flow run` prepends to PYTHONPATH.
pub fn materialize_pylib() -> anyhow::Result<std::path::PathBuf> {
    let root = crate::core_hooks::hive_home()
        .join("core_assets")
        .join("pylib");
    crate::core_hooks::materialize_asset_tree(
        &root,
        &[
            ("hive/__init__.py", PYLIB_INIT, false),
            ("hive/flow.py", PYLIB_FLOW, false),
        ],
    )?;
    Ok(root)
}

/// Hidden `hive flow-op <op> [json-args]` — the seams the materialized
/// python client calls back into. stdout protocol: `[flow] …` progress
/// lines stream through, the final line is one JSON object
/// (`{"ok": true, …}` on success, `{"ok": false, "error": …}` + exit 1).
pub fn op_main(args: &[String]) -> i32 {
    let Some(op) = args.first() else {
        eprintln!("usage: hive flow-op <op> [json-args]");
        return 2;
    };
    let payload: Value = match args.get(1) {
        Some(raw) => match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(err) => {
                println!(
                    "{}",
                    serde_json::json!({"ok": false, "error": format!("bad flow-op args: {err}")})
                );
                return 1;
            }
        },
        None => Value::Object(Map::new()),
    };
    let env = RealEnv::new();
    match run_op(&env, op, &payload) {
        Ok(mut result) => {
            result.insert("ok".to_string(), Value::Bool(true));
            println!("{}", Value::Object(result));
            0
        }
        Err(err) => {
            println!("{}", serde_json::json!({"ok": false, "error": err.0}));
            1
        }
    }
}

fn run_op(env: &dyn FlowEnv, op: &str, args: &Value) -> Result<Map<String, Value>, FlowError> {
    let str_arg = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or("").to_string();
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
        "dispatch" => {
            let msg_id = dispatch(env, &str_arg("name"), &str_arg("body"), &str_arg("artifact"))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tempfile::TempDir;

    #[derive(Debug)]
    struct SpawnCall {
        agent_name: String,
        prompt: String,
        skill: String,
    }

    #[derive(Debug)]
    struct DispatchCall {
        sender_agent: String,
        target_agent: String,
        body: String,
        artifact: String,
    }

    /// The pytest `_wire` seams as one fake env; failure knobs replace the
    /// per-test monkeypatched flaky seams. `sleep` is a no-op, standing in
    /// for the `_REPLY_POLL_SECONDS` / `_DISPATCH_RETRY_GAP` monkeypatches.
    struct FakeEnv {
        workspace: PathBuf,
        tmux_window: String,
        ready: bool,
        reply_any: bool,
        spawn_fail_first: u32,
        dispatch_fail_first: u32,
        spawn_err: String,
        dispatch_err: String,
        spawns: Mutex<Vec<SpawnCall>>,
        dispatches: Mutex<Vec<DispatchCall>>,
        awaits: Mutex<Vec<(String, String)>>,
        replies: Mutex<HashMap<String, Event>>,
        msg_seq: AtomicU32,
        spawn_calls: AtomicU32,
        send_calls: AtomicU32,
        agents: Mutex<Vec<String>>,
        killed: Mutex<Vec<String>>,
    }

    fn fake_env(tmp: &Path) -> FakeEnv {
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

    fn reply_row(body: &str, artifact: &str, msg_id: &str) -> Event {
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

        fn apply_adaptive(&self, window: &str) {
            self.killed.lock().unwrap().push(format!("layout {window}"));
        }

        fn sleep(&self, _seconds: f64) {}
    }

    #[test]
    fn test_agent_spawns_dispatches_and_returns_reply() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.replies.lock().unwrap().insert(
            "m1".to_string(),
            reply_row("done, see file", "/tmp/f.md", "r1"),
        );

        let member = agent(&env, "explore auth\nwrite findings", "explore", None, "").unwrap();

        assert_eq!(member.name, "explore");
        assert_eq!(member.pane, "%1");
        assert_eq!(member.summary, "done, see file");
        assert_eq!(member.artifact, "/tmp/f.md");
        // spawn boots the member-contract plugin skill, no prose prompt
        let spawns = env.spawns.lock().unwrap();
        assert_eq!(spawns[0].agent_name, "explore");
        assert_eq!(spawns[0].prompt, "");
        assert_eq!(spawns[0].skill, "hive:hive");
        // dispatch rode an artifact carrying the full prompt, from the flow sender
        let dispatches = env.dispatches.lock().unwrap();
        assert_eq!(dispatches[0].sender_agent, "flow.run");
        assert_eq!(dispatches[0].target_agent, "explore");
        assert_eq!(
            fs::read_to_string(&dispatches[0].artifact).unwrap(),
            "explore auth\nwrite findings"
        );
        // the wait is scoped to the member: a row anchored to m1 by anyone else is not the reply
        assert_eq!(
            *env.awaits.lock().unwrap(),
            vec![("explore".to_string(), "m1".to_string())]
        );
    }

    #[test]
    fn test_agent_ready_timeout_raises() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.ready = false;
        let err = agent(&env, "task", "explore", Some("codex"), "").unwrap_err();
        assert!(err.0.contains("did not reach ready"), "{err}");
    }

    #[test]
    fn test_agent_claude_skips_ready_gate() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.ready = false; // gate would fail if consulted
        env.replies
            .lock()
            .unwrap()
            .insert("m1".to_string(), reply_row("done", "", "r1"));
        let member = agent(&env, "task", "explore", Some("claude"), "").unwrap();
        assert_eq!(member.summary, "done");
    }

    #[test]
    fn test_agent_rejects_reserved_name() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        let err = agent(&env, "task", "flow", None, "").unwrap_err();
        assert!(err.0.contains("mailbox address kind"), "{err}");
    }

    #[test]
    fn test_ask_dispatches_followup_and_updates_member() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        {
            let mut replies = env.replies.lock().unwrap();
            replies.insert("m1".to_string(), reply_row("first", "", "r1"));
            replies.insert("m2".to_string(), reply_row("fixed", "/tmp/v2.md", "r2"));
        }

        let mut member = agent(&env, "task", "impl", None, "").unwrap();
        member.ask(&env, "rework: handle the null case").unwrap();

        assert_eq!(member.summary, "fixed");
        assert_eq!(member.artifact, "/tmp/v2.md");
        // short single-line follow-up rides the body, no artifact file
        let dispatches = env.dispatches.lock().unwrap();
        assert_eq!(dispatches[1].body, "rework: handle the null case");
        assert_eq!(dispatches[1].artifact, "");
    }

    #[test]
    fn test_ask_long_prompt_rides_an_artifact() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        {
            let mut replies = env.replies.lock().unwrap();
            replies.insert("m1".to_string(), reply_row("first", "", "r1"));
            replies.insert("m2".to_string(), reply_row("ok", "", "r2"));
        }

        let mut member = agent(&env, "task", "impl", None, "").unwrap();
        member
            .ask(&env, "line one\nline two of a long rework order")
            .unwrap();

        let dispatches = env.dispatches.lock().unwrap();
        assert!(!dispatches[1].artifact.is_empty());
        assert!(fs::read_to_string(&dispatches[1].artifact)
            .unwrap()
            .starts_with("line one"));
    }

    #[test]
    fn test_kill_retires_pane_and_blocks_further_asks() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        env.replies
            .lock()
            .unwrap()
            .insert("m1".to_string(), reply_row("done", "", "r1"));
        env.agents.lock().unwrap().push("impl".to_string());

        let mut member = agent(&env, "task", "impl", None, "").unwrap();
        member.kill(&env).unwrap();

        assert_eq!(
            *env.killed.lock().unwrap(),
            vec!["impl".to_string(), "layout dev:0".to_string()]
        );
        assert!(env.agents.lock().unwrap().is_empty());
        let err = member.ask(&env, "more").unwrap_err();
        assert!(err.0.contains("was killed"), "{err}");
    }

    #[test]
    fn test_parallel_returns_in_call_order_and_serializes_spawns() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.reply_any = true;
        let env = &env;

        let thunks: Vec<Box<dyn FnOnce() -> Result<Member, FlowError> + Send + '_>> = vec![
            Box::new(move || agent(env, "task a", "alpha", None, "")),
            Box::new(move || agent(env, "task b", "beta", None, "")),
        ];
        let results = parallel(thunks).unwrap();
        assert_eq!(results[0].name, "alpha");
        assert_eq!(results[1].name, "beta");
        assert!(results[0].summary.starts_with("done-"));
        assert!(results[1].summary.starts_with("done-"));
        let names: HashSet<String> = env
            .spawns
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.agent_name.clone())
            .collect();
        assert_eq!(
            names,
            HashSet::from(["alpha".to_string(), "beta".to_string()])
        );
    }

    #[test]
    fn test_parallel_propagates_first_error() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.ready = false;
        let env = &env;

        let thunks: Vec<Box<dyn FnOnce() -> Result<i32, FlowError> + Send + '_>> = vec![
            Box::new(move || agent(env, "t", "x", Some("codex"), "").map(|_| 0)),
            Box::new(|| Ok(42)),
        ];
        let err = parallel(thunks).unwrap_err();
        assert!(err.0.contains("did not reach ready"), "{err}");
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
    fn test_dispatch_retries_transient_refusal_then_succeeds() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.dispatch_fail_first = 1;
        env.dispatch_err = "transport refused: leader RPC blip".to_string();
        env.replies
            .lock()
            .unwrap()
            .insert("m1".to_string(), reply_row("done", "", "r1"));

        let member = agent(&env, "task", "impl", None, "").unwrap();
        assert_eq!(member.summary, "done");
        assert_eq!(env.send_calls.load(Ordering::SeqCst), 2); // one blip absorbed
    }

    #[test]
    fn test_dispatch_exhaustion_stays_loud() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.dispatch_fail_first = u32::MAX;
        let err = agent(&env, "task", "impl", None, "").unwrap_err();
        assert!(err.0.contains("after 3 attempts"), "{err}");
    }

    #[test]
    fn test_spawn_retries_transient_failure_then_succeeds() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.spawn_fail_first = 1;
        env.spawn_err = "codex app-server refused to mint a thread".to_string();
        env.replies
            .lock()
            .unwrap()
            .insert("m1".to_string(), reply_row("done", "", "r1"));

        let member = agent(&env, "task", "impl", Some("codex"), "").unwrap();
        assert_eq!(member.summary, "done");
        assert_eq!(env.spawn_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_spawn_exhaustion_stays_loud() {
        let tmp = TempDir::new().unwrap();
        let mut env = fake_env(tmp.path());
        env.spawn_fail_first = u32::MAX;
        env.spawn_err = "mint refused".to_string();
        let err = agent(&env, "task", "impl", Some("codex"), "").unwrap_err();
        assert!(err.0.contains("after 3 attempts"), "{err}");
    }

    #[test]
    fn test_agent_rejects_the_whole_mailbox_name_family() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        for name in ["flow", "flow.run", "flow.anything"] {
            let err = agent(&env, "task", name, None, "").unwrap_err();
            assert!(err.0.contains("mailbox address kind"), "{err}");
        }
    }

    // -----------------------------------------------------------------------
    // flow-op bridge
    // -----------------------------------------------------------------------

    use serde_json::json;

    #[test]
    fn test_run_op_covers_the_script_client_protocol() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());

        let r = run_op(&env, "context", &json!({})).unwrap();
        assert_eq!(r.get("teamName"), Some(&Value::String("t-x".into())));
        assert_eq!(
            r.get("workspace"),
            Some(&Value::String(
                env.workspace.to_string_lossy().into_owned()
            ))
        );

        let r = run_op(&env, "spawn", &json!({"name": "impl", "cli": null, "model": ""})).unwrap();
        assert_eq!(r.get("pane"), Some(&Value::String("%1".into())));
        assert_eq!(r.get("cli"), Some(&Value::String("claude".into())));

        run_op(&env, "ready", &json!({"name": "impl", "cli": "claude"})).unwrap();

        let r = run_op(
            &env,
            "dispatch",
            &json!({"name": "impl", "body": "b", "artifact": ""}),
        )
        .unwrap();
        assert_eq!(r.get("msgId"), Some(&Value::String("m1".into())));
        let dispatches = env.dispatches.lock().unwrap();
        assert_eq!(dispatches[0].sender_agent, "flow.run");
        assert_eq!(dispatches[0].target_agent, "impl");
        drop(dispatches);

        env.replies
            .lock()
            .unwrap()
            .insert("m1".to_string(), reply_row("done", "/tmp/f.md", "r1"));
        let r = run_op(&env, "wait-reply", &json!({"name": "impl", "msgId": "m1"})).unwrap();
        assert_eq!(r.get("body"), Some(&Value::String("done".into())));
        assert_eq!(r.get("artifact"), Some(&Value::String("/tmp/f.md".into())));
        assert_eq!(r.get("msgId"), Some(&Value::String("r1".into())));

        env.agents.lock().unwrap().push("impl".to_string());
        run_op(&env, "kill", &json!({"name": "impl"})).unwrap();
        assert_eq!(
            *env.killed.lock().unwrap(),
            vec!["impl".to_string(), "layout dev:0".to_string()]
        );
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
    fn test_run_op_unknown_op_is_loud() {
        let tmp = TempDir::new().unwrap();
        let env = fake_env(tmp.path());
        let err = run_op(&env, "bogus", &json!({})).unwrap_err();
        assert_eq!(err.0, "unknown flow op 'bogus'");
    }

    #[test]
    fn test_materialize_pylib_writes_and_heals_the_tree() {
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path());
        let root = materialize_pylib().unwrap();
        assert!(root.ends_with("core_assets/pylib"));
        assert!(root.join("hive/__init__.py").exists());
        let flow_py = root.join("hive/flow.py");
        let embedded = fs::read_to_string(&flow_py).unwrap();
        assert!(embedded.contains("FLOW_SENDER = \"flow.run\""));
        fs::write(&flow_py, "drifted").unwrap();
        materialize_pylib().unwrap();
        assert_eq!(fs::read_to_string(&flow_py).unwrap(), embedded);
    }
}
