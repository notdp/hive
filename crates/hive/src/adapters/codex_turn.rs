//! Codex turn reader for `hive node run` (see `adapters::turn`).
//! Stub: every call reports the transcript unavailable until the reader
//! lands.

use super::turn::{Cursor, InputBinding, ReadError, TurnAnchor, TurnOutcome, TurnReader};

#[derive(Default)]
pub struct CodexTurnReader;

impl TurnReader for CodexTurnReader {
    fn cursor(&self, _session_id: &str, _cwd: Option<&str>) -> Result<Cursor, ReadError> {
        Err(ReadError::Unavailable(
            "codex turn reader not implemented".into(),
        ))
    }

    fn find_input(
        &self,
        _session_id: &str,
        _cwd: Option<&str>,
        _marker: &str,
        _cursor: &Cursor,
    ) -> Result<InputBinding, ReadError> {
        Err(ReadError::Unavailable(
            "codex turn reader not implemented".into(),
        ))
    }

    fn outcome(
        &self,
        _anchor: &TurnAnchor,
        _cwd: Option<&str>,
    ) -> Result<Option<TurnOutcome>, ReadError> {
        Err(ReadError::Unavailable(
            "codex turn reader not implemented".into(),
        ))
    }
}
