//! Grok turn reader for `hive node run` (see `adapters::turn`).
//!
//! A grok session directory (`GrokAdapter::find_session_file` resolves it)
//! holds two streams the reader pairs by input identity:
//!
//! - `chat_history.jsonl`: the conversation. A user record that started a
//!   prompt turn carries `prompt_index`, the same coordinate as the turn
//!   number; a user record pushed mid-turn (a `<system-reminder>`, an
//!   interjection folded into the running turn) carries `synthetic_reason`
//!   and no `prompt_index`. Reasoning, tool results and assistant records
//!   follow the prompt inside the turn.
//! - `events.jsonl`: `turn_started {session_id, turn_number, …}` and
//!   `turn_ended {outcome, cancellation_category?, …}`. `turn_ended` has no
//!   turn number, so start and end pair by order within the stream; an
//!   `interjected` event between them records input folded into the turn.
//!
//! `conversation_message_count` on `turn_started` is not the history line
//! index (turn 0 of a live session said 2 with the prompt on line 5), so
//! the reader never uses it: the prompt record's `prompt_index` is the
//! turn number, and the marker's record binds the `turn_started` with that
//! number. Cursors are byte offsets past the last complete line of each
//! file, so a half-written trailing line is invisible until its newline
//! lands.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::base::{safe_json_loads, SessionAdapter};
use super::grok::{GrokAdapter, HISTORY_NAME};
use super::turn::{Cursor, InputBinding, ReadError, TurnAnchor, TurnOutcome, TurnReader};

const EVENTS_NAME: &str = "events.jsonl";

#[derive(Default)]
pub struct GrokTurnReader;

/// The reader's cursor: byte offsets past the last complete line of each
/// stream, plus (once bound) the offset of the bound prompt record.
struct Offsets {
    events: u64,
    history: u64,
    input: Option<u64>,
}

impl Offsets {
    fn encode(&self) -> Cursor {
        let mut out = format!("e={};h={}", self.events, self.history);
        if let Some(input) = self.input {
            out.push_str(&format!(";u={input}"));
        }
        out
    }

    fn decode(cursor: &str) -> Result<Self, ReadError> {
        let mut events = None;
        let mut history = None;
        let mut input = None;
        for field in cursor.split(';') {
            let (key, value) = field.split_once('=').ok_or_else(|| bad_cursor(cursor))?;
            let value: u64 = value.parse().map_err(|_| bad_cursor(cursor))?;
            match key {
                "e" => events = Some(value),
                "h" => history = Some(value),
                "u" => input = Some(value),
                _ => return Err(bad_cursor(cursor)),
            }
        }
        match (events, history) {
            (Some(events), Some(history)) => Ok(Offsets {
                events,
                history,
                input,
            }),
            _ => Err(bad_cursor(cursor)),
        }
    }
}

fn bad_cursor(cursor: &str) -> ReadError {
    ReadError::UnsupportedSchema(format!("unreadable grok cursor `{cursor}`"))
}

/// One complete JSONL record and where its line sits in the file.
struct Line {
    start: u64,
    end: u64,
    record: Map<String, Value>,
}

/// Every complete record from byte `from` on. `None` when the file is
/// missing past a non-zero cursor or shorter than `from` (a rewrite under
/// the cursor); a missing file at offset 0 is an empty stream. A complete
/// line that is not a JSON object is schema drift, not a skip.
fn read_from(path: &Path, from: u64) -> Result<Option<Vec<Line>>, ReadError> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(_) if from == 0 => return Ok(Some(Vec::new())),
        Err(_) => return Ok(None),
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len < from {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(from))
        .map_err(|e| ReadError::Unavailable(format!("{}: {e}", path.display())))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| ReadError::Unavailable(format!("{}: {e}", path.display())))?;
    let mut lines = Vec::new();
    let mut start = 0usize;
    while let Some(nl) = bytes[start..].iter().position(|b| *b == b'\n') {
        let end = start + nl + 1;
        let text = String::from_utf8_lossy(&bytes[start..end - 1]);
        if !text.trim().is_empty() {
            let record = safe_json_loads(text.trim()).ok_or_else(|| {
                ReadError::UnsupportedSchema(format!(
                    "{} byte {}: not a JSON object",
                    path.display(),
                    from + start as u64
                ))
            })?;
            lines.push(Line {
                start: from + start as u64,
                end: from + end as u64,
                record,
            });
        }
        start = end;
    }
    Ok(Some(lines))
}

/// Byte offset just past the last complete line; 0 for a missing file.
fn complete_len(path: &Path) -> u64 {
    let Ok(mut file) = fs::File::open(path) else {
        return 0;
    };
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return 0;
    }
    bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|i| i as u64 + 1)
        .unwrap_or(0)
}

fn session_dir(session_id: &str, cwd: Option<&str>) -> Option<PathBuf> {
    GrokAdapter
        .find_session_file(session_id, cwd)
        .and_then(|history| history.parent().map(Path::to_path_buf))
}

fn record_type(record: &Map<String, Value>) -> &str {
    record.get("type").and_then(Value::as_str).unwrap_or("")
}

fn str_field<'a>(record: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    record.get(key).and_then(Value::as_str)
}

/// Every text of a history record's `content`: the string itself, or the
/// `text` blocks of an array joined by newlines; nothing for an empty or
/// absent content.
fn content_text(record: &Map<String, Value>) -> String {
    match record.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| {
                let map = block.as_object()?;
                if map.get("type").and_then(Value::as_str) == Some("text") {
                    Some(map.get("text").and_then(Value::as_str).unwrap_or(""))
                } else {
                    None
                }
            })
            .collect::<Vec<&str>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn prompt_index(record: &Map<String, Value>) -> Option<u64> {
    record.get("prompt_index").and_then(Value::as_u64)
}

fn turn_number(anchor: &TurnAnchor) -> Result<u64, ReadError> {
    anchor
        .turn
        .rsplit_once('/')
        .and_then(|(_, n)| n.parse().ok())
        .ok_or_else(|| {
            ReadError::UnsupportedSchema(format!("unreadable grok turn key `{}`", anchor.turn))
        })
}

fn ambiguous(reason: String) -> Result<Option<TurnOutcome>, ReadError> {
    Ok(Some(TurnOutcome::Ambiguous { reason }))
}

impl TurnReader for GrokTurnReader {
    fn cursor(&self, session_id: &str, cwd: Option<&str>) -> Result<Cursor, ReadError> {
        let dir = session_dir(session_id, cwd)
            .ok_or_else(|| ReadError::Unavailable(format!("no grok session {session_id}")))?;
        let history = complete_len(&dir.join(HISTORY_NAME));
        let events = complete_len(&dir.join(EVENTS_NAME));
        Ok(Offsets {
            events,
            history,
            input: None,
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
        let dir = session_dir(session_id, cwd)
            .ok_or_else(|| ReadError::Unavailable(format!("no grok session {session_id}")))?;
        let offsets = Offsets::decode(cursor)?;
        let Some(history) = read_from(&dir.join(HISTORY_NAME), offsets.history)? else {
            return Ok(InputBinding::Ambiguous(
                "chat history rewritten below the cursor".into(),
            ));
        };
        let Some(input) = history.iter().find(|line| {
            record_type(&line.record) == "user" && content_text(&line.record).contains(marker)
        }) else {
            return Ok(InputBinding::NotYet);
        };
        let Some(number) = prompt_index(&input.record) else {
            let reason = str_field(&input.record, "synthetic_reason").unwrap_or("unknown");
            return Ok(InputBinding::Ambiguous(format!(
                "marker landed as a mid-turn user record ({reason}), not a prompt of its own"
            )));
        };
        let Some(events) = read_from(&dir.join(EVENTS_NAME), offsets.events)? else {
            return Ok(InputBinding::Ambiguous(
                "events rewritten below the cursor".into(),
            ));
        };
        let started = events.iter().find(|line| {
            record_type(&line.record) == "turn_started"
                && line.record.get("turn_number").and_then(Value::as_u64) == Some(number)
        });
        let Some(started) = started else {
            return Ok(InputBinding::NotYet);
        };
        let owner = str_field(&started.record, "session_id").unwrap_or("");
        if owner != session_id {
            return Ok(InputBinding::Ambiguous(format!(
                "turn {number} started in session {owner}, not {session_id}"
            )));
        }
        Ok(InputBinding::Bound(TurnAnchor {
            session: session_id.to_string(),
            turn: format!("{session_id}/{number}"),
            cursor: Offsets {
                events: started.end,
                history: input.end,
                input: Some(input.start),
            }
            .encode(),
        }))
    }

    fn outcome(
        &self,
        anchor: &TurnAnchor,
        cwd: Option<&str>,
    ) -> Result<Option<TurnOutcome>, ReadError> {
        let number = turn_number(anchor)?;
        let offsets = Offsets::decode(&anchor.cursor)?;
        let Some(dir) = session_dir(&anchor.session, cwd) else {
            return Ok(Some(TurnOutcome::SessionChanged {
                reason: format!("grok session {} is gone", anchor.session),
            }));
        };

        let rewritten = format!("chat history rewritten under the anchor of turn {number}");
        let input = offsets.input.unwrap_or(offsets.history);
        let Some(history) = read_from(&dir.join(HISTORY_NAME), input)? else {
            return ambiguous(rewritten);
        };
        let anchored = history.first().is_some_and(|line| {
            line.end == offsets.history
                && record_type(&line.record) == "user"
                && prompt_index(&line.record) == Some(number)
        });
        if !anchored {
            return ambiguous(rewritten);
        }
        let span = &history[1..];

        let Some(events) = read_from(&dir.join(EVENTS_NAME), offsets.events)? else {
            return ambiguous(format!(
                "events rewritten under the anchor of turn {number}"
            ));
        };
        let mut ended = None;
        for (index, line) in events.iter().enumerate() {
            match record_type(&line.record) {
                "turn_started" => {
                    let owner = str_field(&line.record, "session_id").unwrap_or("");
                    if owner != anchor.session {
                        return Ok(Some(TurnOutcome::SessionChanged {
                            reason: format!("events now belong to session {owner}"),
                        }));
                    }
                    let next = line.record.get("turn_number").and_then(Value::as_u64);
                    return ambiguous(format!(
                        "turn {} started before turn {number} ended",
                        next.map(|n| n.to_string()).unwrap_or_default()
                    ));
                }
                "interjected" => {
                    let source = str_field(&line.record, "source").unwrap_or("unknown");
                    return ambiguous(format!(
                        "input folded into turn {number} mid-turn (source {source})"
                    ));
                }
                "turn_ended" => {
                    ended = Some((index, &line.record));
                    break;
                }
                _ => {}
            }
        }
        let Some((index, ended)) = ended else {
            return Ok(None);
        };
        let outcome = str_field(ended, "outcome").unwrap_or("");
        match outcome {
            "completed" => {}
            "cancelled" => {
                let reason = match str_field(ended, "cancellation_category") {
                    Some(category) => format!("{outcome}: {category}"),
                    None => outcome.to_string(),
                };
                return Ok(Some(TurnOutcome::Interrupted { reason }));
            }
            "error" => {
                let reason = match ended.get("cancellation_context") {
                    Some(context) => format!("{outcome}: {context}"),
                    None => outcome.to_string(),
                };
                return Ok(Some(TurnOutcome::Failed { reason }));
            }
            other => {
                return ambiguous(format!(
                    "turn {number} ended with unknown outcome `{other}`"
                ))
            }
        }

        let mut text = None;
        let mut closed = events[index + 1..]
            .iter()
            .any(|line| record_type(&line.record) == "turn_started");
        for line in span {
            match record_type(&line.record) {
                "user" if prompt_index(&line.record).is_some() => {
                    closed = true;
                    break;
                }
                "assistant" => text = Some(content_text(&line.record)),
                _ => {}
            }
        }
        match text {
            Some(text) => Ok(Some(TurnOutcome::Completed { text })),
            // turn_ended lands before the assistant record is flushed: wait
            // for it unless something after the turn shows nothing is coming.
            None if closed => Ok(Some(TurnOutcome::Completed {
                text: String::new(),
            })),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;
    use serde_json::json;
    use std::io::Write;

    const SESSION: &str = "8adb11be-1f39-4b39-a9a0-b962cc93f126";
    const CWD: &str = "/Users/dp/work/hive";
    const MARKER: &str = "nd-0123456789ab";

    struct Fixture {
        _env: EnvGuard,
        _tmp: tempfile::TempDir,
        dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let mut env = EnvGuard::new();
            let tmp = tempfile::tempdir().unwrap();
            env.set("HOME", tmp.path());
            env.remove("GROK_HOME");
            let dir = tmp
                .path()
                .join(".grok/sessions/%2FUsers%2Fdp%2Fwork%2Fhive")
                .join(SESSION);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(HISTORY_NAME), "").unwrap();
            Fixture {
                _env: env,
                _tmp: tmp,
                dir,
            }
        }

        fn append(&self, name: &str, text: &str) {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.dir.join(name))
                .unwrap();
            file.write_all(text.as_bytes()).unwrap();
        }

        fn history(&self, records: &[Value]) {
            let text: String = records.iter().map(|r| r.to_string() + "\n").collect();
            self.append(HISTORY_NAME, &text);
        }

        fn events(&self, records: &[Value]) {
            let text: String = records.iter().map(|r| r.to_string() + "\n").collect();
            self.append(EVENTS_NAME, &text);
        }

        fn cursor(&self) -> Cursor {
            GrokTurnReader.cursor(SESSION, Some(CWD)).unwrap()
        }

        fn find(&self, cursor: &Cursor) -> InputBinding {
            GrokTurnReader
                .find_input(SESSION, Some(CWD), MARKER, cursor)
                .unwrap()
        }

        fn bind(&self, cursor: &Cursor) -> TurnAnchor {
            match self.find(cursor) {
                InputBinding::Bound(anchor) => anchor,
                other => panic!("expected a bound turn, got {other:?}"),
            }
        }

        fn outcome(&self, anchor: &TurnAnchor) -> Option<TurnOutcome> {
            GrokTurnReader.outcome(anchor, Some(CWD)).unwrap()
        }
    }

    fn prompt(index: u64, text: &str) -> Value {
        json!({"type": "user", "content": [{"type": "text", "text": format!("<user_query>\n{text}\n</user_query>")}], "prompt_index": index})
    }

    fn reminder(text: &str) -> Value {
        json!({"type": "user", "content": [{"type": "text", "text": format!("<system-reminder>\n{text}\n</system-reminder>")}], "synthetic_reason": "system_reminder"})
    }

    fn assistant(content: Value) -> Value {
        json!({"type": "assistant", "content": content, "model_id": "grok-4.6-build"})
    }

    fn turn_started(number: u64) -> Value {
        turn_started_in(SESSION, number)
    }

    fn turn_started_in(session: &str, number: u64) -> Value {
        json!({"ts": "2026-09-06T10:55:19.237Z", "type": "turn_started", "session_id": session, "turn_number": number, "model_id": "grok-4.6", "yolo_mode": true, "conversation_message_count": 2, "session_relationship": "primary", "schema_version": "1.0"})
    }

    fn turn_ended(outcome: &str) -> Value {
        json!({"ts": "2026-09-06T10:55:49.069Z", "type": "turn_ended", "outcome": outcome})
    }

    fn phase(name: &str) -> Value {
        json!({"ts": "2026-09-06T10:55:20.000Z", "type": "phase_changed", "phase": name})
    }

    /// A session that already ran turn 0 (the `/hive` join), idle.
    fn idle_after_turn_zero() -> Fixture {
        let fx = Fixture::new();
        fx.history(&[
            json!({"type": "system", "content": "You are Grok 4.6."}),
            reminder("skills"),
            prompt(0, "/hive hornet"),
            json!({"type": "reasoning", "content": null}),
            assistant(json!("已在 hornet，我是 wren。")),
        ]);
        fx.events(&[
            turn_started(0),
            phase("streaming_text"),
            turn_ended("completed"),
        ]);
        fx
    }

    #[test]
    fn test_bound_turn_completes_with_final_assistant_text() {
        let fx = idle_after_turn_zero();
        let cursor = fx.cursor();
        fx.events(&[turn_started(1), phase("waiting_for_model")]);
        fx.history(&[
            prompt(
                1,
                &format!("<HIVE to=hornet.wren>\ntask {MARKER}\nsay it\n</HIVE>"),
            ),
            json!({"type": "reasoning", "content": null}),
            assistant(json!([{"type": "text", "text": "先看一眼。"}])),
            json!({"type": "tool_result", "tool_call_id": "c1", "content": "exit: 0"}),
            reminder("MCP servers connected"),
        ]);

        let anchor = fx.bind(&cursor);
        assert_eq!(anchor.session, SESSION);
        assert_eq!(anchor.turn, format!("{SESSION}/1"));
        assert_eq!(fx.outcome(&anchor), None, "turn still running");

        fx.history(&[assistant(json!([
            {"type": "text", "text": "probe 完成"},
            {"type": "image", "url": "x"},
            {"type": "text", "text": "路径 /tmp/out.md"}
        ]))]);
        fx.events(&[turn_ended("completed")]);
        assert_eq!(
            fx.outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: "probe 完成\n路径 /tmp/out.md".into()
            })
        );
    }

    #[test]
    fn test_empty_assistant_content_completes_empty_without_walking_back() {
        let fx = idle_after_turn_zero();
        let cursor = fx.cursor();
        fx.events(&[turn_started(1)]);
        fx.history(&[prompt(1, MARKER), assistant(json!(""))]);
        fx.events(&[turn_ended("completed")]);

        let anchor = fx.bind(&cursor);
        assert_eq!(
            fx.outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: String::new()
            })
        );
    }

    #[test]
    fn test_cancelled_turn_is_interrupted_with_category() {
        let fx = idle_after_turn_zero();
        let cursor = fx.cursor();
        fx.events(&[turn_started(1)]);
        fx.history(&[prompt(1, MARKER)]);
        let anchor = fx.bind(&cursor);

        fx.events(&[json!({"ts": "2026-09-06T10:55:49.069Z", "type": "turn_ended", "outcome": "cancelled", "cancellation_category": "mid_turn_abort", "cancellation_context": {"tool": "bash"}})]);
        assert_eq!(
            fx.outcome(&anchor),
            Some(TurnOutcome::Interrupted {
                reason: "cancelled: mid_turn_abort".into()
            })
        );
    }

    #[test]
    fn test_error_turn_fails_with_the_engine_label() {
        let fx = idle_after_turn_zero();
        let cursor = fx.cursor();
        fx.events(&[turn_started(1)]);
        fx.history(&[prompt(1, MARKER)]);
        let anchor = fx.bind(&cursor);

        fx.events(&[turn_ended("error")]);
        assert_eq!(
            fx.outcome(&anchor),
            Some(TurnOutcome::Failed {
                reason: "error".into()
            })
        );
    }

    #[test]
    fn test_marker_folded_into_an_open_turn_is_ambiguous() {
        let fx = idle_after_turn_zero();
        fx.events(&[turn_started(1)]);
        fx.history(&[prompt(1, "keep going")]);
        let cursor = fx.cursor();
        fx.history(&[json!({"type": "user", "content": format!("<user_query>\n{MARKER}\n</user_query>"), "synthetic_reason": "interjection"})]);

        match fx.find(&cursor) {
            InputBinding::Ambiguous(reason) => assert!(reason.contains("interjection"), "{reason}"),
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn test_interjection_into_the_bound_turn_is_ambiguous() {
        let fx = idle_after_turn_zero();
        let cursor = fx.cursor();
        fx.events(&[turn_started(1)]);
        fx.history(&[prompt(1, MARKER)]);
        let anchor = fx.bind(&cursor);

        fx.events(&[json!({"ts": "2026-09-06T10:55:30.000Z", "type": "interjected", "source": "direct", "image_count": 0, "redirect_kind": "interjection"})]);
        match fx.outcome(&anchor) {
            Some(TurnOutcome::Ambiguous { reason }) => {
                assert!(reason.contains("folded"), "{reason}")
            }
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn test_turn_ended_before_assistant_flush_is_still_running() {
        let fx = idle_after_turn_zero();
        let cursor = fx.cursor();
        fx.events(&[turn_started(1)]);
        fx.history(&[
            prompt(1, MARKER),
            json!({"type": "reasoning", "content": null}),
        ]);
        let anchor = fx.bind(&cursor);

        fx.events(&[turn_ended("completed")]);
        assert_eq!(fx.outcome(&anchor), None);

        fx.history(&[assistant(json!("late"))]);
        assert_eq!(
            fx.outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: "late".into()
            })
        );
    }

    #[test]
    fn test_next_turn_without_an_assistant_record_closes_empty() {
        let fx = idle_after_turn_zero();
        let cursor = fx.cursor();
        fx.events(&[turn_started(1)]);
        fx.history(&[prompt(1, MARKER)]);
        let anchor = fx.bind(&cursor);

        fx.events(&[turn_ended("completed"), turn_started(2)]);
        fx.history(&[prompt(2, "next"), assistant(json!("not yours"))]);
        assert_eq!(
            fx.outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: String::new()
            })
        );
    }

    #[test]
    fn test_session_directory_gone_is_session_changed() {
        let fx = idle_after_turn_zero();
        let cursor = fx.cursor();
        fx.events(&[turn_started(1)]);
        fx.history(&[prompt(1, MARKER)]);
        let anchor = fx.bind(&cursor);

        fs::remove_dir_all(&fx.dir).unwrap();
        assert!(matches!(
            fx.outcome(&anchor),
            Some(TurnOutcome::SessionChanged { .. })
        ));
        assert!(matches!(
            GrokTurnReader.find_input(SESSION, Some(CWD), MARKER, &cursor),
            Err(ReadError::Unavailable(_))
        ));
    }

    #[test]
    fn test_turn_started_for_another_session_is_session_changed() {
        let fx = idle_after_turn_zero();
        let cursor = fx.cursor();
        fx.events(&[turn_started(1)]);
        fx.history(&[prompt(1, MARKER)]);
        let anchor = fx.bind(&cursor);

        fx.events(&[turn_started_in("other-session", 0)]);
        assert!(matches!(
            fx.outcome(&anchor),
            Some(TurnOutcome::SessionChanged { .. })
        ));
    }

    #[test]
    fn test_second_turn_started_before_turn_ended_is_ambiguous() {
        let fx = idle_after_turn_zero();
        let cursor = fx.cursor();
        fx.events(&[turn_started(1)]);
        fx.history(&[prompt(1, MARKER)]);
        let anchor = fx.bind(&cursor);

        fx.events(&[turn_started(2)]);
        assert_eq!(
            fx.outcome(&anchor),
            Some(TurnOutcome::Ambiguous {
                reason: "turn 2 started before turn 1 ended".into()
            })
        );
    }

    #[test]
    fn test_truncated_trailing_lines_are_not_yet() {
        let fx = idle_after_turn_zero();
        let cursor = fx.cursor();
        fx.events(&[turn_started(1)]);
        let full = prompt(1, MARKER).to_string();
        fx.append(HISTORY_NAME, &full[..full.len() - 5]);
        assert_eq!(fx.find(&cursor), InputBinding::NotYet);

        fx.append(HISTORY_NAME, &full[full.len() - 5..]);
        fx.append(HISTORY_NAME, "\n");
        let anchor = fx.bind(&cursor);

        let end = turn_ended("completed").to_string();
        fx.append(EVENTS_NAME, &end[..end.len() - 3]);
        assert_eq!(fx.outcome(&anchor), None);

        fx.append(EVENTS_NAME, &end[end.len() - 3..]);
        fx.append(EVENTS_NAME, "\n");
        fx.history(&[assistant(json!("done"))]);
        assert_eq!(
            fx.outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: "done".into()
            })
        );
    }

    #[test]
    fn test_cursor_stops_before_a_partial_trailing_line() {
        let fx = idle_after_turn_zero();
        let before = fx.cursor();
        let full = prompt(1, MARKER).to_string();
        fx.append(HISTORY_NAME, &full[..10]);
        assert_eq!(fx.cursor(), before);
        fx.append(HISTORY_NAME, &full[10..]);
        fx.append(HISTORY_NAME, "\n");
        fx.events(&[turn_started(1)]);
        assert!(matches!(fx.find(&before), InputBinding::Bound(_)));
    }

    #[test]
    fn test_only_the_turn_carrying_the_marker_binds() {
        let fx = idle_after_turn_zero();
        let cursor = fx.cursor();
        fx.events(&[turn_started(1)]);
        fx.history(&[prompt(1, "human first"), assistant(json!("human reply"))]);
        fx.events(&[turn_ended("completed")]);
        assert_eq!(fx.find(&cursor), InputBinding::NotYet);

        fx.events(&[turn_started(2)]);
        fx.history(&[prompt(2, MARKER), assistant(json!("node reply"))]);
        fx.events(&[turn_ended("completed")]);
        let anchor = fx.bind(&cursor);
        assert_eq!(anchor.turn, format!("{SESSION}/2"));
        assert_eq!(
            fx.outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: "node reply".into()
            })
        );
    }

    #[test]
    fn test_marker_quoted_by_an_assistant_record_does_not_bind() {
        let fx = idle_after_turn_zero();
        let cursor = fx.cursor();
        fx.events(&[turn_started(1)]);
        fx.history(&[
            prompt(1, "what was the id?"),
            assistant(json!(format!("it was {MARKER}"))),
        ]);
        fx.events(&[turn_ended("completed")]);
        assert_eq!(fx.find(&cursor), InputBinding::NotYet);
    }

    #[test]
    fn test_prompt_record_before_its_turn_started_is_not_yet() {
        let fx = idle_after_turn_zero();
        let cursor = fx.cursor();
        fx.history(&[prompt(1, MARKER)]);
        assert_eq!(fx.find(&cursor), InputBinding::NotYet);
        fx.events(&[turn_started(1)]);
        assert!(matches!(fx.find(&cursor), InputBinding::Bound(_)));
    }

    #[test]
    fn test_history_rewritten_under_the_anchor_is_ambiguous() {
        let fx = idle_after_turn_zero();
        let cursor = fx.cursor();
        fx.events(&[turn_started(1)]);
        fx.history(&[prompt(1, MARKER)]);
        let anchor = fx.bind(&cursor);

        let history = fx.dir.join(HISTORY_NAME);
        let text = fs::read_to_string(&history).unwrap();
        let kept: String = text.lines().take(2).map(|l| l.to_string() + "\n").collect();
        fs::write(&history, kept).unwrap();
        fx.events(&[turn_ended("completed")]);
        assert!(matches!(
            fx.outcome(&anchor),
            Some(TurnOutcome::Ambiguous { .. })
        ));
    }

    #[test]
    fn test_complete_garbage_line_is_schema_drift() {
        let fx = idle_after_turn_zero();
        let cursor = fx.cursor();
        fx.append(HISTORY_NAME, "not json\n");
        assert!(matches!(
            GrokTurnReader.find_input(SESSION, Some(CWD), MARKER, &cursor),
            Err(ReadError::UnsupportedSchema(_))
        ));
    }

    #[test]
    fn test_unknown_session_is_unavailable() {
        let _fx = Fixture::new();
        assert!(matches!(
            GrokTurnReader.cursor("nope", Some(CWD)),
            Err(ReadError::Unavailable(_))
        ));
    }

    #[test]
    fn test_offsets_round_trip_and_reject_garbage() {
        let bound = Offsets {
            events: 12,
            history: 34,
            input: Some(5),
        };
        let decoded = Offsets::decode(&bound.encode()).unwrap();
        assert_eq!(
            (decoded.events, decoded.history, decoded.input),
            (12, 34, Some(5))
        );
        let free = Offsets::decode("e=1;h=2").unwrap();
        assert_eq!(free.input, None);
        assert!(Offsets::decode("e=1").is_err());
        assert!(Offsets::decode("h=x;e=1").is_err());
        assert!(Offsets::decode("").is_err());
    }
}
