//! flow v2 spike: can rquickjs (QuickJS) host the CCD-workflow-style JS
//! dialect over an async Rust FlowEnv?
//!
//! Proves: `export const meta` static parse without body execution, the JS
//! prelude dialect (agent/parallel/pipeline/phase/log + Member.ask/kill),
//! real Promise semantics (.then chains), determinism poisons (Date.now /
//! Math.random / argless new Date), and — the load-bearing question —
//! whether N concurrent `agent()` calls actually overlap when the ops are
//! async Rust host functions.
//!
//! Not covered (ordinary work, no engine risk): schema validation of
//! replies, journal/resume, real tmux spawns.

use rquickjs::prelude::{Async, Func};
use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, Promise, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- mock env

#[derive(Debug, Clone)]
pub struct OpRecord {
    pub op: String,
    pub name: String,
    pub start: Instant,
    pub end: Instant,
}

#[derive(Default)]
pub struct MockEnv {
    pub ops: Mutex<Vec<OpRecord>>,
    counter: AtomicU64,
    spawn_failures_left: Mutex<HashMap<String, u32>>,
}

impl MockEnv {
    pub fn op_count(&self) -> usize {
        self.ops.lock().unwrap().len()
    }

    pub fn records(&self, op: &str) -> Vec<OpRecord> {
        self.ops
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.op == op)
            .cloned()
            .collect()
    }

    fn next_id(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::SeqCst) + 1
    }
}

fn err_json(msg: &str) -> String {
    serde_json::json!({"ok": false, "error": msg}).to_string()
}

/// The spike stand-in for the production op layer (spawn/ready/dispatch/
/// wait-reply/kill against registry+tmux+bus). Sleeps model real latencies;
/// wait-reply is the long pole so concurrency is measurable.
async fn flow_op(env: Arc<MockEnv>, op: String, args_json: String) -> String {
    let args: serde_json::Value = match serde_json::from_str(&args_json) {
        Ok(v) => v,
        Err(e) => return err_json(&format!("bad op args: {e}")),
    };
    let name = args["name"].as_str().unwrap_or("").to_string();
    let start = Instant::now();

    let result = match op.as_str() {
        "spawn" => {
            if name.starts_with("flaky") {
                let mut budget = env.spawn_failures_left.lock().unwrap();
                let left = budget.entry(name.clone()).or_insert(2);
                if *left > 0 {
                    *left -= 1;
                    drop(budget);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    let rec = OpRecord { op, name, start, end: Instant::now() };
                    env.ops.lock().unwrap().push(rec);
                    return err_json("tmux split raced the registry (injected)");
                }
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
            serde_json::json!({"ok": true, "pane": format!("%{}", env.next_id()), "cli": "claude"})
        }
        "ready" => {
            tokio::time::sleep(Duration::from_millis(10)).await;
            serde_json::json!({"ok": true})
        }
        "dispatch" => {
            tokio::time::sleep(Duration::from_millis(10)).await;
            serde_json::json!({"ok": true, "msgId": format!("m{}", env.next_id())})
        }
        "wait-reply" => {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let msg_id = args["msgId"].as_str().unwrap_or("");
            serde_json::json!({
                "ok": true,
                "body": format!("reply from {name} to {msg_id}"),
                "artifact": "",
                "msgId": format!("m{}", env.next_id()),
            })
        }
        "kill" => {
            tokio::time::sleep(Duration::from_millis(5)).await;
            serde_json::json!({"ok": true})
        }
        other => serde_json::json!({"ok": false, "error": format!("unknown op '{other}'")}),
    };

    let rec = OpRecord { op, name, start, end: Instant::now() };
    env.ops.lock().unwrap().push(rec);
    result.to_string()
}

// ------------------------------------------------------------- meta parse

/// Extract the `export const meta = {...}` literal without executing
/// anything. Balanced-brace scan aware of strings and escapes; meta is
/// required to be a pure literal (no `${}` interpolation), same as CCD.
pub fn extract_meta(src: &str) -> Option<&str> {
    let key = "export const meta";
    let at = src.find(key)?;
    let after = &src[at + key.len()..];
    let eq = after.find('=')?;
    let brace_rel = after[eq..].find('{')?;
    let body = &after[eq + brace_rel..];

    let mut depth = 0usize;
    let mut in_str: Option<char> = None;
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        if let Some(q) = in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                in_str = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => in_str = Some(c),
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&body[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------- prelude

const PRELUDE: &str = r#"
'use strict';
// -- determinism poisons (same contract as CCD workflows: replay safety) --
{
  const RealDate = Date;
  globalThis.Date = new Proxy(RealDate, {
    construct(target, args) {
      if (args.length === 0) throw new Error('new Date() is banned in flow scripts (breaks resume)');
      return Reflect.construct(target, args);
    },
    get(target, prop, receiver) {
      if (prop === 'now') return () => { throw new Error('Date.now() is banned in flow scripts (breaks resume)'); };
      const v = Reflect.get(target, prop, receiver);
      return typeof v === 'function' ? v.bind(target) : v;
    },
  });
  Math.random = () => { throw new Error('Math.random() is banned in flow scripts (breaks resume)'); };
}

globalThis.log = (m) => __host_log(String(m));
globalThis.phase = (t) => __host_log('phase: ' + String(t));

const __op = async (op, args) => {
  const r = JSON.parse(await __flow_op(op, JSON.stringify(args ?? {})));
  if (!r.ok) throw new Error(r.error || (op + ' failed'));
  return r;
};

globalThis.agent = async (prompt, opts = {}) => {
  const name = opts.name;
  if (!name) throw new Error('agent() requires opts.name');
  const spawned = await __op('spawn', { name, cli: opts.cli ?? null, model: opts.model ?? '' });
  await __op('ready', { name, cli: spawned.cli });
  const d = await __op('dispatch', { name, body: prompt });
  log(`${name} dispatched (${d.msgId}); waiting`);
  const reply = await __op('wait-reply', { name, msgId: d.msgId });
  const m = {
    name,
    pane: spawned.pane,
    summary: reply.body ?? '',
    artifact: reply.artifact ?? '',
    msgId: reply.msgId ?? '',
    _dead: false,
    async ask(p) {
      if (m._dead) throw new Error(`member '${name}' was killed; spawn a new one`);
      const dd = await __op('dispatch', { name, body: p });
      const r = await __op('wait-reply', { name, msgId: dd.msgId });
      m.summary = r.body ?? ''; m.artifact = r.artifact ?? ''; m.msgId = r.msgId ?? '';
      return m;
    },
    async kill() {
      await __op('kill', { name });
      m._dead = true;
      log(`${name} retired`);
    },
  };
  return m;
};

// CCD contract: never rejects; a failed branch resolves to null.
globalThis.parallel = (thunks) => Promise.all((thunks ?? []).map((t) =>
  Promise.resolve().then(t).catch((e) => {
    __host_log('parallel branch failed: ' + (e && e.message || e));
    return null;
  })
));

// CCD contract: per-item, no barrier between stages; a throwing stage
// drops the item to null and skips its remaining stages.
globalThis.pipeline = (items, ...stages) => Promise.all((items ?? []).map(async (item, i) => {
  let prev = item;
  for (const s of stages) {
    try { prev = await s(prev, item, i); }
    catch (e) {
      __host_log(`pipeline item ${i} dropped: ` + (e && e.message || e));
      return null;
    }
  }
  return prev;
}));
"#;

// ----------------------------------------------------------------- runner

pub struct RunOutcome {
    pub meta: serde_json::Value,
    pub result: serde_json::Value,
    pub host_log: Vec<String>,
}

pub async fn run_script(env: Arc<MockEnv>, src: &str) -> anyhow::Result<RunOutcome> {
    let meta_src = extract_meta(src)
        .ok_or_else(|| anyhow::anyhow!("script must start with `export const meta = {{...}}`"))?;
    let body = src.replacen("export const meta", "const meta", 1);

    let rt = AsyncRuntime::new()?;
    let ctx = AsyncContext::full(&rt).await?;
    let host_log: Arc<Mutex<Vec<String>>> = Arc::default();

    let (meta_json, result_json, log_out) = ctx
        .async_with(async |ctx| -> anyhow::Result<(String, String, Vec<String>)> {
            let globals = ctx.globals();
            let log_sink = host_log.clone();
            globals.set(
                "__host_log",
                Func::from(move |msg: String| {
                    println!("[flow] {msg}");
                    log_sink.lock().unwrap().push(msg);
                }),
            )?;
            let op_env = env.clone();
            globals.set(
                "__flow_op",
                Func::from(Async(move |op: String, args: String| {
                    let env = op_env.clone();
                    async move { flow_op(env, op, args).await }
                })),
            )?;

            ctx.eval::<(), _>(PRELUDE).catch(&ctx).map_err(stringify_err)?;

            let meta_json: String = ctx
                .eval(format!("JSON.stringify(({meta_src}))"))
                .catch(&ctx)
                .map_err(stringify_err)?;

            let wrapped = format!("(async () => {{\n{body}\n}})()");
            let promise: Promise = ctx.eval(wrapped).catch(&ctx).map_err(stringify_err)?;
            let result: Value = promise.into_future().await.catch(&ctx).map_err(stringify_err)?;
            let result_json = match ctx.json_stringify(result).catch(&ctx).map_err(stringify_err)? {
                Some(s) => s.to_string()?,
                None => "null".to_string(),
            };

            let logs = host_log.lock().unwrap().clone();
            Ok((meta_json, result_json, logs))
        })
        .await?;

    rt.idle().await;

    Ok(RunOutcome {
        meta: serde_json::from_str(&meta_json)?,
        result: serde_json::from_str(&result_json)?,
        host_log: log_out,
    })
}

fn stringify_err(e: rquickjs::CaughtError<'_>) -> anyhow::Error {
    anyhow::anyhow!("js error: {e}")
}

// ------------------------------------------------------------------- main

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "scripts/demo.js".to_string());
    let src = std::fs::read_to_string(&path)?;
    let env = Arc::new(MockEnv::default());
    let started = Instant::now();
    let out = run_script(env.clone(), &src).await?;
    println!("--- meta ---\n{}", serde_json::to_string_pretty(&out.meta)?);
    println!("--- result ---\n{}", serde_json::to_string_pretty(&out.result)?);
    println!(
        "--- {} ops, wall {:?} ---",
        env.op_count(),
        started.elapsed()
    );
    Ok(())
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Arc<MockEnv> {
        Arc::new(MockEnv::default())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_meta_parses_without_executing_body() {
        let src = r#"export const meta = {
  name: 'meta-only',
  description: 'body must not run: {braces} and "quotes} inside strings',
  phases: [{ title: 'P', detail: 'x = {y}' }],
}
await agent('should never spawn', { name: 'ghost' })
return 1"#;
        let meta_src = extract_meta(src).expect("meta literal found");
        assert!(meta_src.starts_with('{') && meta_src.ends_with('}'));
        assert!(meta_src.contains("quotes}"));
        // Nothing was executed: extraction is pure text work.
        let e = env();
        assert_eq!(e.op_count(), 0);
        // And the extracted literal evaluates standalone to the right object.
        let rt = AsyncRuntime::new().unwrap();
        let ctx = AsyncContext::full(&rt).await.unwrap();
        let name: String = ctx
            .async_with(async |ctx| {
                ctx.eval::<String, _>(format!("(({meta_src})).name")).unwrap()
            })
            .await;
        assert_eq!(name, "meta-only");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_parallel_agents_actually_overlap() {
        let src = r#"export const meta = { name: 'conc', description: 'overlap probe', phases: [] }
const members = await parallel([
  () => agent('t1', { name: 'c1' }),
  () => agent('t2', { name: 'c2' }),
  () => agent('t3', { name: 'c3' }),
])
return members.filter(Boolean).length"#;
        let e = env();
        let started = Instant::now();
        let out = run_script(e.clone(), src).await.unwrap();
        let wall = started.elapsed();
        assert_eq!(out.result, serde_json::json!(3));
        // Sequential would be 3 x (40+10+10+300) = 1080ms+. Concurrent ~360ms.
        assert!(
            wall < Duration::from_millis(700),
            "agents serialized: wall {wall:?}"
        );
        // The three wait-reply windows must overlap pairwise.
        let waits = e.records("wait-reply");
        assert_eq!(waits.len(), 3);
        let latest_start = waits.iter().map(|r| r.start).max().unwrap();
        let earliest_end = waits.iter().map(|r| r.end).min().unwrap();
        assert!(
            latest_start < earliest_end,
            "wait-reply ops did not overlap"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_torture_script_exercises_dialect_surface() {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/torture.js"),
        )
        .unwrap();
        let out = run_script(env(), &src).await.unwrap();
        let checks = out.result.as_object().expect("torture returns object");
        assert!(!checks.is_empty());
        for (check, verdict) in checks {
            assert_ne!(
                verdict.as_str().unwrap_or(""),
                "FAIL",
                "torture check '{check}' failed: {:?}",
                out.result
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_demo_script_runs_end_to_end() {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/demo.js"),
        )
        .unwrap();
        let e = env();
        let out = run_script(e.clone(), &src).await.unwrap();
        assert_eq!(out.meta["name"], "demo-auth-review");
        assert_eq!(out.result["reviews"].as_array().unwrap().len(), 3);
        // scout spawned, 3 reviewers spawned = 4 spawns; scout killed once.
        assert_eq!(e.records("spawn").len(), 4);
        assert_eq!(e.records("kill").len(), 1);
        // follow-up ask happened: 4 initial dispatches + 1 ask.
        assert_eq!(e.records("dispatch").len(), 5);
        assert!(out.host_log.iter().any(|l| l.starts_with("phase: ")));
    }
}
