//! The turn reader behind `hive node run`: how a node runner learns, from an
//! engine's own transcript, that its dispatched task was received, which
//! engine turn it started, and what that turn's final assistant message
//! was. One implementation per CLI (`claude_turn`, `codex_turn`,
//! `grok_turn`); the node core (`node.rs`) drives them and never parses a
//! transcript itself.
//!
//! The anchor is input identity, not time: the runner injects a `marker`
//! (the dispatch id) into the text the member receives, and the reader
//! binds the turn that consumed that exact input. A turn the reader cannot
//! attribute to the marker is reported, never guessed at.

use std::fmt;

/// Where the runner was in the transcript before it dispatched. Opaque to
/// the core: each reader serialises what it needs (file identity, byte
/// offset, history line count) into the string, and only that reader
/// parses it back.
pub type Cursor = String;

/// The engine turn bound to one dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnAnchor {
    /// Engine session id the input landed in.
    pub session: String,
    /// Engine-native turn key: the claude user record uuid, the codex
    /// `turn_id`, grok's `<session_id>/<turn_number>`.
    pub turn: String,
    /// Reader state to resume from when polling for the outcome (file
    /// identity, offset of the bound input, history cursor). Opaque.
    pub cursor: Cursor,
}

/// What `find_input` saw in the transcript since the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputBinding {
    /// The marker has not appeared yet; poll again.
    NotYet,
    /// The marker landed as a fresh turn's input.
    Bound(TurnAnchor),
    /// The marker landed, but not as a turn of its own (folded into a
    /// running turn, queued attachment, a shape the reader cannot
    /// attribute): the node cannot own a turn. `reason` names the shape.
    Ambiguous(String),
}

/// How the bound turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The engine closed the turn normally. `text` is the final assistant
    /// message of that turn in full: every text block, original order,
    /// thinking and tool blocks excluded. Empty when the turn closed
    /// without a text block (callers see `completed` with an empty body).
    Completed { text: String },
    /// The turn was cut short by a human or the engine (cancel, escape,
    /// interrupt); `reason` keeps the engine's own label.
    Interrupted { reason: String },
    /// The engine reported an error ending the turn.
    Failed { reason: String },
    /// The turn's end cannot be attributed (records after the anchor no
    /// longer chain to it, a compaction rewrote the branch, a second input
    /// merged in).
    Ambiguous { reason: String },
    /// The transcript now belongs to a different session (clear, resume
    /// into another id, fork): the anchor is void.
    SessionChanged { reason: String },
    /// The engine has closed the turn but the final message is not on disk
    /// yet (grok writes `turn_ended` before the history line; claude writes
    /// one block per record). Not a result: the core keeps polling under
    /// its flush budget and ends `ambiguous` when the text never lands.
    /// `reason` names what is still missing.
    Flushing { reason: String },
}

/// Why a reader could not read at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// No transcript for the session (not written yet, path unresolvable,
    /// file gone).
    Unavailable(String),
    /// The transcript exists but its records are not a shape the reader
    /// knows (schema drift).
    UnsupportedSchema(String),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Unavailable(why) => write!(f, "transcript unavailable: {why}"),
            ReadError::UnsupportedSchema(why) => write!(f, "unsupported transcript schema: {why}"),
        }
    }
}

/// One engine's transcript reader. `session_id` and `cwd` come from the
/// member's roster row (the engine's own session id, the cwd the member
/// was spawned in); readers resolve the transcript through their adapter's
/// `find_session_file` and never through the caller's cwd.
///
/// Every call re-opens the transcript: a reader holds no file handles
/// between calls, and a half-written last line is "not yet", never an
/// error and never a terminal record.
pub trait TurnReader: Send + Sync {
    /// Snapshot of where the transcript ends right now. Taken by the core
    /// before it dispatches; `find_input` searches only past it.
    fn cursor(&self, session_id: &str, cwd: Option<&str>) -> Result<Cursor, ReadError>;

    /// Look past `cursor` for the input record carrying `marker` and bind
    /// the turn it started.
    fn find_input(
        &self,
        session_id: &str,
        cwd: Option<&str>,
        marker: &str,
        cursor: &Cursor,
    ) -> Result<InputBinding, ReadError>;

    /// The bound turn's end, or `None` while it is still running.
    fn outcome(
        &self,
        anchor: &TurnAnchor,
        cwd: Option<&str>,
    ) -> Result<Option<TurnOutcome>, ReadError>;
}

/// The reader for a roster `cli` value; `None` for a CLI hive has no
/// transcript reader for.
pub fn reader_for(cli: &str) -> Option<Box<dyn TurnReader>> {
    match cli {
        "claude" => Some(Box::new(super::claude_turn::ClaudeTurnReader)),
        "codex" => Some(Box::new(super::codex_turn::CodexTurnReader)),
        "grok" => Some(Box::new(super::grok_turn::GrokTurnReader)),
        _ => None,
    }
}
