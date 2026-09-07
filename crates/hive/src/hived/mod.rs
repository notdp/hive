//! Team-scoped hived: message transport, runtime signals, notify watcher.
//!
//! Delivery has exactly one state: the native transport (claude inbox /
//! codex daemon / grok leader) either accepted the message or refused it.
//! There is no tracked in-between and no confirmation oracle — acceptance
//! means the target's own runtime owns it from there.

use std::sync::OnceLock;
use std::time::Instant;

mod busy;
mod client;
mod idle_notify;
mod lifecycle;
mod paths;
mod payloads;
mod reexec;
mod runtime;
mod seams;
mod server;
mod state;
mod status;
mod succession;
mod supervisors;

#[cfg(test)]
pub(crate) mod testhook;
#[cfg(test)]
mod tests;

pub(crate) use busy::*;
pub use client::*;
pub use idle_notify::*;
pub use lifecycle::*;
pub use paths::*;
pub(crate) use payloads::*;
pub use reexec::*;
pub(crate) use runtime::*;
pub use seams::*;
pub use server::*;
pub use state::*;
pub use status::*;
pub use supervisors::*;

pub const IDLE_NOTIFY_TICK_SECONDS: f64 = 1.0;
pub const IDLE_NOTIFY_THRESHOLD_SECONDS: f64 = 5.0;
pub const IDLE_NOTIFY_MESSAGE: &str = "Window idle 5s+ (all agents stopped). Return to review.";
pub const IDLE_NOTIFY_MISSING_PRUNE_TICKS: i64 = 5;
pub const NOTIFY_DEBUG_HEARTBEAT_SECONDS: f64 = 30.0;
pub const HIVED_CODE_CHECK_SECONDS: f64 = 5.0;
pub const HIVED_OWNER_CHECK_SECONDS: f64 = 5.0;
const HIVED_REEXEC_LOCK_ENV: &str = "HIVE_HIVED_REEXEC_LOCK_FD";
pub const SOCKET_READY_TIMEOUT: f64 = 2.0;
pub const SOCKET_RETRY_INTERVAL: f64 = 0.1;
// The CLI's socket budget must be strictly longer than the work it asks the
// hived to perform: worst-case native transport submission (claude inbox
// connect+write / codex daemon RPC / grok leader prompt+ack) plus slack for
// scheduling and payload plumbing. A send blocks on nothing else: it
// returns the moment the transport accepts, and nothing tracks it after.
pub const REQUEST_SLACK: f64 = 5.0;
pub const HIVED_API_VERSION: i64 = 5;
pub const BUSY_OUTPUT_THRESHOLD_SECONDS: f64 = 3.0;
// A probed session id only speaks for the session it saw: nothing tells the
// hived that the human typed `/new` in an unmanaged pane, so the snapshot
// ages out and the adapter re-probes instead of pinning a dead id forever.
const SESSION_SNAPSHOT_FRESHNESS_S: f64 = 600.0;
const TRANSCRIPT_PATH_CACHE_TTL: f64 = 60.0;
const CLAUDE_JOBS_CACHE_TTL: f64 = 30.0;
const GROK_REAP_GRACE_SECONDS: f64 = 120.0;
// One send_keys attempt per pane per cooldown window, so a slow-starting
// codex is not typed at twice while the process check cannot see it yet.
const CODEX_REATTACH_COOLDOWN_SECONDS: f64 = 60.0;

// waitingFor values that do not gate a send: a /status-style dialog open in
// an attached viewer parks the status on "waiting", but the inbox still
// queues normally and the message shows the moment the dialog closes.
const SEND_GATE_WAIVED_REASONS: [&str; 1] = ["registry:dialog open"];

// Near-zero process clock (runtime_snapshot's timestamps share the shape).
// A stamp that must read as "long ago" seeds NEG_INFINITY, not 0.0: zero is
// only moments before the first tick on this clock.
fn monotonic() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

fn native_submit_timeout() -> f64 {
    // claude's worst case is a delivery that has to wake a parked engine
    // first (ledger check + tty-less attach + entry poll) before the inbox
    // write itself.
    let claude = crate::adapters::claude_sessions::SUBMIT_TIMEOUT
        + crate::adapters::claude_bg::WAKE_SUBMIT_BUDGET;
    claude
        .max(crate::adapters::codex_app_server::SUBMIT_TIMEOUT)
        .max(crate::adapters::grok_leader::SUBMIT_TIMEOUT)
}

fn send_request_timeout() -> f64 {
    native_submit_timeout() + REQUEST_SLACK
}
