//! hive::flow_script — the JavaScript surface of the flow engine.
//!
//! `hive flow run script.js` evaluates the script in an embedded QuickJS
//! runtime. The dialect mirrors Claude Code's workflow scripts: the file
//! starts with a pure-literal `export const meta = {...}`, the body runs in
//! an async context (top-level `await` and `return` both work), and the
//! surface is `agent()` / `ask()` / `kill()` / `parallel()` / `pipeline()`
//! / `phase()` / `log()`. The whole dialect lives in the embedded prelude
//! below; the only host primitive is `__flow_op(json)`, which parses into a
//! typed `flow::FlowOp` and runs on a blocking thread — concurrent
//! `agent()` calls overlap while the JS side stays single-threaded
//! (`parallel()` is `Promise.all`).
//!
//! Determinism contract (same as Claude Code workflows): `Date.now()`,
//! `Math.random()`, argless `new Date()` and `Date()` throw inside the
//! script, so a resumed run replays the same op sequence. Every run appends
//! each successful op to a journal (`<workspace>/artifacts/flow/<runId>.jsonl`,
//! keyed by the op's canonical serialization); `--resume <runId>` replays
//! cached results for the unchanged prefix. Spawn replay probes liveness
//! first: a member still alive is reused (a changed prompt then becomes a
//! live dispatch to the surviving member); a dead member respawns, and
//! every later op on that member bypasses the stale cache.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rquickjs::prelude::{Async, Func};
use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, Promise, Value as JsValue};
use serde_json::{Map, Value};

use crate::flow::{log, run_op, FlowEnv, FlowOp};

const DIALECT_HELP: &str =
    "flow scripts are JavaScript: start with `export const meta = { name: '...', description: '...' }` \
     (see the hive skill's orchestration reference for the dialect)";

// ---------------------------------------------------------------------------
// meta: extracted without executing anything. Balanced-brace scan aware of
// strings and escapes; meta must be a pure literal (no `${}` interpolation).
// ---------------------------------------------------------------------------

fn extract_meta(src: &str) -> Option<&str> {
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

// ---------------------------------------------------------------------------
// journal
// ---------------------------------------------------------------------------

/// Op-level resume journal keyed by `FlowOp::key()`. Values queue per key in
/// record order (the same op can legitimately repeat). Failed ops are never
/// recorded: on resume they simply run again.
pub struct Journal {
    cached: Mutex<HashMap<String, VecDeque<Value>>>,
    fresh: Mutex<HashSet<String>>,
    writer: Mutex<fs::File>,
    /// On resume the rewritten journal streams to a sibling `.tmp` file and
    /// only replaces the original in `finalize()` — a resume that dies
    /// mid-replay leaves the prior journal intact.
    live_path: PathBuf,
    tmp_path: Option<PathBuf>,
    write_failed: std::sync::atomic::AtomicBool,
}

impl Journal {
    pub fn open(path: &Path, resume: bool) -> anyhow::Result<Journal> {
        let mut cached: HashMap<String, VecDeque<Value>> = HashMap::new();
        if resume {
            let text = fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("cannot read journal {}: {e}", path.display()))?;
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                let row: Value = serde_json::from_str(line)
                    .map_err(|e| anyhow::anyhow!("corrupt journal line: {e}"))?;
                if let (Some(k), Some(r)) = (row.get("k").and_then(Value::as_str), row.get("r")) {
                    cached
                        .entry(k.to_string())
                        .or_default()
                        .push_back(r.clone());
                }
            }
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp_path = resume.then(|| path.with_extension("jsonl.tmp"));
        let writer = fs::File::create(tmp_path.as_deref().unwrap_or(path))?;
        Ok(Journal {
            cached: Mutex::new(cached),
            fresh: Mutex::new(HashSet::new()),
            writer: Mutex::new(writer),
            live_path: path.to_path_buf(),
            tmp_path,
            write_failed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Publish a resumed run's rewritten journal over the original. Call
    /// once the run has finished, successfully or not — either way the temp
    /// file holds every op that completed.
    pub fn finalize(&self) {
        if let Some(tmp) = &self.tmp_path {
            if let Err(e) = fs::rename(tmp, &self.live_path) {
                eprintln!(
                    "[flow] warning: could not publish resumed journal {}: {e}",
                    self.live_path.display()
                );
            }
        }
    }

    fn take(&self, key: &str) -> Option<Value> {
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        cached.get_mut(key)?.pop_front()
    }

    fn record(&self, key: &str, result: &Value) {
        let line = serde_json::json!({"k": key, "r": result});
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let outcome = writeln!(writer, "{line}").and_then(|_| writer.flush());
        if let Err(e) = outcome {
            if !self
                .write_failed
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                eprintln!(
                    "[flow] warning: journal write failed ({e}); this run may not resume cleanly"
                );
            }
        }
    }

    fn is_fresh(&self, name: &str) -> bool {
        self.fresh
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(name)
    }

    fn mark_fresh(&self, name: &str) {
        self.fresh
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_string());
    }
}

// ---------------------------------------------------------------------------
// the op bridge
// ---------------------------------------------------------------------------

fn ok_json(result: &Value) -> String {
    let mut map = match result {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    map.insert("ok".to_string(), Value::Bool(true));
    Value::Object(map).to_string()
}

fn err_json(msg: &str) -> String {
    serde_json::json!({"ok": false, "error": msg}).to_string()
}

/// One `__flow_op` call from the script: journal replay when possible,
/// otherwise `run_op` on a blocking thread.
async fn journaled_op(env: Arc<dyn FlowEnv>, journal: Arc<Journal>, op_json: String) -> String {
    let op: FlowOp = match serde_json::from_str(&op_json) {
        Ok(op) => op,
        Err(e) => return err_json(&format!("bad flow op {op_json}: {e}")),
    };
    let name = op.member().to_string();
    let key = op.key();

    if op.journaled() && !journal.is_fresh(&name) {
        if let Some(rec) = journal.take(&key) {
            if op.is_spawn() {
                let alive = {
                    let env = env.clone();
                    let n = name.clone();
                    tokio::task::spawn_blocking(move || env.alive(&n))
                        .await
                        .unwrap_or(false)
                };
                if alive {
                    log(&format!("{name} alive from previous run; reusing"));
                    journal.record(&key, &rec);
                    return ok_json(&rec);
                }
                log(&format!("{name} from journal is gone; respawning"));
            } else {
                journal.record(&key, &rec);
                return ok_json(&rec);
            }
        }
    }
    // Any live spawn makes the member fresh: later cached ops for it must
    // not replay against a member that never saw them.
    if op.is_spawn() {
        journal.mark_fresh(&name);
    }

    let outcome = {
        let env = env.clone();
        let op = op.clone();
        tokio::task::spawn_blocking(move || run_op(&*env, &op)).await
    };
    match outcome {
        Ok(Ok(map)) => {
            let value = Value::Object(map);
            if op.journaled() {
                journal.record(&key, &value);
            }
            ok_json(&value)
        }
        Ok(Err(e)) => err_json(&e.0),
        Err(join) => err_json(&format!("flow op panicked: {join}")),
    }
}

// ---------------------------------------------------------------------------
// the dialect prelude — the entire script-facing API over the one host
// primitive `__flow_op(json)`; `__host_log(msg)` is only the stderr sink
// (`log()`, `phase()`, and the parallel/pipeline failure notices).
// ---------------------------------------------------------------------------

const PRELUDE: &str = r###"
'use strict';
// -- determinism poisons: a resumed run must replay the same op sequence.
// A guardrail, not a sandbox: prototype-level escapes still reach the clock.
{
  const RealDate = Date;
  globalThis.Date = new Proxy(RealDate, {
    apply() { throw new Error('Date() is banned in flow scripts (breaks resume)'); },
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

// The current phase rides every spawn as the pane group, so the board can
// draw the topology from the roster instead of a hand-written sidecar.
let __phase = '';
globalThis.phase = (t) => { __phase = String(t); __host_log('phase: ' + __phase); };

const __op = async (op) => {
  const r = JSON.parse(await __flow_op(JSON.stringify(op)));
  if (!r.ok) throw new Error(r.error || (op.op + ' failed'));
  return r;
};

// -- structured replies: JSON Schema validator over the keywords flow
// schemas use; anything else is refused loudly rather than ignored.
const __KNOWN = new Set(['type', 'properties', 'required', 'items', 'enum', 'description', 'title', 'additionalProperties']);
const __validate = (schema, value, path = '$') => {
  const errs = [];
  if (!schema || typeof schema !== 'object') return errs;
  for (const k of Object.keys(schema)) {
    if (!__KNOWN.has(k)) throw new Error(`schema keyword '${k}' at ${path} is not supported by flow (supported: ${[...__KNOWN].join(', ')})`);
  }
  const typeOf = (v) => Array.isArray(v) ? 'array' : v === null ? 'null' : typeof v;
  const t = schema.type;
  if (t) {
    const ok = t === 'integer' ? (typeof value === 'number' && Number.isInteger(value)) : typeOf(value) === t;
    if (!ok) { errs.push(`${path}: expected ${t}, got ${typeOf(value)}`); return errs; }
  }
  if (Array.isArray(schema.enum) && !schema.enum.some((e) => e === value)) {
    errs.push(`${path}: not one of [${schema.enum.join(', ')}]`);
  }
  if (schema.properties || schema.required) {
    for (const k of schema.required ?? []) {
      if (value === null || typeof value !== 'object' || !(k in value)) errs.push(`${path}.${k}: required`);
    }
    for (const [k, sub] of Object.entries(schema.properties ?? {})) {
      if (value && typeof value === 'object' && k in value) errs.push(...__validate(sub, value[k], `${path}.${k}`));
    }
  }
  if (schema.items && Array.isArray(value)) {
    value.forEach((v, i) => errs.push(...__validate(schema.items, v, `${path}[${i}]`)));
  }
  return errs;
};

const __parseReplyJson = (text) => {
  let t = String(text).trim();
  const fence = t.match(/^```(?:json)?\s*([\s\S]*?)\s*```$/);
  if (fence) t = fence[1].trim();
  return JSON.parse(t);
};

const __schemaClause = (schema) =>
  '\n\n## 回信格式(硬性)\n回报给 flow.run 的消息 body 必须是一个符合下述 JSON Schema 的纯 JSON——不带 markdown code fence,不带任何多余文字:\n'
  + JSON.stringify(schema, null, 2);

const SCHEMA_REASKS = 2;

// Wait for `name`'s reply to msgId. Without a schema the reply is
// {body, artifact, msgId}; with one, the validated object (re-asked twice
// on mismatch, then thrown). Fresh values every time — nothing to mutate.
const __collect = async (name, msgId, schema) => {
  let reply = await __op({ op: 'wait-reply', name, msg_id: msgId });
  if (!schema) return { body: reply.body ?? '', artifact: reply.artifact ?? '', msgId: reply.msgId ?? '' };
  for (let attempt = 0; ; attempt++) {
    let errs;
    try {
      const data = __parseReplyJson(reply.body);
      errs = __validate(schema, data);
      if (errs.length === 0) return data;
    } catch (e) {
      if (e instanceof SyntaxError) errs = ['body 不是可解析的 JSON: ' + e.message]; else throw e;
    }
    if (attempt >= SCHEMA_REASKS) {
      throw new Error(`member '${name}' 的回信连续 ${SCHEMA_REASKS + 1} 次不符合 schema: ${errs.join('; ')}`);
    }
    log(`${name} 回信不符合 schema(${errs.join('; ')}); 重新要求 (${attempt + 1}/${SCHEMA_REASKS})`);
    const d = await __op({ op: 'dispatch-ask', name, prompt: `回信不符合要求: ${errs.join('; ')}。重发一条消息,body 为纯 JSON(不带 code fence),符合以下 JSON Schema。${__schemaClause(schema)}` });
    reply = await __op({ op: 'wait-reply', name, msg_id: d.msgId });
  }
};

// Spawn a member, dispatch `prompt` as its task, block for its reply. The
// prompt is the whole contract — write it self-contained. Members are
// addressed by name afterwards: ask(name, ...) / kill(name).
globalThis.agent = async (prompt, opts = {}) => {
  const name = opts.name;
  if (!name) throw new Error('agent() requires opts.name — every flow member gets a stable name');
  const spawned = await __op({ op: 'spawn', name, cli: opts.cli ?? null, model: opts.model ?? '', group: __phase });
  log(`${name} spawned in ${spawned.pane}`);
  await __op({ op: 'ready', name, cli: spawned.cli });
  const fullPrompt = opts.schema ? prompt + __schemaClause(opts.schema) : prompt;
  const d = await __op({ op: 'dispatch-task', name, prompt: fullPrompt });
  log(`${name} dispatched (${d.msgId}); waiting for reply…`);
  const out = await __collect(name, d.msgId, opts.schema);
  log(`${name} replied`);
  return out;
};

// Follow-up to a living member (question, rework order); it keeps its full
// context — what a dead headless subagent cannot do.
globalThis.ask = async (name, prompt, opts = {}) => {
  const fp = opts.schema ? prompt + __schemaClause(opts.schema) : prompt;
  const d = await __op({ op: 'dispatch-ask', name, prompt: fp });
  log(`${name} asked (${d.msgId}); waiting…`);
  const out = await __collect(name, d.msgId, opts.schema);
  log(`${name} answered`);
  return out;
};

// Retire the member's pane; the window re-tiles.
globalThis.kill = async (name) => {
  await __op({ op: 'kill', name });
  log(`${name} retired`);
};

// Run thunks concurrently; a failed branch resolves to null, never rejects.
globalThis.parallel = (thunks) => Promise.all((thunks ?? []).map((t) =>
  Promise.resolve().then(t).catch((e) => {
    __host_log('parallel branch failed: ' + (e && e.message || e));
    return null;
  })
));

// Per-item pipeline, no barrier between stages; a throwing stage drops the
// item to null and skips its remaining stages. Stages receive
// (prev, originalItem, index).
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
"###;

// ---------------------------------------------------------------------------
// runner
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct RunOutcome {
    pub meta: Value,
    pub result: Value,
}

fn stringify_err(e: rquickjs::CaughtError<'_>) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

pub async fn run_script(
    env: Arc<dyn FlowEnv>,
    journal: Arc<Journal>,
    src: &str,
) -> anyhow::Result<RunOutcome> {
    let meta_src = extract_meta(src).ok_or_else(|| anyhow::anyhow!("{DIALECT_HELP}"))?;
    if meta_src.contains("${") {
        anyhow::bail!(
            "meta must be a pure literal — no `${{}}` interpolation inside `export const meta`"
        );
    }
    let body = src.replacen("export const meta", "const meta", 1);

    let rt = AsyncRuntime::new()?;
    let ctx = AsyncContext::full(&rt).await?;

    let (meta_json, result_json) = ctx
        .async_with(async |ctx| -> anyhow::Result<(String, String)> {
            let globals = ctx.globals();
            globals.set("__host_log", Func::from(|msg: String| log(&msg)))?;
            globals.set(
                "__flow_op",
                Func::from(Async(move |op_json: String| {
                    let env = env.clone();
                    let journal = journal.clone();
                    async move { journaled_op(env, journal, op_json).await }
                })),
            )?;

            ctx.eval::<(), _>(PRELUDE)
                .catch(&ctx)
                .map_err(stringify_err)?;

            let meta_json: String = ctx
                .eval(format!("JSON.stringify(({meta_src}))"))
                .catch(&ctx)
                .map_err(stringify_err)?;

            let wrapped = format!("(async () => {{\n{body}\n}})()");
            let promise: Promise = ctx.eval(wrapped).catch(&ctx).map_err(stringify_err)?;
            let result: JsValue = promise
                .into_future()
                .await
                .catch(&ctx)
                .map_err(stringify_err)?;
            let result_json = match ctx
                .json_stringify(result)
                .catch(&ctx)
                .map_err(stringify_err)?
            {
                Some(s) => s.to_string()?,
                None => "null".to_string(),
            };
            Ok((meta_json, result_json))
        })
        .await?;

    rt.idle().await;

    Ok(RunOutcome {
        meta: serde_json::from_str(&meta_json)?,
        result: serde_json::from_str(&result_json)?,
    })
}

/// `hive flow run` body. Returns the process exit code.
pub fn run_cmd(script_path: &str, resume: Option<&str>) -> i32 {
    let src = match fs::read_to_string(script_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: cannot read {script_path}: {e}");
            return 1;
        }
    };
    // Checked again inside run_script; here it runs before a run id is
    // minted and a journal file is created, so a bad script leaves nothing
    // on disk that looks resumable.
    if extract_meta(&src).is_none() {
        eprintln!("Error: {DIALECT_HELP}");
        return 1;
    }
    let env: Arc<dyn FlowEnv> = Arc::new(crate::flow::RealEnv::new());
    let workspace = match env.context() {
        Ok(ctx) => ctx.workspace,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    let run_id = match resume {
        Some(id) => id.to_string(),
        None => {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("flow-{secs}-{}", std::process::id())
        }
    };
    let journal_path = journal_path(&workspace, &run_id);
    if resume.is_some() && !journal_path.exists() {
        eprintln!(
            "Error: no journal for run '{run_id}' ({})",
            journal_path.display()
        );
        return 1;
    }
    let journal = match Journal::open(&journal_path, resume.is_some()) {
        Ok(j) => Arc::new(j),
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    log(&format!(
        "run {run_id}{} — resume with: hive flow run {script_path} --resume {run_id}",
        if resume.is_some() { " (resumed)" } else { "" }
    ));

    let rt = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let outcome = rt.block_on(run_script(env, journal.clone(), &src));
    journal.finalize();
    match outcome {
        Ok(outcome) => {
            log("result:");
            println!(
                "{}",
                serde_json::to_string_pretty(&outcome.result).unwrap_or_else(|_| "null".into())
            );
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

pub fn journal_path(workspace: &str, run_id: &str) -> PathBuf {
    Path::new(workspace)
        .join("artifacts")
        .join("flow")
        .join(format!("{run_id}.jsonl"))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::test_env::*;
    use std::sync::atomic::Ordering;
    use tempfile::TempDir;

    fn scratch_journal(tmp: &Path) -> Arc<Journal> {
        Arc::new(Journal::open(&tmp.join("journal.jsonl"), false).unwrap())
    }

    async fn run(
        env: Arc<FakeEnv>,
        journal: Arc<Journal>,
        src: &str,
    ) -> anyhow::Result<RunOutcome> {
        let outcome = run_script(env, journal.clone(), src).await;
        journal.finalize();
        outcome
    }

    #[tokio::test]
    async fn test_script_without_meta_is_rejected_with_dialect_help() {
        let tmp = TempDir::new().unwrap();
        let env = Arc::new(fake_env(tmp.path()));
        let err = run(env, scratch_journal(tmp.path()), "return 1")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("export const meta"), "{err}");
    }

    #[tokio::test]
    async fn test_meta_interpolation_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let env = Arc::new(fake_env(tmp.path()));
        let src = "export const meta = { name: `run-${1}`, description: 'x' }\nreturn 1";
        let err = run(env, scratch_journal(tmp.path()), src)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("pure literal"), "{err}");
    }

    #[tokio::test]
    async fn test_dialect_end_to_end_with_phases_parallel_ask_and_kill() {
        let tmp = TempDir::new().unwrap();
        let mut fake = fake_env(tmp.path());
        fake.reply_any = true;
        let env = Arc::new(fake);
        let src = r#"export const meta = { name: 'demo-review', description: 'scout, fan-out, follow-up ask' }
phase('Explore')
const scout = await agent('探索认证模块,列出改动面', { name: 'scout' })
phase('Review')
const reviews = await parallel([
  () => agent(`复查安全面:\n${scout.body}`, { name: 'sec' }),
  () => agent(`复查性能面:\n${scout.body}`, { name: 'perf' }),
  () => agent(`复查测试面:\n${scout.body}`, { name: 'tests', cli: 'codex' }),
])
const ok = reviews.filter(Boolean)
const followup = await ask('sec', '第一条发现给出修复建议')
await kill('scout')
return { scout: scout.body, reviews: ok.length, followup: followup.body }"#;
        let out = run(env.clone(), scratch_journal(tmp.path()), src)
            .await
            .unwrap();
        assert_eq!(out.meta["name"], "demo-review");
        assert_eq!(out.result["reviews"], 3);
        assert!(out.result["scout"].as_str().unwrap().starts_with("done-"));
        assert!(out.result["followup"]
            .as_str()
            .unwrap()
            .starts_with("done-"));
        let spawns = env.spawns.lock().unwrap();
        assert_eq!(spawns.len(), 4);
        // the phase rides each spawn as its pane group
        assert_eq!(spawns[0].group, "Explore");
        assert!(spawns[1..].iter().all(|s| s.group == "Review"));
        assert_eq!(env.dispatches.lock().unwrap().len(), 5); // 4 tasks + 1 ask
        assert_eq!(*env.retired.lock().unwrap(), vec!["scout".to_string()]);
    }

    #[tokio::test]
    async fn test_dialect_contracts_poisons_nulls_and_gone() {
        let tmp = TempDir::new().unwrap();
        let mut fake = fake_env(tmp.path());
        fake.reply_any = true;
        let env = Arc::new(fake);
        let src = r#"export const meta = { name: 'torture', description: 'dialect contracts' }
const checks = {}
try { Date.now(); checks.dateNow = 'FAIL' } catch (e) { checks.dateNow = 'ok' }
try { Date(); checks.dateCall = 'FAIL' } catch (e) { checks.dateCall = 'ok' }
try { Math.random(); checks.mathRandom = 'FAIL' } catch (e) { checks.mathRandom = 'ok' }
try { new Date(); checks.newDate = 'FAIL' } catch (e) { checks.newDate = 'ok' }
checks.datedCtor = new Date(0).getTime() === 0 ? 'ok' : 'FAIL'
const pr = await parallel([
  () => agent('good', { name: 'p-ok' }),
  () => { throw new Error('sync boom') },
  async () => { throw new Error('async boom') },
])
checks.parallelNulls = pr.length === 3 && pr[0] !== null && pr[1] === null && pr[2] === null ? 'ok' : 'FAIL'
const pl = await pipeline([1, 2, 3],
  async (n) => { if (n === 2) throw new Error('drop'); return n * 10 },
  async (prev, item, i) => `${item}:${prev}:${i}`,
)
checks.pipelineDrop = JSON.stringify(pl) === JSON.stringify(['1:10:0', null, '3:30:2']) ? 'ok' : 'FAIL'
checks.thenChain = await agent('chain', { name: 'chain' }).then((r) => ask('chain', 'more')).then((r) => r.body.length > 0 ? 'ok' : 'FAIL')
await kill('chain')
try { await ask('chain', 'speak'); checks.deadGuard = 'FAIL' } catch (e) { checks.deadGuard = /gone/.test(e.message) ? 'ok' : 'FAIL:' + e.message }
try { __validate({ type: 'object', minProperties: 1 }, {}); checks.strictSchema = 'FAIL' } catch (e) { checks.strictSchema = /not supported/.test(e.message) ? 'ok' : 'FAIL' }
let rounds = 0, dry = 0
while (dry < 2) { rounds++; if (rounds > 1) dry++; if (rounds > 10) break }
checks.whileLoop = rounds === 3 ? 'ok' : 'FAIL'
return checks"#;
        let out = run(env, scratch_journal(tmp.path()), src).await.unwrap();
        for (check, verdict) in out.result.as_object().unwrap() {
            assert_eq!(verdict, "ok", "check '{check}': {:?}", out.result);
        }
    }

    #[tokio::test]
    async fn test_schema_returns_the_validated_object_after_reasks() {
        let tmp = TempDir::new().unwrap();
        let fake = fake_env(tmp.path());
        {
            let mut replies = fake.replies.lock().unwrap();
            replies.insert("m1".into(), reply_row("not json at all", "", "r1"));
            replies.insert(
                "m2".into(),
                reply_row(
                    "```json\n{\"verdict\": \"pass\", \"score\": 3}\n```",
                    "",
                    "r2",
                ),
            );
            replies.insert("m3".into(), reply_row("plain follow-up", "", "r3"));
        }
        let env = Arc::new(fake);
        let src = r#"export const meta = { name: 's', description: 'schema' }
const verdict = await agent('judge it', { name: 'judge', schema: {
  type: 'object',
  required: ['verdict', 'score'],
  properties: { verdict: { type: 'string', enum: ['pass', 'fail'] }, score: { type: 'integer' } },
} })
const note = await ask('judge', 'thanks, one more note')
return { verdict, note }"#;
        let out = run(env.clone(), scratch_journal(tmp.path()), src)
            .await
            .unwrap();
        assert_eq!(out.result["verdict"]["verdict"], "pass");
        assert_eq!(out.result["verdict"]["score"], 3);
        assert_eq!(out.result["note"]["body"], "plain follow-up");
        let d = env.dispatches.lock().unwrap();
        assert_eq!(d.len(), 3); // task + re-ask + ask
        assert!(fs::read_to_string(&d[0].artifact)
            .unwrap()
            .contains("JSON Schema"));
        let re_ask = if d[1].artifact.is_empty() {
            d[1].body.clone()
        } else {
            fs::read_to_string(&d[1].artifact).unwrap()
        };
        assert!(re_ask.contains("JSON Schema"));
    }

    #[tokio::test]
    async fn test_schema_exhaustion_is_loud() {
        let tmp = TempDir::new().unwrap();
        let fake = fake_env(tmp.path());
        {
            let mut replies = fake.replies.lock().unwrap();
            replies.insert("m1".into(), reply_row("junk", "", "r1"));
            replies.insert("m2".into(), reply_row("more junk", "", "r2"));
            replies.insert("m3".into(), reply_row("{\"wrong\": true}", "", "r3"));
        }
        let env = Arc::new(fake);
        let src = r#"export const meta = { name: 's', description: 'schema exhaustion' }
await agent('judge it', { name: 'judge', schema: { type: 'object', required: ['verdict'] } })
return 'unreachable'"#;
        let err = run(env.clone(), scratch_journal(tmp.path()), src)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("不符合 schema"), "{err}");
        assert_eq!(env.dispatches.lock().unwrap().len(), 3);
    }

    const RESUME_SRC: &str = r#"export const meta = { name: 'resume', description: 'journal probe' }
const r = await agent('solo task', { name: 'solo' })
return r.body"#;

    #[tokio::test]
    async fn test_journal_replays_unchanged_run_without_new_ops() {
        let tmp = TempDir::new().unwrap();
        let journal_file = tmp.path().join("j.jsonl");
        let mut fake = fake_env(tmp.path());
        fake.reply_any = true;
        let env1 = Arc::new(fake);
        let out1 = run(
            env1,
            Arc::new(Journal::open(&journal_file, false).unwrap()),
            RESUME_SRC,
        )
        .await
        .unwrap();

        let fake2 = fake_env(tmp.path());
        fake2.agents.lock().unwrap().push("solo".to_string()); // survived
        let env2 = Arc::new(fake2);
        let out2 = run(
            env2.clone(),
            Arc::new(Journal::open(&journal_file, true).unwrap()),
            RESUME_SRC,
        )
        .await
        .unwrap();

        assert_eq!(out1.result, out2.result);
        assert_eq!(env2.spawn_calls.load(Ordering::SeqCst), 0);
        assert_eq!(env2.send_calls.load(Ordering::SeqCst), 0);
        assert!(env2.awaits.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_journal_changed_prompt_redispatches_to_living_member() {
        let tmp = TempDir::new().unwrap();
        let journal_file = tmp.path().join("j.jsonl");
        let mut fake = fake_env(tmp.path());
        fake.reply_any = true;
        run(
            Arc::new(fake),
            Arc::new(Journal::open(&journal_file, false).unwrap()),
            RESUME_SRC,
        )
        .await
        .unwrap();

        let mut fake2 = fake_env(tmp.path());
        fake2.reply_any = true;
        fake2.agents.lock().unwrap().push("solo".to_string());
        let env2 = Arc::new(fake2);
        let changed = RESUME_SRC.replace("solo task", "revised task");
        run(
            env2.clone(),
            Arc::new(Journal::open(&journal_file, true).unwrap()),
            &changed,
        )
        .await
        .unwrap();

        assert_eq!(env2.spawn_calls.load(Ordering::SeqCst), 0);
        assert_eq!(env2.send_calls.load(Ordering::SeqCst), 1);
        let d = env2.dispatches.lock().unwrap();
        assert_eq!(fs::read_to_string(&d[0].artifact).unwrap(), "revised task");
    }

    #[tokio::test]
    async fn test_journal_respawns_dead_member_and_bypasses_stale_cache() {
        let tmp = TempDir::new().unwrap();
        let journal_file = tmp.path().join("j.jsonl");
        let mut fake = fake_env(tmp.path());
        fake.reply_any = true;
        run(
            Arc::new(fake),
            Arc::new(Journal::open(&journal_file, false).unwrap()),
            RESUME_SRC,
        )
        .await
        .unwrap();

        let mut fake2 = fake_env(tmp.path());
        fake2.reply_any = true; // member gone: respawn + redispatch live
        let env2 = Arc::new(fake2);
        run(
            env2.clone(),
            Arc::new(Journal::open(&journal_file, true).unwrap()),
            RESUME_SRC,
        )
        .await
        .unwrap();

        assert_eq!(env2.spawn_calls.load(Ordering::SeqCst), 1);
        assert_eq!(env2.send_calls.load(Ordering::SeqCst), 1);
        assert_eq!(env2.awaits.lock().unwrap().len(), 1);
    }

    #[test]
    fn test_resume_abandoned_before_finalize_preserves_the_original_journal() {
        let tmp = TempDir::new().unwrap();
        let journal_file = tmp.path().join("j.jsonl");
        fs::write(
            &journal_file,
            "{\"k\":\"{\\\"op\\\":\\\"spawn\\\"}\",\"r\":{\"pane\":\"%1\"}}\n{\"k\":\"w\",\"r\":{\"body\":\"done\"}}\n",
        )
        .unwrap();
        let original = fs::read_to_string(&journal_file).unwrap();

        let j = Journal::open(&journal_file, true).unwrap();
        j.record("{\"op\":\"spawn\"}", &serde_json::json!({"pane": "%1"}));
        drop(j);
        assert_eq!(fs::read_to_string(&journal_file).unwrap(), original);

        let j = Journal::open(&journal_file, true).unwrap();
        j.record("{\"op\":\"spawn\"}", &serde_json::json!({"pane": "%2"}));
        j.finalize();
        let published = fs::read_to_string(&journal_file).unwrap();
        assert!(published.contains("%2"));
        assert!(!published.contains("done"));
    }

    #[test]
    fn test_extract_meta_handles_braces_inside_strings() {
        let src = r#"export const meta = {
  name: 'x',
  description: 'strings with } and { and "quoted }" and `tick }`',
}
return 1"#;
        let meta = extract_meta(src).unwrap();
        assert!(meta.starts_with('{') && meta.ends_with('}'));
        assert!(meta.contains("tick }"));
        assert!(extract_meta("const meta = {}").is_none());
    }
}
