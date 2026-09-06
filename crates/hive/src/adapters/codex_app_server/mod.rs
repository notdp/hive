//! Codex app-server client over a single shared daemon.
//!
//! One `codex app-server --listen unix://<sock>` daemon per CODEX_HOME hosts
//! every hive codex thread. Each codex TUI attaches with `codex resume
//! <threadId> --remote unix://<sock> --cd <cwd>` and drives its own thread;
//! hive connects as one more client over the same socket for runtime signals
//! and turn delivery.
//!
//! Identity is the threadId (== transcript sessionId), never the process
//! environment: the daemon's env is frozen at spawn time and shared by every
//! thread, so `TMUX_PANE` is stripped from it and codex's own per-thread
//! `CODEX_THREAD_ID` injection into tool subprocesses is the tool-side
//! identity. Which thread belongs to which tmux pane is recorded in a
//! per-pane `.thread` file beside the socket.
//!
//! Transport is WebSocket framing over the unix socket — RFC6455 masked text
//! frames, one background reader thread per connection.

const HANDSHAKE_TIMEOUT: f64 = 5.0;
const CALL_TIMEOUT: f64 = 10.0;

/// Worst-case local submission budget for one send_to_pane call (fresh daemon
/// handshake plus the turn/start RPC). The hived derives its request budgets
/// from this so a valid slow acceptance can never outlive the caller's timeout.
pub const SUBMIT_TIMEOUT: f64 = HANDSHAKE_TIMEOUT + CALL_TIMEOUT;
const DAEMON_START_TIMEOUT: f64 = 8.0;
const CONNECT_COOLDOWN: f64 = 5.0;
const RESUME_COOLDOWN: f64 = 5.0;

/// Accepted-transport classification for durable delivery observations: the
/// shared daemon took the turn. Not proof the turn produced output.
pub const TURN_START_ACCEPTED: &str = "turnStartAccepted";

/// Interrupt outcomes: the daemon aborted the running turn, or there was no
/// turn to abort (an idle thread is nothing to interrupt, not a failure).
pub const TURN_INTERRUPT_ACCEPTED: &str = "turnInterruptAccepted";
pub const NO_RUNNING_TURN: &str = "noRunningTurn";

mod client;
mod daemon;
mod records;
#[cfg(test)]
pub(crate) mod tests;
mod transport;

pub use client::*;
pub use daemon::*;
pub use records::*;
pub use transport::*;
