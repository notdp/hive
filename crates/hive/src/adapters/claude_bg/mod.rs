//! Claude background jobs: the engine behind a hive claude member.
//!
//! A claude member is a `claude --bg` job. The engine (a full Claude Code TUI
//! on a pty owned by claude's own supervisor daemon, argv `claude bg-spare`)
//! runs outside tmux; the member's pane only shows it through a `claude attach
//! <jobId>` viewer, so the pane process table says nothing about the member's
//! life. Identity is the jobId — durable across engine restarts, wakes and
//! upgrades (the engine pid is not). Which job belongs to which tmux pane is
//! recorded in a per-pane `.job` file under the claude config tree, written by
//! whoever binds the pane to a job (spawn, managed launch, fork) — the same
//! shape as codex's pane `.thread` records.
//!
//! Signal surfaces (2.1.240 real-machine verified):
//!
//! - `<claude-config>/sessions/<enginePid>.json`: the live engine's registry
//!   entry — `kind:"bg"`, `jobId`, `status` (idle|busy|waiting; not a
//!   documented enum), `waitingFor` (only while waiting), `statusUpdatedAt`,
//!   `sessionId`, `messagingSocketPath`. The attach viewer never registers.
//! - `claude agents --json --all`: the durable job ledger. A sleeping engine
//!   (supervisor parks jobs idle ~1h) or a stopped one keeps its row but loses
//!   `pid`/`status` — that field absence is the asleep-vs-dead separator.
//!   ~270ms per call, so it runs only on resolution misses, never per tick.
//! - `claude attach <jobId>` with no tty (stdin /dev/null) prints "Waking…"
//!   and exits 0 after reviving a parked/stopped engine — new pid, same
//!   jobId/sessionId. That is the wake primitive delivery self-heals with.
//!
//! Hidden claude subcommands are only recognized at `argv[1]`, so every
//! invocation here calls the binary directly with the subcommand first. Spawn
//! env is washed of CLAUDE*/ANTHROPIC* vars: an inherited
//! `CLAUDE_CODE_CHILD_SESSION` marker makes the engine skip registration
//! entirely (invisible to `agents --json` and undeliverable).

mod attach;
mod engine;
mod keyboard;
mod lifecycle;

#[cfg(test)]
pub(crate) mod testhook;
#[cfg(test)]
mod tests;

pub use engine::*;
pub use keyboard::*;
pub use lifecycle::*;

use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const AGENTS_TIMEOUT: f64 = 10.0; // observed ~270ms; the cap only bounds a hung CLI
const SPAWN_TIMEOUT: f64 = 60.0;
const WAKE_TIMEOUT: f64 = 20.0; // observed ~2-6s including a fresh supervisor start
const WAKE_ENTRY_TIMEOUT: f64 = 5.0; // the wake is synchronous; the entry follows fast
const ENTRY_POLL_INTERVAL: f64 = 0.3;
/// Worst-case extra submission budget when delivery must wake a parked engine
/// first: one ledger read, the tty-less attach that revives it, and the short
/// entry re-read. The hived folds this into its request budgets.
pub const WAKE_SUBMIT_BUDGET: f64 = AGENTS_TIMEOUT + WAKE_TIMEOUT + WAKE_ENTRY_TIMEOUT;

/// An engine entry whose statusUpdatedAt stopped advancing this long ago is
/// not trusted as busy/waiting truth (wedged engine, clock issues); liveness
/// still holds — the pid check is what proves the process.
pub const STATUS_STALE_AFTER_SECONDS: f64 = 30.0 * 60.0;

/// Job ids observed are 8 lowercase hex chars (the sessionId prefix); accept a
/// small band around that so a format drift upstream does not break resolution.
pub fn looks_like_job_id(value: &str) -> bool {
    let n = value.chars().count();
    (6..=12).contains(&n) && value.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

fn sleep_s(secs: f64) {
    #[cfg(test)]
    {
        if testhook::with(|h| h.no_sleep).unwrap_or(false) {
            return;
        }
    }
    if secs > 0.0 {
        thread::sleep(Duration::from_secs_f64(secs));
    }
}

fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
