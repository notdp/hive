//! hive::flow_script — the JavaScript surface of the flow engine.
//!
//! `hive flow run script.js` evaluates the script in an embedded QuickJS
//! runtime. The dialect mirrors Claude Code's workflow scripts: the file
//! starts with a pure-literal `export const meta = {...}`, the body runs in
//! an async context (top-level `await` and `return` both work), and the
//! surface is `agent()` / `Member.ask()/.kill()` / `parallel()` /
//! `pipeline()` / `phase()` / `log()`. The whole dialect lives in the
//! embedded prelude below; the only host primitive is `__flow_op(op, json)`,
//! which crosses into `crate::flow::run_op` on a blocking thread — so
//! concurrent `agent()` calls overlap while the JS side stays
//! single-threaded (`parallel()` is `Promise.all`).
//!
//! Determinism contract (same as Claude Code workflows): `Date.now()`,
//! `Math.random()`, and argless `new Date()` throw inside the script, so a
//! resumed run replays the same op sequence. Every run appends each
//! successful op to a journal (`<workspace>/artifacts/flow/<runId>.jsonl`,
//! keyed by op + args); `--resume <runId>` replays cached results for the
//! unchanged prefix. Spawn replay probes the registry first: a member that
//! is still alive is reused (a changed prompt then becomes a live dispatch
//! to the surviving member — something a die-with-the-run subagent model
//! cannot do); a dead member respawns, and every later op on that member
//! bypasses the stale cache.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rquickjs::prelude::{Async, Func};
use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, Promise, Value as JsValue};
use serde_json::{Map, Value};

use crate::flow::{run_op, FlowEnv};

const SCHEMA_JS_HELP: &str =
    "flow scripts are JavaScript: start with `export const meta = { name: '...', description: '...' }` \
     (the Python flow client is gone; see the hive skill's orchestration reference for the dialect)";

// ---------------------------------------------------------------------------
// meta: extracted without executing anything. Balanced-brace scan aware of
// strings and escapes; meta must be a pure literal (no `${}` interpolation),
// the same contract Claude Code imposes on workflow meta.
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

/// Op-level resume journal. Keys are `op\x1f<raw args json>` — the prelude
/// builds every args object with literal field order, so the raw string is
/// stable across runs. Values queue per key in record order (the same op
/// with identical args can legitimately repeat). Failed ops are never
/// recorded: on resume they simply run again.
pub struct Journal {
    cached: Mutex<HashMap<String, VecDeque<Value>>>,
    fresh: Mutex<HashSet<String>>,
    writer: Mutex<fs::File>,
    /// On resume the rewritten journal streams to a sibling `.tmp` file and
    /// only replaces the original in `finalize()` — a resume that dies
    /// mid-replay (script error, Ctrl-C) leaves the prior journal intact
    /// instead of truncating away the un-replayed suffix.
    live_path: PathBuf,
    tmp_path: Option<PathBuf>,
    write_failed: std::sync::atomic::AtomicBool,
}

fn journal_key(op: &str, args_raw: &str) -> String {
    format!("{op}\x1f{args_raw}")
}

impl Journal {
    /// Open the journal at `path`. With `resume`, prior records load as the
    /// replay cache and the rewrite streams to a temp file (cache hits are
    /// re-recorded as they replay); `finalize()` publishes it atomically.
    pub fn open(path: &Path, resume: bool) -> anyhow::Result<Journal> {
        let mut cached: HashMap<String, VecDeque<Value>> = HashMap::new();
        if resume {
            let text = fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("cannot read journal {}: {e}", path.display()))?;
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                let row: Value = serde_json::from_str(line)
                    .map_err(|e| anyhow::anyhow!("corrupt journal line: {e}"))?;
                if let (Some(k), Some(r)) = (row.get("k").and_then(Value::as_str), row.get("r")) {
                    cached.entry(k.to_string()).or_default().push_back(r.clone());
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
    /// once the run has finished (successfully or not) — either way the
    /// temp file holds every op that completed, a strict superset of what
    /// dropping it would keep.
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
            // Loud once: a journal that silently stops recording turns the
            // next --resume into duplicate dispatches.
            if !self
                .write_failed
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                eprintln!("[flow] warning: journal write failed ({e}); this run may not resume cleanly");
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

fn flow_log(message: &str) {
    println!("[flow] {message}");
    let _ = std::io::stdout().flush();
}

/// One `__flow_op` call from the script: journal replay when possible,
/// otherwise `run_op` on a blocking thread (spawn/dispatch/wait-reply all
/// block on real transports). `context` and `kill` always run live — kill is
/// cheap and idempotent, and replaying it would hide a member someone
/// revived between runs.
async fn journaled_op(
    env: Arc<dyn FlowEnv>,
    journal: Arc<Journal>,
    op: String,
    args_raw: String,
) -> String {
    let args: Value = match serde_json::from_str(&args_raw) {
        Ok(v) => v,
        Err(e) => return err_json(&format!("bad op args: {e}")),
    };
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let key = journal_key(&op, &args_raw);
    let journaled = op != "context" && op != "kill";

    if journaled && !journal.is_fresh(&name) {
        if let Some(rec) = journal.take(&key) {
            if op == "spawn" {
                let alive = {
                    let env = env.clone();
                    let n = name.clone();
                    tokio::task::spawn_blocking(move || env.member_exists(&n))
                        .await
                        .unwrap_or(false)
                };
                if alive {
                    flow_log(&format!("{name} alive from previous run; reusing"));
                    journal.record(&key, &rec);
                    return ok_json(&rec);
                }
                flow_log(&format!("{name} from journal is gone; respawning"));
                // fall through to a live spawn; the fresh mark below then
                // keeps every later cached op for this member from replaying
                // against a member that never saw it.
            } else {
                journal.record(&key, &rec);
                return ok_json(&rec);
            }
        }
    }

    if op == "spawn" && !name.is_empty() {
        journal.mark_fresh(&name);
    }

    let outcome = {
        let env = env.clone();
        let op = op.clone();
        tokio::task::spawn_blocking(move || run_op(&*env, &op, &args)).await
    };
    match outcome {
        Ok(Ok(map)) => {
            let value = Value::Object(map);
            if journaled {
                journal.record(&key, &value);
            }
            ok_json(&value)
        }
        Ok(Err(e)) => err_json(&e.0),
        Err(join) => err_json(&format!("flow op '{op}' panicked: {join}")),
    }
}

// ---------------------------------------------------------------------------
// the dialect prelude — the entire script-facing API. The only host
// primitives are `__flow_op(op, json)` and `__host_log(msg)`; everything a
// script touches is defined here, ships inside the binary, and is covered
// by the engine tests below (no two-sided client to keep in sync).
// ---------------------------------------------------------------------------

const PRELUDE: &str = r###"
'use strict';
// -- determinism poisons: a resumed run must replay the same op sequence --
{
  const RealDate = Date;
  globalThis.Date = new Proxy(RealDate, {
    apply() {
      throw new Error('Date() is banned in flow scripts (breaks resume)');
    },
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
  // Guardrail, not a sandbox: prototype-level escapes
  // (new Date(0).constructor.now()) still reach the clock.
  Math.random = () => { throw new Error('Math.random() is banned in flow scripts (breaks resume)'); };
}

globalThis.log = (m) => __host_log(String(m));
globalThis.phase = (t) => __host_log('phase: ' + String(t));

// Args objects below use literal field order on purpose: the raw JSON is
// the journal key.
const __op = async (op, args) => {
  const r = JSON.parse(await __flow_op(op, JSON.stringify(args ?? {})));
  if (!r.ok) throw new Error(r.error || (op + ' failed'));
  return r;
};

// -- structured replies: subset JSON Schema validator (type / properties /
// required / items / enum — what flow schemas actually use) --------------
const __validate = (schema, value, path = '$') => {
  const errs = [];
  if (!schema || typeof schema !== 'object') return errs;
  const typeOf = (v) => Array.isArray(v) ? 'array' : v === null ? 'null' : typeof v;
  const t = schema.type;
  if (t) {
    const ok = t === 'integer'
      ? (typeof value === 'number' && Number.isInteger(value))
      : typeOf(value) === t;
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

const __absorb = (m, reply) => {
  m.summary = reply.body ?? '';
  m.artifact = reply.artifact ?? '';
  m.msgId = reply.msgId ?? '';
  m.data = undefined; // stale structured data must not outlive its reply
};

const __settle = async (m, reply, schema) => {
  __absorb(m, reply);
  if (!schema) return m;
  for (let attempt = 0; ; attempt++) {
    let errs;
    try {
      const data = __parseReplyJson(m.summary);
      errs = __validate(schema, data);
      if (errs.length === 0) { m.data = data; return m; }
    } catch (e) {
      errs = ['body 不是可解析的 JSON: ' + e.message];
    }
    if (attempt >= SCHEMA_REASKS) {
      throw new Error(`member '${m.name}' 的回信连续 ${SCHEMA_REASKS + 1} 次不符合 schema: ${errs.join('; ')}`);
    }
    log(`${m.name} 回信不符合 schema(${errs.join('; ')}); 重新要求 (${attempt + 1}/${SCHEMA_REASKS})`);
    const d = await __op('dispatch-ask', { name: m.name, prompt: `回信不符合要求: ${errs.join('; ')}。重发一条消息,body 为纯 JSON(不带 code fence),符合以下 JSON Schema。${__schemaClause(schema)}` });
    const r = await __op('wait-reply', { name: m.name, msgId: d.msgId });
    __absorb(m, r);
  }
};

// Spawn a member, dispatch `prompt` as its task, block for its reply. The
// prompt is the whole contract — write it self-contained (scope,
// deliverable path, acceptance, material paths). With opts.schema the
// reply body must be pure JSON matching it; the validated object lands on
// `member.data` (invalid replies are re-asked, twice, then throw).
globalThis.agent = async (prompt, opts = {}) => {
  const name = opts.name;
  if (!name) throw new Error('agent() requires opts.name — every flow member gets a stable name');
  const fullPrompt = opts.schema ? prompt + __schemaClause(opts.schema) : prompt;
  const spawned = await __op('spawn', { name, cli: opts.cli ?? null, model: opts.model ?? '' });
  log(`${name} spawned in ${spawned.pane}`);
  await __op('ready', { name, cli: spawned.cli });
  const d = await __op('dispatch-task', { name, prompt: fullPrompt });
  log(`${name} dispatched (${d.msgId}); waiting for reply…`);
  const m = {
    name,
    pane: spawned.pane,
    summary: '',
    artifact: '',
    msgId: '',
    data: undefined,
    _dead: false,
    // Follow-up (question, rework order); the member keeps its full
    // context — this is what a dead headless subagent cannot do.
    async ask(p, askOpts = {}) {
      if (m._dead) throw new Error(`member '${name}' was killed; spawn a new one`);
      const fp = askOpts.schema ? p + __schemaClause(askOpts.schema) : p;
      const dd = await __op('dispatch-ask', { name, prompt: fp });
      log(`${name} asked (${dd.msgId}); waiting…`);
      const r = await __op('wait-reply', { name, msgId: dd.msgId });
      await __settle(m, r, askOpts.schema);
      log(`${name} answered (${m.msgId})`);
      return m;
    },
    // Retire the member's pane; the window re-tiles.
    async kill() {
      await __op('kill', { name });
      m._dead = true;
      log(`${name} retired`);
    },
  };
  const reply = await __op('wait-reply', { name, msgId: d.msgId });
  await __settle(m, reply, opts.schema);
  log(`${name} replied (${m.msgId})`);
  return m;
};

// Run thunks concurrently; a failed branch resolves to null, never rejects.
globalThis.parallel = (thunks) => Promise.all((thunks ?? []).map((t) =>
  Promise.resolve().then(t).catch((e) => {
    __host_log('parallel branch failed: ' + (e && e.message || e));
    return null;
  })
));

// Per-item pipeline, no barrier between stages; a throwing stage drops the
// item to null and skips its remaining stages. Stage callbacks receive
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
    let meta_src = extract_meta(src).ok_or_else(|| anyhow::anyhow!("{SCHEMA_JS_HELP}"))?;
    // meta must be a pure literal: interpolation would break the static
    // parse (the brace scanner cannot see `${}` nesting) and meta is
    // evaluated standalone before the body runs.
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
            globals.set(
                "__host_log",
                Func::from(|msg: String| flow_log(&msg)),
            )?;
            globals.set(
                "__flow_op",
                Func::from(Async(move |op: String, args: String| {
                    let env = env.clone();
                    let journal = journal.clone();
                    async move { journaled_op(env, journal, op, args).await }
                })),
            )?;

            ctx.eval::<(), _>(PRELUDE).catch(&ctx).map_err(stringify_err)?;

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

/// `hive flow run` body: resolve the team, open the journal, evaluate the
/// script, print the result JSON. Returns the process exit code.
pub fn run_cmd(script_path: &str, resume: Option<&str>) -> i32 {
    let src = match fs::read_to_string(script_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: cannot read {script_path}: {e}");
            return 1;
        }
    };
    // Fail on a non-dialect script before touching tmux or the registry —
    // the likeliest authoring mistake is a Python-era script.
    if extract_meta(&src).is_none() {
        eprintln!("Error: {SCHEMA_JS_HELP}");
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
    flow_log(&format!(
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
    // Publish the rewritten journal even on error — it holds every op that
    // completed, which is exactly what the next --resume needs.
    journal.finalize();
    match outcome {
        Ok(outcome) => {
            flow_log("result:");
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

    async fn run(env: Arc<FakeEnv>, journal: Arc<Journal>, src: &str) -> anyhow::Result<RunOutcome> {
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
        assert!(err.to_string().contains("Python"), "{err}");
    }

    #[tokio::test]
    async fn test_dialect_end_to_end_with_parallel_ask_and_kill() {
        let tmp = TempDir::new().unwrap();
        let mut fake = fake_env(tmp.path());
        fake.reply_any = true;
        let env = Arc::new(fake);
        let src = r#"export const meta = {
  name: 'demo-review',
  description: 'scout, fan-out, follow-up ask',
  phases: [{ title: 'Explore' }, { title: 'Review' }],
}
phase('Explore')
const scout = await agent('探索认证模块,列出改动面', { name: 'scout' })
phase('Review')
const reviewers = await parallel([
  () => agent(`复查安全面:\n${scout.summary}`, { name: 'sec' }),
  () => agent(`复查性能面:\n${scout.summary}`, { name: 'perf' }),
  () => agent(`复查测试面:\n${scout.summary}`, { name: 'tests', cli: 'codex' }),
])
const ok = reviewers.filter(Boolean)
await ok[0].ask('第一条发现给出修复建议')
await scout.kill()
log(`review done: ${ok.length}/3`)
return { scout: scout.summary, reviews: ok.map((r) => ({ name: r.name })) }"#;
        let out = run(env.clone(), scratch_journal(tmp.path()), src)
            .await
            .unwrap();
        assert_eq!(out.meta["name"], "demo-review");
        assert_eq!(out.result["reviews"].as_array().unwrap().len(), 3);
        assert!(out.result["scout"].as_str().unwrap().starts_with("done-"));
        assert_eq!(env.spawns.lock().unwrap().len(), 4);
        // 4 task dispatches + 1 ask
        assert_eq!(env.dispatches.lock().unwrap().len(), 5);
        assert_eq!(
            *env.killed.lock().unwrap(),
            vec!["scout".to_string(), "layout dev:0".to_string()]
        );
    }

    #[tokio::test]
    async fn test_dialect_torture_poisons_and_null_contracts() {
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
checks.thenChain = await agent('chain', { name: 'chain' }).then((m) => m.ask('more')).then((m) => m.summary.length > 0 ? 'ok' : 'FAIL')
const victim = await agent('to be killed', { name: 'victim' })
await victim.kill()
try { await victim.ask('speak'); checks.deadGuard = 'FAIL' } catch (e) { checks.deadGuard = 'ok' }
let rounds = 0, dry = 0
while (dry < 2) { rounds++; if (rounds > 1) dry++; if (rounds > 10) break }
checks.whileLoop = rounds === 3 ? 'ok' : 'FAIL'
return checks"#;
        let out = run(env, scratch_journal(tmp.path()), src).await.unwrap();
        for (check, verdict) in out.result.as_object().unwrap() {
            assert_eq!(verdict, "ok", "torture check '{check}': {:?}", out.result);
        }
    }

    #[tokio::test]
    async fn test_schema_validates_and_reasks_until_valid() {
        let tmp = TempDir::new().unwrap();
        let fake = fake_env(tmp.path());
        {
            let mut replies = fake.replies.lock().unwrap();
            replies.insert("m1".into(), reply_row("not json at all", "", "r1"));
            replies.insert(
                "m2".into(),
                reply_row("```json\n{\"verdict\": \"pass\", \"score\": 3}\n```", "", "r2"),
            );
            replies.insert("m3".into(), reply_row("plain follow-up", "", "r3"));
        }
        let env = Arc::new(fake);
        let src = r#"export const meta = { name: 's', description: 'schema' }
const m = await agent('judge it', { name: 'judge', schema: {
  type: 'object',
  required: ['verdict', 'score'],
  properties: { verdict: { type: 'string', enum: ['pass', 'fail'] }, score: { type: 'integer' } },
} })
const first = m.data
await m.ask('thanks, one more note')
return { verdict: first.verdict, score: first.score, dataCleared: m.data === undefined }"#;
        let out = run(env.clone(), scratch_journal(tmp.path()), src)
            .await
            .unwrap();
        assert_eq!(out.result["verdict"], "pass");
        assert_eq!(out.result["score"], 3);
        // a schema-less reply must not leave stale structured data behind
        assert_eq!(out.result["dataCleared"], true);
        // task dispatch + one schema re-ask + one plain ask
        let dispatches = env.dispatches.lock().unwrap();
        assert_eq!(dispatches.len(), 3);
        // the task artifact and the re-ask both carry the schema clause
        let task_artifact = fs::read_to_string(&dispatches[0].artifact).unwrap();
        assert!(task_artifact.contains("JSON Schema"), "{task_artifact}");
        assert!(dispatches[1].body.contains("JSON Schema") || {
            let re_ask = fs::read_to_string(&dispatches[1].artifact).unwrap();
            re_ask.contains("JSON Schema")
        });
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
        // initial task + 2 re-asks, then loud failure
        assert_eq!(env.dispatches.lock().unwrap().len(), 3);
    }

    const RESUME_SRC: &str = r#"export const meta = { name: 'resume', description: 'journal probe' }
const m = await agent('solo task', { name: 'solo' })
return m.summary"#;

    #[tokio::test]
    async fn test_journal_replays_unchanged_run_without_new_ops() {
        let tmp = TempDir::new().unwrap();
        let journal_file = tmp.path().join("j.jsonl");

        let mut fake = fake_env(tmp.path());
        fake.reply_any = true;
        let env1 = Arc::new(fake);
        let j1 = Arc::new(Journal::open(&journal_file, false).unwrap());
        let out1 = run(env1.clone(), j1, RESUME_SRC).await.unwrap();

        let fake2 = fake_env(tmp.path());
        // the member survived; nothing may respawn or redispatch
        fake2.agents.lock().unwrap().push("solo".to_string());
        let env2 = Arc::new(fake2);
        let j2 = Arc::new(Journal::open(&journal_file, true).unwrap());
        let out2 = run(env2.clone(), j2, RESUME_SRC).await.unwrap();

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
        let env1 = Arc::new(fake);
        let j1 = Arc::new(Journal::open(&journal_file, false).unwrap());
        run(env1, j1, RESUME_SRC).await.unwrap();

        let mut fake2 = fake_env(tmp.path());
        fake2.reply_any = true;
        fake2.agents.lock().unwrap().push("solo".to_string());
        let env2 = Arc::new(fake2);
        let j2 = Arc::new(Journal::open(&journal_file, true).unwrap());
        let changed = RESUME_SRC.replace("solo task", "revised task");
        run(env2.clone(), j2, &changed).await.unwrap();

        // spawn replayed (member alive), the new task went out live
        assert_eq!(env2.spawn_calls.load(Ordering::SeqCst), 0);
        assert_eq!(env2.send_calls.load(Ordering::SeqCst), 1);
        let dispatches = env2.dispatches.lock().unwrap();
        assert_eq!(
            fs::read_to_string(&dispatches[0].artifact).unwrap(),
            "revised task"
        );
    }

    #[tokio::test]
    async fn test_journal_respawns_dead_member_and_bypasses_stale_cache() {
        let tmp = TempDir::new().unwrap();
        let journal_file = tmp.path().join("j.jsonl");

        let mut fake = fake_env(tmp.path());
        fake.reply_any = true;
        let env1 = Arc::new(fake);
        let j1 = Arc::new(Journal::open(&journal_file, false).unwrap());
        run(env1, j1, RESUME_SRC).await.unwrap();

        // member gone: same script must respawn and redispatch live
        let mut fake2 = fake_env(tmp.path());
        fake2.reply_any = true;
        let env2 = Arc::new(fake2);
        let j2 = Arc::new(Journal::open(&journal_file, true).unwrap());
        run(env2.clone(), j2, RESUME_SRC).await.unwrap();

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
            "{\"k\":\"spawn\\u001f{}\",\"r\":{\"pane\":\"%1\"}}\n{\"k\":\"wait\\u001f{}\",\"r\":{\"body\":\"done\"}}\n",
        )
        .unwrap();
        let original = fs::read_to_string(&journal_file).unwrap();

        // A resume that records a prefix and then dies without finalize()
        // (script error, Ctrl-C) must not have touched the original.
        let j = Journal::open(&journal_file, true).unwrap();
        j.record("spawn\u{1f}{}", &serde_json::json!({"pane": "%1"}));
        drop(j);
        assert_eq!(fs::read_to_string(&journal_file).unwrap(), original);

        // finalize() publishes the rewrite atomically.
        let j = Journal::open(&journal_file, true).unwrap();
        j.record("spawn\u{1f}{}", &serde_json::json!({"pane": "%2"}));
        j.finalize();
        let published = fs::read_to_string(&journal_file).unwrap();
        assert!(published.contains("%2"));
        assert!(!published.contains("done"));
    }

    #[tokio::test]
    async fn test_meta_interpolation_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let env = Arc::new(fake_env(tmp.path()));
        let src = "export const meta = { name: `run-${1}`, description: 'x' }\nreturn 1";
        let err = run(env, scratch_journal(tmp.path()), src).await.unwrap_err();
        assert!(err.to_string().contains("pure literal"), "{err}");
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
