use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::adapters::claude_sessions::{config_dir, truthy_str};

use super::engine::{hooked_engine_for_job, EngineSession};
use super::keyboard::strip_ansi;
use super::{
    looks_like_job_id, sleep_s, AGENTS_TIMEOUT, ENTRY_POLL_INTERVAL, SPAWN_TIMEOUT,
    WAKE_ENTRY_TIMEOUT, WAKE_TIMEOUT,
};

#[cfg(test)]
use super::testhook;

// --------------------------------------------------------------------------
// job ledger (claude agents --json --all) and lifecycle
// --------------------------------------------------------------------------

/// Environment for claude bg invocations.
///
/// CLAUDE*/ANTHROPIC* vars are washed: an inherited
/// `CLAUDE_CODE_CHILD_SESSION` makes the engine skip registration — invisible
/// and undeliverable. The config-tree override survives as
/// `CLAUDE_CONFIG_DIR` so a sandboxed lane's engine registers in the same
/// tree hive reads. The other engines' session markers go the same way: the
/// spawner may be a codex or grok member, and its session id keys *its*
/// roster row — inherited into this job, every hive call the job makes would
/// sign as the spawner.
pub fn bg_env(extra: Option<&HashMap<String, String>>) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars()
        .filter(|(k, _)| {
            !(k.starts_with("CLAUDE")
                || k.starts_with("ANTHROPIC")
                || k == "CODEX_THREAD_ID"
                || k == "GROK_SESSION_ID")
        })
        .collect();
    let config = config_dir();
    let home_default = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude");
    if config != home_default {
        env.insert(
            "CLAUDE_CONFIG_DIR".to_string(),
            config.to_string_lossy().into_owned(),
        );
    }
    if let Some(extra) = extra {
        for (k, v) in extra {
            env.insert(k.clone(), v.clone());
        }
    }
    env
}

/// `subprocess.run(argv, capture_output=True, timeout=...)`: (returncode,
/// stdout, stderr), or None when the call itself failed or timed out.
pub(super) fn run_capture(
    argv: &[String],
    timeout: f64,
    cwd: Option<&str>,
    env: &HashMap<String, String>,
) -> Option<(i32, String, String)> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().ok()?;
    let mut out_pipe = child.stdout.take()?;
    let mut err_pipe = child.stderr.take()?;
    let out_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = out_thread.join().unwrap_or_default();
                let stderr = err_thread.join().unwrap_or_default();
                return Some((
                    status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&stdout).into_owned(),
                    String::from_utf8_lossy(&stderr).into_owned(),
                ));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

/// All job rows from `claude agents --json --all`; None when the CLI call
/// itself failed (distinct from an empty ledger).
pub fn list_jobs(claude_bin: &str) -> Option<Vec<Map<String, Value>>> {
    let argv = vec![
        claude_bin.to_string(),
        "agents".to_string(),
        "--json".to_string(),
        "--all".to_string(),
    ];
    let (code, stdout, _stderr) = run_capture(&argv, AGENTS_TIMEOUT, None, &bg_env(None))?;
    if code != 0 {
        return None;
    }
    let rows: Value = serde_json::from_str(&strip_ansi(&stdout)).ok()?;
    let rows = rows.as_array()?;
    Some(
        rows.iter()
            .filter_map(|row| row.as_object().cloned())
            .collect(),
    )
}

fn hooked_list_jobs(claude_bin: &str) -> Option<Vec<Map<String, Value>>> {
    #[cfg(test)]
    {
        if let Some(Some(v)) = testhook::with(|h| h.list_jobs_rows.clone()) {
            return v;
        }
    }
    list_jobs(claude_bin)
}

/// The ledger row for *job_id*, or None (unknown job, or CLI failure).
///
/// A row without `pid`/`status` is a parked or stopped engine — asleep, not
/// dead: `claude attach` wakes it with the same jobId/sessionId.
pub fn job_row(job_id: &str, claude_bin: &str) -> Option<Map<String, Value>> {
    if job_id.is_empty() {
        return None;
    }
    let rows = hooked_list_jobs(claude_bin)?;
    rows.into_iter()
        .find(|row| truthy_str(row.get("id")) == job_id)
}

pub fn job_exists(job_id: &str, claude_bin: &str) -> bool {
    job_row(job_id, claude_bin).is_some()
}

/// `backgrounded\s*·\s*(\S+)` over the ANSI-stripped spawn stdout.
fn spawn_announced(plain: &str) -> String {
    let chars: Vec<char> = plain.chars().collect();
    let key: Vec<char> = "backgrounded".chars().collect();
    let n = chars.len();
    let mut idx = 0;
    while idx + key.len() <= n {
        if chars[idx..idx + key.len()] == key[..] {
            let mut i = idx + key.len();
            while i < n && chars[i].is_whitespace() {
                i += 1;
            }
            if i < n && chars[i] == '\u{b7}' {
                i += 1;
                while i < n && chars[i].is_whitespace() {
                    i += 1;
                }
                let start = i;
                while i < n && !chars[i].is_whitespace() {
                    i += 1;
                }
                if i > start {
                    return chars[start..i].iter().collect();
                }
            }
        }
        idx += 1;
    }
    String::new()
}

/// Start a `claude --bg` job; return its jobId, or None on failure.
///
/// *extra_args* are forwarded verbatim (`--model`, `-r <sid> --fork-session`,
/// `--settings` …) and become the job's durable `respawnFlags`, so any
/// path-valued flag must be absolute. The prompt is the positional argument
/// (never `-p`, which `--bg` rejects). An empty *name* adds no `--name` (the
/// caller passed its own in *extra_args*).
pub fn spawn_job(
    cwd: &str,
    name: &str,
    prompt: &str,
    extra_args: &[String],
    extra_env: Option<&HashMap<String, String>>,
    claude_bin: &str,
) -> Option<String> {
    let mut argv = vec![claude_bin.to_string(), "--bg".to_string()];
    if !name.is_empty() {
        argv.push("--name".to_string());
        argv.push(name.to_string());
    }
    argv.extend(extra_args.iter().cloned());
    if !prompt.is_empty() {
        argv.push(prompt.to_string());
    }
    let cwd = if cwd.is_empty() { None } else { Some(cwd) };
    let (code, stdout, _stderr) = run_capture(&argv, SPAWN_TIMEOUT, cwd, &bg_env(extra_env))?;
    if code != 0 {
        return None;
    }
    let announced = spawn_announced(&strip_ansi(&stdout));
    // The announcement is stdout, not a contract: an escape hive does not
    // strip (the FORCE_COLOR class) or a reworded line yields a token no
    // registry row can ever carry as its `jobId`, and the caller would poll
    // for it until the whole startup budget burned. Refuse it here instead.
    if looks_like_job_id(&announced) {
        Some(announced)
    } else {
        None
    }
}

/// Revive a parked/stopped engine without a terminal.
///
/// `claude attach <jobId>` with stdin at /dev/null prints "Waking…", spins
/// the engine back up (new pid, same jobId/sessionId) and exits 0. On a
/// removed job it fails; the caller reads the registry to see the result.
pub fn wake_job(job_id: &str, claude_bin: &str) -> bool {
    if job_id.is_empty() {
        return false;
    }
    let argv = vec![
        claude_bin.to_string(),
        "attach".to_string(),
        job_id.to_string(),
    ];
    match run_capture(&argv, WAKE_TIMEOUT, None, &bg_env(None)) {
        Some((code, _out, _err)) => code == 0,
        None => false,
    }
}

fn hooked_wake_job(job_id: &str, claude_bin: &str) -> bool {
    #[cfg(test)]
    {
        if let Some(Some(v)) = testhook::with(|h| {
            if h.wake_result.is_some() {
                h.wakes.push(job_id.to_string());
            }
            h.wake_result
        }) {
            return v;
        }
    }
    wake_job(job_id, claude_bin)
}

/// Poll for the engine's registry entry (spawn readiness).
pub fn wait_engine_entry(job_id: &str, timeout: f64) -> Option<EngineSession> {
    wait_engine_entry_until(job_id, timeout, || false)
}

/// [`wait_engine_entry`] that also stops early once *give_up* says the entry
/// can no longer come (the attach client behind it exited, say).
pub(super) fn wait_engine_entry_until(
    job_id: &str,
    timeout: f64,
    mut give_up: impl FnMut() -> bool,
) -> Option<EngineSession> {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
    loop {
        if let Some(engine) = hooked_engine_for_job(job_id) {
            return Some(engine);
        }
        if give_up() || Instant::now() >= deadline {
            return None;
        }
        sleep_s(ENTRY_POLL_INTERVAL);
    }
}

/// The job's live engine entry, waking a parked engine when needed.
///
/// Returns None when no engine came up — the job is gone (removed) or the
/// wake failed; the caller decides whether that is a delivery error.
/// *timeout* None means the wake-entry default.
pub fn ensure_engine(
    job_id: &str,
    timeout: Option<f64>,
    claude_bin: &str,
) -> Option<EngineSession> {
    if let Some(engine) = hooked_engine_for_job(job_id) {
        return Some(engine);
    }
    if !hooked_wake_job(job_id, claude_bin) {
        return None;
    }
    wait_engine_entry(job_id, timeout.unwrap_or(WAKE_ENTRY_TIMEOUT))
}
