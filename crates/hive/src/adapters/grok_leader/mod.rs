//! Grok leader client over a key-scoped leader daemon.
//!
//! A leader daemon (`grok agent leader --leader-socket <sock>`, sharing the
//! real GROK_HOME) is keyed by who it serves (`keys.rs`): a team member's
//! engine lives on `m-<team>.<member>` and outlives any pane, a raw `hive grok`
//! pane on `p<slug>`. A pane reaches its daemon through `resolve_pane_key`; a
//! headless member needs no pane at all (`spawn_member_daemon`). The grok TUI
//! in a pane attaches with `grok --leader --leader-socket <sock> --session-id
//! <uuid>`; hive attaches as a second client through `grok agent --leader
//! stdio --leader-socket <sock>` — a subprocess speaking ACP JSON-RPC 2.0 as
//! newline-delimited JSON on stdin/stdout. The leader's own socket protocol is
//! private, so hive never talks to the socket directly: the stdio subprocess
//! is the supported door.
//!
//! Which session that second client drives is not discoverable from the leader
//! (`session/list` returns every session of the cwd), so hive mints the
//! session id at spawn time and records it beside the socket in the key's
//! `.session` file. The client loads exactly that session and folds only its
//! notifications.
//!
//! `session/load` replays the session's past `session/update` notifications
//! before it answers, so everything received before the load response is dropped —
//! a replayed turn must never mark the pane busy. Delivery acks on the leader
//! echoing the prompt back (queue entry or `user_message_chunk`): the
//! `session/prompt` response itself only lands when the whole turn ends, which
//! can be minutes.

use std::env;
use std::path::PathBuf;

mod client;
mod daemon;
mod keys;
mod pool;

pub use client::*;
pub use daemon::*;
pub use keys::*;
pub use pool::*;

const _INIT_TIMEOUT: f64 = 10.0; // initialize answers ~2 s after process start
const _LOAD_TIMEOUT: f64 = 5.0; // session/load ~0.8 s plus the notification replay
const _HANDSHAKE_TIMEOUT: f64 = _INIT_TIMEOUT + _LOAD_TIMEOUT;
const _ACK_TIMEOUT: f64 = 10.0;
const _CALL_TIMEOUT: f64 = 10.0;
const _DAEMON_START_TIMEOUT: f64 = 8.0;
const _CONNECT_COOLDOWN: f64 = 5.0;

/// Worst-case local submission budget for one send_to_pane call: a cold client
/// (initialize + session/load) plus the ack wait. The hived derives its request
/// budgets from this so a valid slow acceptance can never outlive its caller.
pub const SUBMIT_TIMEOUT: f64 = _HANDSHAKE_TIMEOUT + _ACK_TIMEOUT;

/// Accepted-transport classification for durable delivery observations: the
/// leader took the prompt into the session queue. Not proof the turn ran.
pub const PROMPT_QUEUED: &str = "sessionPromptQueued";

/// The ACP cancel left for the leader. It is a notification, so this is the
/// only accept class there is — see [`GrokStdioClient::cancel`].
pub const CANCEL_SENT: &str = "sessionCancelSent";

const _TOOL_PHASES: [&str; 2] = ["tool_open", "tool_result_pending_reply"];
const _MESSAGE_CHUNKS: [&str; 3] = [
    "agent_message_chunk",
    "agent_thought_chunk",
    "user_message_chunk",
];

pub fn grok_home() -> PathBuf {
    match env::var("GROK_HOME") {
        Ok(value) => PathBuf::from(value),
        Err(_) => PathBuf::from(env::var("HOME").unwrap_or_default()).join(".grok"),
    }
}

// --------------------------------------------------------------------------
// tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests;
