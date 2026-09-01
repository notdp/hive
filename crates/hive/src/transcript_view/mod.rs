//! Read-only live mirror for a Claude session transcript.
//!
//! An interactive Claude session (a desktop ccd, a joined session) has no
//! attachable pty — `claude attach` is job-only, and resuming would fork a
//! second engine. Its truth layer is the transcript JSONL, appended event by
//! event as the turn unfolds, so a faithful renderer over that file IS the
//! mirror: native-looking, keystrokes go nowhere by construction.
//!
//! Parse layer: [`TranscriptParser`] folds raw JSONL lines into typed
//! [`DisplayBlock`]s (user band, aggregated tool group, run, thinking,
//! assistant markdown, worked-for). Blocks can finalize late — a tool group
//! stays open until a non-read event arrives — so the parser exposes both the
//! finalized stream (`push`/`flush`) and a live snapshot (`pending_blocks`).
//! The TUI renders the blocks; the plain non-tty stream below renders the
//! same blocks to the legacy ANSI line format.

/// Grok Build's markdown engine (xai-grok-markdown, Apache-2.0) with the
/// palette derived from the active [`ViewTheme`] (groknight or grokday) —
/// syntax highlighting, tables, headings, the whole surface.
pub(crate) mod grok_md;
mod model;
mod parser;
mod stream;
#[cfg(test)]
mod tests;

pub use model::*;
pub use parser::*;
pub use stream::*;
