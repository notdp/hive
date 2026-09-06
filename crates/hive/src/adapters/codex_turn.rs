//! Codex turn reader for `hive node run` (see `adapters::turn`).
//!
//! A codex rollout (`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ts>-<thread>.jsonl`,
//! resolved through `CodexAdapter::find_session_file`) brackets every turn
//! with `event_msg` records: `task_started {turn_id}`, then the turn's
//! `response_item`s (the user-role input, assistant messages with a
//! `phase`, tool calls and outputs), then one terminal event for the
//! same `turn_id`. Shapes verified on this machine's rollouts:
//!
//! - `task_complete {turn_id, last_agent_message}`: the normal close;
//!   `last_agent_message` is the final answer text, `null` when the turn
//!   produced none.
//! - `turn_aborted {turn_id?, reason: "interrupted"}`: a human cancel;
//!   older builds write it without a `turn_id`. No `task_complete` follows.
//! - `error {message, codex_error_info}`: carries no `turn_id`; in every
//!   sample it is the record right before a `task_complete` with a null
//!   message, so the reader treats it as the turn's end.
//! - A dispatch delivered while a turn runs is steered into it: its
//!   user-role record lands after that turn's output, with no
//!   `task_started` of its own.
//!
//! `CodexAdapter::iter_messages` drops `event_msg` records, so this module
//! walks the raw lines itself. The cursor is the file's identity (path and
//! inode) plus a byte offset at a record boundary; a trailing line without
//! its newline is "not yet" everywhere.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use super::base::{safe_json_loads, SessionAdapter};
use super::codex::CodexAdapter;
use super::turn::{Cursor, InputBinding, ReadError, TurnAnchor, TurnOutcome, TurnReader};

#[derive(Default)]
pub struct CodexTurnReader;

/// The decoded cursor: which file, and where in it.
struct Position {
    path: PathBuf,
    ino: u64,
    offset: u64,
}

impl Position {
    fn encode(&self) -> Cursor {
        json!({
            "path": self.path.to_string_lossy(),
            "ino": self.ino,
            "offset": self.offset,
        })
        .to_string()
    }

    fn decode(cursor: &str) -> Result<Position, ReadError> {
        let map = safe_json_loads(cursor)
            .ok_or_else(|| ReadError::Unavailable(format!("not a codex cursor: {cursor}")))?;
        let path = map.get("path").and_then(Value::as_str);
        let ino = map.get("ino").and_then(Value::as_u64);
        let offset = map.get("offset").and_then(Value::as_u64);
        match (path, ino, offset) {
            (Some(path), Some(ino), Some(offset)) => Ok(Position {
                path: PathBuf::from(path),
                ino,
                offset,
            }),
            _ => Err(ReadError::Unavailable(format!(
                "not a codex cursor: {cursor}"
            ))),
        }
    }
}

/// The rollout as it is on disk right now.
struct Transcript {
    path: PathBuf,
    ino: u64,
    len: u64,
    /// The `session_meta` id on line 1, when that line is complete.
    meta_id: Option<String>,
}

/// One complete record and the offset just past its newline.
struct Record {
    end: u64,
    value: Map<String, Value>,
}

impl Record {
    fn kind(&self) -> &str {
        self.value.get("type").and_then(Value::as_str).unwrap_or("")
    }

    fn payload(&self) -> Option<&Map<String, Value>> {
        match self.value.get("payload") {
            Some(Value::Object(map)) => Some(map),
            _ => None,
        }
    }

    fn payload_type(&self) -> &str {
        self.payload()
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    fn payload_str(&self, key: &str) -> Option<&str> {
        self.payload()
            .and_then(|p| p.get(key))
            .and_then(Value::as_str)
    }

    fn role(&self) -> &str {
        self.payload_str("role").unwrap_or("")
    }

    /// Every `text` string in the message's content items, joined in
    /// order (user `input_text`, assistant `output_text`).
    fn content_text(&self) -> String {
        let Some(Value::Array(items)) = self.payload().and_then(|p| p.get("content")) else {
            return String::new();
        };
        items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A `response_item` that is engine output rather than input: anything
    /// but a user/developer/system message.
    fn is_engine_output(&self) -> bool {
        self.kind() == "response_item"
            && (self.payload_type() != "message" || self.role() == "assistant")
    }
}

fn open_transcript(session_id: &str, cwd: Option<&str>) -> Result<Transcript, ReadError> {
    let path = CodexAdapter
        .find_session_file(session_id, cwd)
        .ok_or_else(|| ReadError::Unavailable(format!("no rollout for session {session_id}")))?;
    let meta = fs::metadata(&path)
        .map_err(|e| ReadError::Unavailable(format!("{}: {e}", path.display())))?;
    let meta_id = first_record(&path)?.and_then(|record| {
        if record.kind() != "session_meta" {
            return None;
        }
        record.payload_str("id").map(str::to_string)
    });
    Ok(Transcript {
        path,
        ino: meta.ino(),
        len: meta.len(),
        meta_id,
    })
}

fn first_record(path: &Path) -> Result<Option<Record>, ReadError> {
    Ok(records_from(path, 0, Some(1))?.into_iter().next())
}

/// Complete records from `offset` to the end of the file (at most `limit`
/// of them). The last line is skipped when its newline has not landed. A
/// complete line that is not a JSON object is schema drift, not noise: a
/// terminal record hidden in it must not be silently skipped.
fn records_from(path: &Path, offset: u64, limit: Option<usize>) -> Result<Vec<Record>, ReadError> {
    let mut file = fs::File::open(path)
        .map_err(|e| ReadError::Unavailable(format!("{}: {e}", path.display())))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| ReadError::Unavailable(format!("{}: {e}", path.display())))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| ReadError::Unavailable(format!("{}: {e}", path.display())))?;
    let mut records = Vec::new();
    let mut start = 0usize;
    while let Some(rel) = bytes[start..].iter().position(|b| *b == b'\n') {
        let end = start + rel + 1;
        let line = String::from_utf8_lossy(&bytes[start..end - 1]);
        let line = line.trim();
        if !line.is_empty() {
            let value = safe_json_loads(line).ok_or_else(|| {
                ReadError::UnsupportedSchema(format!(
                    "{}: line at byte {} is not a JSON object",
                    path.display(),
                    offset + start as u64
                ))
            })?;
            records.push(Record {
                end: offset + end as u64,
                value,
            });
            if limit.is_some_and(|n| records.len() >= n) {
                break;
            }
        }
        start = end;
    }
    Ok(records)
}

/// The offset of the last record boundary at or before `len`: `len` itself
/// when the file ends in a newline, else the start of the unfinished line.
fn boundary_before(path: &Path, len: u64) -> Result<u64, ReadError> {
    let mut file = fs::File::open(path)
        .map_err(|e| ReadError::Unavailable(format!("{}: {e}", path.display())))?;
    let mut window = 64 * 1024u64;
    loop {
        let chunk = window.min(len);
        let start = len - chunk;
        file.seek(SeekFrom::Start(start))
            .map_err(|e| ReadError::Unavailable(format!("{}: {e}", path.display())))?;
        let mut bytes = vec![0u8; chunk as usize];
        file.read_exact(&mut bytes)
            .map_err(|e| ReadError::Unavailable(format!("{}: {e}", path.display())))?;
        if let Some(rel) = bytes.iter().rposition(|b| *b == b'\n') {
            return Ok(start + rel as u64 + 1);
        }
        if start == 0 {
            return Ok(0);
        }
        window *= 4;
    }
}

/// Where the final text of a completed turn came from.
#[derive(Debug, PartialEq, Eq)]
enum FinalTextSource {
    LastAgentMessage,
    FinalAnswerItem,
}

/// One assistant message seen while polling the bound turn.
struct AgentMessage {
    phase: Option<String>,
    text: String,
}

/// The completed turn's text: `last_agent_message` when the engine wrote
/// one, else the last `final_answer` assistant message of the turn.
/// Commentary never stands in for a final answer, and nothing reaches into
/// an earlier turn: the fallback yields an empty string when the turn
/// wrote no final answer.
fn final_text(complete: &Record, messages: &[AgentMessage]) -> (String, FinalTextSource) {
    if let Some(text) = complete.payload_str("last_agent_message") {
        return (text.to_string(), FinalTextSource::LastAgentMessage);
    }
    let text = messages
        .iter()
        .rev()
        .find(|m| m.phase.as_deref().is_none_or(|p| p == "final_answer"))
        .map(|m| m.text.clone())
        .unwrap_or_default();
    (text, FinalTextSource::FinalAnswerItem)
}

fn outcome_from(records: &[Record], anchor: &TurnAnchor) -> Option<TurnOutcome> {
    let mut messages: Vec<AgentMessage> = Vec::new();
    for record in records {
        match record.kind() {
            "session_meta" => {
                let id = record.payload_str("id").unwrap_or("");
                if id != anchor.session {
                    return Some(TurnOutcome::SessionChanged {
                        reason: format!("session_meta {id} after the anchor"),
                    });
                }
            }
            "event_msg" => {
                let turn_id = record.payload_str("turn_id");
                match record.payload_type() {
                    "task_complete" => {
                        if turn_id != Some(anchor.turn.as_str()) {
                            return Some(TurnOutcome::Ambiguous {
                                reason: format!(
                                    "task_complete for turn {} before the bound turn closed",
                                    turn_id.unwrap_or("?")
                                ),
                            });
                        }
                        let (text, _) = final_text(record, &messages);
                        return Some(TurnOutcome::Completed { text });
                    }
                    "turn_aborted" => {
                        let reason = record.payload_str("reason").unwrap_or("");
                        if turn_id.is_some_and(|id| id != anchor.turn) {
                            return Some(TurnOutcome::Ambiguous {
                                reason: format!(
                                    "turn_aborted for turn {} while the bound turn was open",
                                    turn_id.unwrap_or("?")
                                ),
                            });
                        }
                        return Some(TurnOutcome::Interrupted {
                            reason: format!("turn_aborted: {reason}"),
                        });
                    }
                    "error" => {
                        let message = record.payload_str("message").unwrap_or("");
                        let info = record.payload_str("codex_error_info").unwrap_or("");
                        let reason = if info.is_empty() {
                            message.to_string()
                        } else {
                            format!("{message} ({info})")
                        };
                        return Some(TurnOutcome::Failed { reason });
                    }
                    "task_started" => {
                        return Some(TurnOutcome::Ambiguous {
                            reason: format!(
                                "task_started {} before the bound turn closed",
                                turn_id.unwrap_or("?")
                            ),
                        });
                    }
                    "thread_rolled_back" => {
                        return Some(TurnOutcome::Ambiguous {
                            reason: "thread_rolled_back while the bound turn was open".into(),
                        });
                    }
                    _ => {}
                }
            }
            "response_item" if record.payload_type() == "message" => match record.role() {
                "assistant" => messages.push(AgentMessage {
                    phase: record.payload_str("phase").map(str::to_string),
                    text: record.content_text(),
                }),
                "user" => {
                    return Some(TurnOutcome::Ambiguous {
                        reason: "another input steered into the bound turn".into(),
                    });
                }
                _ => {}
            },
            _ => {}
        }
    }
    None
}

// ponytail: a user-role record that precedes the marker inside the same
// turn (codex's own `<environment_context>` / `<user_instructions>`
// injections, a skill expansion) does not disqualify the turn; only engine
// output before the marker does. Two human submissions merged into one
// turn's initial input would look the same and is not told apart.
fn binding_from(
    records: &[Record],
    marker: &str,
    session_id: &str,
    transcript: &Transcript,
) -> InputBinding {
    let mut open_turn: Option<String> = None;
    let mut turn_has_output = false;
    for record in records {
        match record.kind() {
            "event_msg" => match record.payload_type() {
                "task_started" => {
                    open_turn = record.payload_str("turn_id").map(str::to_string);
                    turn_has_output = false;
                }
                "task_complete" | "turn_aborted" => open_turn = None,
                _ => {}
            },
            "response_item" => {
                if record.is_engine_output() {
                    turn_has_output |= open_turn.is_some();
                    continue;
                }
                if record.payload_type() != "message"
                    || record.role() != "user"
                    || !record.content_text().contains(marker)
                {
                    continue;
                }
                return match &open_turn {
                    None => InputBinding::Ambiguous(
                        "input landed with no task_started after the cursor (steered into a turn already running)"
                            .into(),
                    ),
                    Some(turn) if turn_has_output => InputBinding::Ambiguous(format!(
                        "input steered into running turn {turn}"
                    )),
                    Some(turn) => InputBinding::Bound(TurnAnchor {
                        session: session_id.to_string(),
                        turn: turn.clone(),
                        cursor: Position {
                            path: transcript.path.clone(),
                            ino: transcript.ino,
                            offset: record.end,
                        }
                        .encode(),
                    }),
                };
            }
            _ => {}
        }
    }
    InputBinding::NotYet
}

impl TurnReader for CodexTurnReader {
    fn cursor(&self, session_id: &str, cwd: Option<&str>) -> Result<Cursor, ReadError> {
        let transcript = open_transcript(session_id, cwd)?;
        let offset = boundary_before(&transcript.path, transcript.len)?;
        Ok(Position {
            path: transcript.path,
            ino: transcript.ino,
            offset,
        }
        .encode())
    }

    fn find_input(
        &self,
        session_id: &str,
        cwd: Option<&str>,
        marker: &str,
        cursor: &Cursor,
    ) -> Result<InputBinding, ReadError> {
        let position = Position::decode(cursor)?;
        let transcript = open_transcript(session_id, cwd)?;
        if transcript.path != position.path || transcript.ino != position.ino {
            return Ok(InputBinding::Ambiguous(format!(
                "rollout replaced since the cursor ({} -> {})",
                position.path.display(),
                transcript.path.display()
            )));
        }
        if transcript.len < position.offset {
            return Ok(InputBinding::Ambiguous(
                "rollout shorter than the cursor".into(),
            ));
        }
        let records = records_from(&transcript.path, position.offset, None)?;
        Ok(binding_from(&records, marker, session_id, &transcript))
    }

    fn outcome(
        &self,
        anchor: &TurnAnchor,
        cwd: Option<&str>,
    ) -> Result<Option<TurnOutcome>, ReadError> {
        let position = Position::decode(&anchor.cursor)?;
        let transcript = open_transcript(&anchor.session, cwd)?;
        if transcript.path != position.path || transcript.ino != position.ino {
            return Ok(Some(TurnOutcome::SessionChanged {
                reason: format!(
                    "rollout replaced since the anchor ({} -> {})",
                    position.path.display(),
                    transcript.path.display()
                ),
            }));
        }
        if transcript.len < position.offset {
            return Ok(Some(TurnOutcome::SessionChanged {
                reason: "rollout shorter than the anchor".into(),
            }));
        }
        if let Some(id) = &transcript.meta_id {
            if *id != anchor.session {
                return Ok(Some(TurnOutcome::SessionChanged {
                    reason: format!("session_meta id is {id}"),
                }));
            }
        }
        let records = records_from(&transcript.path, position.offset, None)?;
        Ok(outcome_from(&records, anchor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;
    use std::io::Write;

    const SESSION: &str = "01a0763d-9ce2-7b53-958f-bd8827cd8006";
    const TURN_A: &str = "01a07640-7e48-7f62-928e-f6c34c7eda09";
    const TURN_B: &str = "01a0765b-98e6-72e0-9e29-d80767b29f52";
    const MARKER: &str = "nd-0123456789ab";

    struct Fixture {
        _env: EnvGuard,
        _tmp: tempfile::TempDir,
        path: PathBuf,
    }

    /// A temp `CODEX_HOME` holding one rollout for `SESSION` whose first
    /// line is the session_meta.
    fn fixture() -> Fixture {
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("CODEX_HOME", tmp.path());
        let dir = tmp.path().join("sessions/2026/09/06");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-09-06T18-22-24-{SESSION}.jsonl"));
        let fx = Fixture {
            _env: env,
            _tmp: tmp,
            path,
        };
        append(&fx, &[session_meta(SESSION)]);
        fx
    }

    fn append(fx: &Fixture, lines: &[Value]) {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&fx.path)
            .unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    fn append_raw(fx: &Fixture, text: &str) {
        let mut file = fs::OpenOptions::new().append(true).open(&fx.path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn session_meta(id: &str) -> Value {
        json!({"timestamp": "2026-09-06T10:22:24.000Z", "type": "session_meta",
               "payload": {"id": id, "cwd": "/any", "timestamp": "2026-09-06T10:22:24.000Z"}})
    }

    fn task_started(turn: &str) -> Value {
        json!({"timestamp": "2026-09-06T10:55:09.552Z", "type": "event_msg",
               "payload": {"type": "task_started", "turn_id": turn, "started_at": 1788692109}})
    }

    fn task_complete(turn: &str, last: Value) -> Value {
        json!({"timestamp": "2026-09-06T10:55:12.758Z", "type": "event_msg",
               "payload": {"type": "task_complete", "turn_id": turn, "last_agent_message": last,
                           "started_at": 1788692109, "completed_at": 1788692112}})
    }

    fn user(text: &str) -> Value {
        json!({"timestamp": "2026-09-06T10:55:09.626Z", "type": "response_item",
               "payload": {"type": "message", "id": "msg_u", "role": "user",
                           "content": [{"type": "input_text", "text": text}]}})
    }

    fn assistant(phase: &str, text: &str) -> Value {
        json!({"timestamp": "2026-09-06T10:55:12.666Z", "type": "response_item",
               "payload": {"type": "message", "id": "msg_a", "role": "assistant",
                           "content": [{"type": "output_text", "text": text}], "phase": phase}})
    }

    fn tool_output() -> Value {
        json!({"timestamp": "2026-09-06T10:55:11.000Z", "type": "response_item",
               "payload": {"type": "custom_tool_call_output", "call_id": "call_1",
                           "output": [{"type": "input_text", "text": "ok"}]}})
    }

    fn token_count() -> Value {
        json!({"timestamp": "2026-09-06T10:55:12.754Z", "type": "event_msg",
               "payload": {"type": "token_count", "info": null}})
    }

    fn dispatch() -> String {
        format!("<HIVE to=hornet.sage artifact=/ws/artifacts/tasks/sage-{MARKER}.md>\ntask {MARKER}\nsay hi\n</HIVE>")
    }

    fn bind(cursor: &Cursor) -> TurnAnchor {
        match CodexTurnReader
            .find_input(SESSION, None, MARKER, cursor)
            .unwrap()
        {
            InputBinding::Bound(anchor) => anchor,
            other => panic!("expected Bound, got {other:?}"),
        }
    }

    fn outcome(anchor: &TurnAnchor) -> Option<TurnOutcome> {
        CodexTurnReader.outcome(anchor, None).unwrap()
    }

    #[test]
    fn test_codex_turn_missing_rollout_is_unavailable() {
        let fx = fixture();
        let err = CodexTurnReader.cursor("deadbeef-0000-0000-0000-000000000000", None);
        assert!(matches!(err, Err(ReadError::Unavailable(_))), "{err:?}");
        let _ = fx;
    }

    #[test]
    fn test_codex_turn_bound_and_completed_via_last_agent_message() {
        let fx = fixture();
        append(
            &fx,
            &[
                task_started(TURN_A),
                user("earlier"),
                task_complete(TURN_A, json!("old")),
            ],
        );
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        assert_eq!(
            CodexTurnReader
                .find_input(SESSION, None, MARKER, &cursor)
                .unwrap(),
            InputBinding::NotYet
        );

        append(&fx, &[task_started(TURN_B), user(&dispatch())]);
        let anchor = bind(&cursor);
        assert_eq!(anchor.session, SESSION);
        assert_eq!(anchor.turn, TURN_B);
        assert_eq!(outcome(&anchor), None);

        append(
            &fx,
            &[
                assistant("commentary", "working"),
                tool_output(),
                assistant("final_answer", "probe done"),
                token_count(),
                task_complete(TURN_B, json!("probe done")),
            ],
        );
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: "probe done".into()
            })
        );
    }

    #[test]
    fn test_codex_turn_null_last_agent_message_falls_back_to_final_answer() {
        let fx = fixture();
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append(&fx, &[task_started(TURN_B), user(&dispatch())]);
        let anchor = bind(&cursor);
        append(
            &fx,
            &[
                assistant("commentary", "thinking aloud"),
                assistant("final_answer", "line one"),
                task_complete(TURN_B, Value::Null),
            ],
        );
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: "line one".into()
            })
        );
        let records = records_from(&fx.path, 0, None).unwrap();
        let complete = records.last().unwrap();
        let messages = vec![AgentMessage {
            phase: Some("final_answer".into()),
            text: "line one".into(),
        }];
        assert_eq!(
            final_text(complete, &messages),
            ("line one".into(), FinalTextSource::FinalAnswerItem)
        );
    }

    #[test]
    fn test_codex_turn_null_last_agent_message_without_final_answer_is_empty() {
        let fx = fixture();
        append(
            &fx,
            &[
                task_started(TURN_A),
                assistant("final_answer", "old answer"),
                task_complete(TURN_A, json!("old answer")),
            ],
        );
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append(&fx, &[task_started(TURN_B), user(&dispatch())]);
        let anchor = bind(&cursor);
        append(
            &fx,
            &[
                assistant("commentary", "only commentary"),
                task_complete(TURN_B, Value::Null),
            ],
        );
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: String::new()
            })
        );
    }

    #[test]
    fn test_codex_turn_empty_last_agent_message_stays_empty() {
        let fx = fixture();
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append(&fx, &[task_started(TURN_B), user(&dispatch())]);
        let anchor = bind(&cursor);
        append(
            &fx,
            &[
                assistant("final_answer", "text"),
                task_complete(TURN_B, json!("")),
            ],
        );
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: String::new()
            })
        );
    }

    #[test]
    fn test_codex_turn_marker_without_task_started_is_ambiguous() {
        let fx = fixture();
        append(
            &fx,
            &[
                task_started(TURN_A),
                user("human prompt"),
                assistant("commentary", "on it"),
            ],
        );
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append(&fx, &[user(&dispatch())]);
        let binding = CodexTurnReader
            .find_input(SESSION, None, MARKER, &cursor)
            .unwrap();
        assert!(
            matches!(binding, InputBinding::Ambiguous(ref why) if why.contains("no task_started")),
            "{binding:?}"
        );
    }

    #[test]
    fn test_codex_turn_steer_into_open_turn_is_ambiguous() {
        let fx = fixture();
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append(
            &fx,
            &[
                task_started(TURN_A),
                user("human prompt"),
                assistant("commentary", "on it"),
                tool_output(),
                user(&dispatch()),
            ],
        );
        let binding = CodexTurnReader
            .find_input(SESSION, None, MARKER, &cursor)
            .unwrap();
        assert_eq!(
            binding,
            InputBinding::Ambiguous(format!("input steered into running turn {TURN_A}"))
        );
    }

    #[test]
    fn test_codex_turn_context_injection_before_marker_still_binds() {
        let fx = fixture();
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append(
            &fx,
            &[
                task_started(TURN_B),
                user("<environment_context>\n  <cwd>/x</cwd>\n</environment_context>"),
                user(&dispatch()),
            ],
        );
        assert_eq!(bind(&cursor).turn, TURN_B);
    }

    #[test]
    fn test_codex_turn_aborted_is_interrupted() {
        let fx = fixture();
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append(&fx, &[task_started(TURN_B), user(&dispatch())]);
        let anchor = bind(&cursor);
        append(
            &fx,
            &[
                json!({"timestamp": "2026-09-06T10:55:12.000Z", "type": "event_msg",
                     "payload": {"type": "turn_aborted", "turn_id": TURN_B, "reason": "interrupted",
                                 "completed_at": 1788692112, "duration_ms": 2074}}),
            ],
        );
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Interrupted {
                reason: "turn_aborted: interrupted".into()
            })
        );
    }

    #[test]
    fn test_codex_turn_aborted_without_turn_id_is_interrupted() {
        let fx = fixture();
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append(&fx, &[task_started(TURN_B), user(&dispatch())]);
        let anchor = bind(&cursor);
        append(
            &fx,
            &[
                json!({"timestamp": "2026-09-06T10:55:12.000Z", "type": "event_msg",
                     "payload": {"type": "turn_aborted", "reason": "interrupted"}}),
            ],
        );
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Interrupted {
                reason: "turn_aborted: interrupted".into()
            })
        );
    }

    #[test]
    fn test_codex_turn_error_event_is_failed() {
        let fx = fixture();
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append(&fx, &[task_started(TURN_B), user(&dispatch())]);
        let anchor = bind(&cursor);
        append(
            &fx,
            &[
                json!({"timestamp": "2026-09-06T10:55:12.000Z", "type": "event_msg",
                       "payload": {"type": "error",
                                   "message": "stream disconnected before completion",
                                   "codex_error_info": "other"}}),
                task_complete(TURN_B, Value::Null),
            ],
        );
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Failed {
                reason: "stream disconnected before completion (other)".into()
            })
        );
    }

    #[test]
    fn test_codex_turn_new_task_started_before_close_is_ambiguous() {
        let fx = fixture();
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append(&fx, &[task_started(TURN_B), user(&dispatch())]);
        let anchor = bind(&cursor);
        append(&fx, &[task_started(TURN_A)]);
        assert!(
            matches!(outcome(&anchor), Some(TurnOutcome::Ambiguous { ref reason }) if reason.contains(TURN_A))
        );
    }

    #[test]
    fn test_codex_turn_second_input_in_bound_turn_is_ambiguous() {
        let fx = fixture();
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append(&fx, &[task_started(TURN_B), user(&dispatch())]);
        let anchor = bind(&cursor);
        append(
            &fx,
            &[
                assistant("commentary", "on it"),
                user("<HIVE from=hornet.orch to=hornet.sage>\nalso do this\n</HIVE>"),
                task_complete(TURN_B, json!("did both")),
            ],
        );
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Ambiguous {
                reason: "another input steered into the bound turn".into()
            })
        );
    }

    #[test]
    fn test_codex_turn_file_shorter_than_anchor_is_session_changed() {
        let fx = fixture();
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append(&fx, &[task_started(TURN_B), user(&dispatch())]);
        let anchor = bind(&cursor);
        fs::write(&fx.path, format!("{}\n", session_meta(SESSION))).unwrap();
        assert!(matches!(
            outcome(&anchor),
            Some(TurnOutcome::SessionChanged { .. })
        ));
    }

    #[test]
    fn test_codex_turn_replaced_rollout_is_session_changed() {
        let fx = fixture();
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append(&fx, &[task_started(TURN_B), user(&dispatch())]);
        let anchor = bind(&cursor);
        let body = fs::read(&fx.path).unwrap();
        fs::remove_file(&fx.path).unwrap();
        fs::write(&fx.path, body).unwrap();
        append(&fx, &[task_complete(TURN_B, json!("done"))]);
        assert!(matches!(
            outcome(&anchor),
            Some(TurnOutcome::SessionChanged { ref reason }) if reason.contains("replaced")
        ));
    }

    #[test]
    fn test_codex_turn_foreign_session_meta_is_session_changed() {
        let fx = fixture();
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append(&fx, &[task_started(TURN_B), user(&dispatch())]);
        let anchor = bind(&cursor);
        append(&fx, &[session_meta("ffffffff-0000-0000-0000-000000000000")]);
        assert!(matches!(
            outcome(&anchor),
            Some(TurnOutcome::SessionChanged { ref reason }) if reason.contains("ffffffff")
        ));
    }

    #[test]
    fn test_codex_turn_truncated_trailing_line_is_not_yet() {
        let fx = fixture();
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append(&fx, &[task_started(TURN_B)]);
        let user_line = user(&dispatch()).to_string();
        append_raw(&fx, &user_line[..user_line.len() / 2]);
        assert_eq!(
            CodexTurnReader
                .find_input(SESSION, None, MARKER, &cursor)
                .unwrap(),
            InputBinding::NotYet
        );
        append_raw(&fx, &format!("{}\n", &user_line[user_line.len() / 2..]));
        let anchor = bind(&cursor);

        let complete = task_complete(TURN_B, json!("done")).to_string();
        append_raw(&fx, &complete[..complete.len() - 4]);
        assert_eq!(outcome(&anchor), None);
        append_raw(&fx, &format!("{}\n", &complete[complete.len() - 4..]));
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: "done".into()
            })
        );
    }

    #[test]
    fn test_codex_turn_cursor_backs_up_over_partial_line() {
        let fx = fixture();
        let started = task_started(TURN_B).to_string();
        append_raw(&fx, &started[..started.len() / 2]);
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        let position = Position::decode(&cursor).unwrap();
        assert_eq!(
            position.offset,
            fs::metadata(&fx.path).unwrap().len() - (started.len() / 2) as u64
        );
        append_raw(&fx, &format!("{}\n", &started[started.len() / 2..]));
        append(&fx, &[user(&dispatch())]);
        assert_eq!(bind(&cursor).turn, TURN_B);
    }

    #[test]
    fn test_codex_turn_binds_second_of_two_short_turns() {
        let fx = fixture();
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append(
            &fx,
            &[
                task_started(TURN_A),
                user("quick question"),
                assistant("final_answer", "quick answer"),
                task_complete(TURN_A, json!("quick answer")),
                task_started(TURN_B),
                user(&dispatch()),
                assistant("final_answer", "probe done"),
                task_complete(TURN_B, json!("probe done")),
            ],
        );
        let anchor = bind(&cursor);
        assert_eq!(anchor.turn, TURN_B);
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: "probe done".into()
            })
        );
    }

    #[test]
    fn test_codex_turn_earlier_turn_text_never_leaks() {
        let fx = fixture();
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append(
            &fx,
            &[
                task_started(TURN_A),
                user("decoy"),
                assistant("final_answer", "decoy answer"),
                task_complete(TURN_A, json!("decoy answer")),
                task_started(TURN_B),
                user(&dispatch()),
                task_complete(TURN_B, Value::Null),
            ],
        );
        let anchor = bind(&cursor);
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: String::new()
            })
        );
    }

    #[test]
    fn test_codex_turn_complete_bad_line_is_unsupported_schema() {
        let fx = fixture();
        let cursor = CodexTurnReader.cursor(SESSION, None).unwrap();
        append_raw(&fx, "not json\n");
        let err = CodexTurnReader.find_input(SESSION, None, MARKER, &cursor);
        assert!(
            matches!(err, Err(ReadError::UnsupportedSchema(_))),
            "{err:?}"
        );
    }

    #[test]
    fn test_codex_turn_cursor_round_trips() {
        let position = Position {
            path: PathBuf::from("/x/rollout-a.jsonl"),
            ino: 42,
            offset: 1234,
        };
        let decoded = Position::decode(&position.encode()).unwrap();
        assert_eq!(decoded.path, position.path);
        assert_eq!(decoded.ino, 42);
        assert_eq!(decoded.offset, 1234);
        assert!(matches!(
            Position::decode("nope"),
            Err(ReadError::Unavailable(_))
        ));
    }
}
