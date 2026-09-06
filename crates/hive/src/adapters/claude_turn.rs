//! Claude turn reader for `hive node run` (see `adapters::turn`): binds the
//! dispatch marker to the `user` record that carried it into the session
//! JSONL and follows that turn's `parentUuid` chain to its end.
//!
//! Shapes below were read off real transcripts on the developing machine
//! (Claude Code 2.1.219 – 2.1.260). What the file says about a turn:
//!
//! - An idle delivery writes `queue-operation enqueue`, `dequeue`, then a
//!   `user` record (`promptSource: queued`) carrying the text: that record
//!   is the anchor. A mid-turn arrival writes `enqueue`, an `attachment` of
//!   type `queued_command`, and a terminal `queue-operation remove`
//!   (`reason: absorbed_mid_turn` from 2.1.246, no reason before) and no
//!   `user` record at all: the marker is folded into someone else's turn,
//!   and the node cannot own it.
//! - One API message is written as one record per content block
//!   (`apiBlockIndex`), every block record carrying the whole message's
//!   `stop_reason` and `message.id`; a thinking block with
//!   `stop_reason: end_turn` lands before the text block that follows it.
//!   The final message is therefore complete only once a record that is
//!   not one of its blocks has landed after it.
//! - A human interrupt is a `user` record whose text is
//!   `[Request interrupted by user]` (Escape) or
//!   `[Request interrupted by user for tool use]` (a rejected tool call,
//!   after a `tool_result` marked `toolDenialKind: user-rejected`), a
//!   minority also carrying `interruptedMessageId`. Its `parentUuid` may
//!   name an assistant record claude never finished writing.
//! - An API failure that exhausts retries is an `assistant` record with
//!   `isApiErrorMessage: true`, `error: <label>` and the text
//!   `API Error: …`; the `system api_error` records before it are retries.
//!   A safeguard refusal that falls back to another model writes a
//!   `system model_refusal_fallback` (`direction: retry`) and then the
//!   retried message, whose `parentUuid` names a record that was never
//!   written and whose `supersedesUuids` names another one. Without a
//!   fallback it writes `system model_refusal_no_fallback` and the turn is
//!   over.
//! - Compaction writes a `system compact_boundary` with `parentUuid: null`
//!   (`logicalParentUuid` points back) and a `user` record with
//!   `isCompactSummary: true`.
//!
//! Because claude repairs its own chain through records it never wrote,
//! the reader does not require an unbroken `parentUuid` path: a chained
//! record after the anchor whose parent is neither in the turn nor in any
//! record since the anchor belongs to the one turn the session is running,
//! which is the bound one. A fresh input in that position is the one case
//! where it is not (a rewind), and it is reported ambiguous.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::base::{safe_json_loads, SessionAdapter};
use super::claude::ClaudeAdapter;
use super::turn::{Cursor, InputBinding, ReadError, TurnAnchor, TurnOutcome, TurnReader};

#[derive(Default)]
pub struct ClaudeTurnReader;

/// One parsed transcript line.
type Record = Map<String, Value>;

impl TurnReader for ClaudeTurnReader {
    fn cursor(&self, session_id: &str, cwd: Option<&str>) -> Result<Cursor, ReadError> {
        let path = transcript(session_id, cwd)?;
        let (identity, len) = snapshot(&path)?;
        Ok(encode_cursor(&identity, len))
    }

    fn find_input(
        &self,
        session_id: &str,
        cwd: Option<&str>,
        marker: &str,
        cursor: &Cursor,
    ) -> Result<InputBinding, ReadError> {
        let path = transcript(session_id, cwd)?;
        let (identity, len) = snapshot(&path)?;
        // A replaced or truncated file voids the offset; the marker is
        // unique, so the whole file is a safe window.
        let start = match decode_cursor(cursor) {
            Some((id, offset)) if id == identity && offset <= len => offset,
            _ => 0,
        };
        for (offset, record) in read_records(&path, start)? {
            match record_type(&record) {
                "user" => {
                    let Some(text) = user_input_text(&record) else {
                        continue;
                    };
                    if !text.contains(marker) {
                        continue;
                    }
                    let (Some(uuid), Some(session)) = (uuid_of(&record), session_of(&record))
                    else {
                        return Err(ReadError::UnsupportedSchema(
                            "user record without uuid or sessionId".into(),
                        ));
                    };
                    return Ok(InputBinding::Bound(TurnAnchor {
                        session: session.to_string(),
                        turn: uuid.to_string(),
                        cursor: encode_cursor(&identity, offset),
                    }));
                }
                "attachment" => {
                    let Some(attachment) = record.get("attachment").and_then(Value::as_object)
                    else {
                        continue;
                    };
                    if attachment.get("type").and_then(Value::as_str) == Some("queued_command")
                        && content_text(attachment.get("prompt")).contains(marker)
                    {
                        return Ok(InputBinding::Ambiguous(
                            "queued_command attachment: folded into the running turn".into(),
                        ));
                    }
                }
                "queue-operation" => {
                    // `enqueue` precedes the idle delivery's own `user`
                    // record by milliseconds; only the terminal `remove`
                    // says the text was absorbed.
                    if record.get("operation").and_then(Value::as_str) == Some("remove")
                        && content_text(record.get("content")).contains(marker)
                    {
                        let reason = record
                            .get("reason")
                            .and_then(Value::as_str)
                            .map(|r| format!(" ({r})"))
                            .unwrap_or_default();
                        return Ok(InputBinding::Ambiguous(format!(
                            "queue-operation remove{reason}: absorbed into the running turn"
                        )));
                    }
                }
                _ => {}
            }
        }
        Ok(InputBinding::NotYet)
    }

    fn outcome(
        &self,
        anchor: &TurnAnchor,
        cwd: Option<&str>,
    ) -> Result<Option<TurnOutcome>, ReadError> {
        let path = transcript(&anchor.session, cwd)?;
        let (identity, len) = snapshot(&path)?;
        let Some((anchor_identity, offset)) = decode_cursor(&anchor.cursor) else {
            return Err(ReadError::UnsupportedSchema(format!(
                "anchor cursor {:?} is not this reader's",
                anchor.cursor
            )));
        };
        if anchor_identity != identity {
            return Ok(Some(TurnOutcome::SessionChanged {
                reason: "transcript file replaced".into(),
            }));
        }
        if len < offset {
            return Ok(Some(TurnOutcome::SessionChanged {
                reason: "transcript truncated below the anchor".into(),
            }));
        }
        let mut records = read_records(&path, offset)?.into_iter();
        match records.next() {
            Some((_, first)) if uuid_of(&first) == Some(anchor.turn.as_str()) => {}
            _ => {
                return Ok(Some(TurnOutcome::SessionChanged {
                    reason: "the record at the anchor offset is not the anchor".into(),
                }))
            }
        }
        Ok(walk_turn(anchor, records.map(|(_, record)| record)))
    }
}

/// The chain walk after the anchor record: `None` while the turn runs.
fn walk_turn(anchor: &TurnAnchor, records: impl Iterator<Item = Record>) -> Option<TurnOutcome> {
    let mut turn: HashSet<String> = HashSet::from([anchor.turn.clone()]);
    let mut seen: HashSet<String> = turn.clone();
    let mut final_message: Option<FinalMessage> = None;
    for record in records {
        if let Some(session) = session_of(&record) {
            if session != anchor.session {
                return Some(TurnOutcome::SessionChanged {
                    reason: format!("record from session {session}"),
                });
            }
        }
        let kind = record_type(&record);
        let uuid = uuid_of(&record).map(str::to_string);
        if let Some(message) = &mut final_message {
            if kind == "assistant" && message_id(&record) == message.id {
                message.texts.extend(text_blocks(&record));
                continue;
            }
            if uuid.is_some() || closes_message(&record) {
                return Some(TurnOutcome::Completed {
                    text: message.texts.join("\n"),
                });
            }
            continue;
        }
        let parent = record.get("parentUuid").and_then(Value::as_str);
        let in_turn = parent.is_some_and(|p| turn.contains(p));
        // ponytail: a chained record whose parent nobody since the anchor
        // wrote is adopted as the running turn's (claude's own repairs
        // after a refusal fallback or an interrupt look exactly like this).
        // A rewind to a pre-anchor record looks the same and is not
        // covered; its fresh input is what reports it.
        let orphan = uuid.is_some() && !parent.is_some_and(|p| seen.contains(p));
        if let Some(uuid) = &uuid {
            seen.insert(uuid.clone());
        }
        let ours = in_turn || orphan;
        match kind {
            "user" => {
                if let Some(reason) = interrupt_reason(&record) {
                    return Some(TurnOutcome::Interrupted { reason });
                }
                if flag(&record, "isCompactSummary") {
                    return Some(TurnOutcome::Ambiguous {
                        reason: "compaction summary replaced the turn's context".into(),
                    });
                }
                if user_input_text(&record).is_none()
                    || flag(&record, "isMeta")
                    || flag(&record, "turnCompanion")
                {
                    // A tool result or a harness companion: the turn goes on.
                    if ours {
                        turn.extend(uuid);
                    }
                    continue;
                }
                return Some(TurnOutcome::Ambiguous {
                    reason: if in_turn {
                        "second input merged into the turn".into()
                    } else {
                        "a new input started outside the turn".into()
                    },
                });
            }
            "assistant" => {
                if !ours {
                    continue;
                }
                turn.extend(uuid);
                if flag(&record, "isApiErrorMessage") {
                    let label = record
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("api_error");
                    return Some(TurnOutcome::Failed {
                        reason: format!("{label}: {}", text_blocks(&record).join("\n")),
                    });
                }
                if flag(&record, "isAbortedMidStream") {
                    continue;
                }
                let message = record.get("message").and_then(Value::as_object);
                match message
                    .and_then(|m| m.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    Some("end_turn") => {
                        final_message = Some(FinalMessage {
                            id: message_id(&record),
                            texts: text_blocks(&record),
                        });
                    }
                    None | Some("tool_use") => {}
                    Some("max_tokens") => {
                        return Some(TurnOutcome::Failed {
                            reason: "max_tokens".into(),
                        })
                    }
                    Some("refusal") => {
                        let why = message
                            .and_then(|m| m.get("stop_details"))
                            .and_then(|d| d.get("explanation"))
                            .and_then(Value::as_str)
                            .unwrap_or("stop_reason refusal");
                        return Some(TurnOutcome::Failed {
                            reason: format!("refusal: {why}"),
                        });
                    }
                    Some(other) => {
                        return Some(TurnOutcome::Ambiguous {
                            reason: format!("assistant stop_reason {other}"),
                        })
                    }
                }
            }
            "system" => match record.get("subtype").and_then(Value::as_str) {
                Some("compact_boundary") => {
                    let trigger = record
                        .get("compactMetadata")
                        .and_then(|m| m.get("trigger"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    return Some(TurnOutcome::Ambiguous {
                        reason: format!("compact_boundary (trigger={trigger}) inside the turn"),
                    });
                }
                Some("model_refusal_no_fallback") if ours => {
                    let why = record
                        .get("apiRefusalExplanation")
                        .and_then(Value::as_str)
                        .unwrap_or("safeguards refused the request");
                    return Some(TurnOutcome::Failed {
                        reason: format!("model_refusal_no_fallback: {why}"),
                    });
                }
                _ => {
                    if ours {
                        turn.extend(uuid);
                    }
                }
            },
            _ => {
                // attachment rows chain; queue rows, last-prompt,
                // cost-state and the other harness rows do not.
                if ours {
                    turn.extend(uuid);
                }
            }
        }
    }
    None
}

struct FinalMessage {
    id: Option<String>,
    texts: Vec<String>,
}

/// Records without a uuid that claude writes only once a turn has closed:
/// `last-prompt` and the dequeue that opens the next queued turn. (An
/// `enqueue` is a human typing and can interleave with a streaming message.)
fn closes_message(record: &Record) -> bool {
    match record_type(record) {
        "last-prompt" => true,
        "queue-operation" => record.get("operation").and_then(Value::as_str) == Some("dequeue"),
        _ => false,
    }
}

fn transcript(session_id: &str, cwd: Option<&str>) -> Result<PathBuf, ReadError> {
    ClaudeAdapter
        .find_session_file(session_id, cwd)
        .ok_or_else(|| {
            ReadError::Unavailable(format!("no transcript for claude session {session_id}"))
        })
}

/// File identity (`dev:ino`) and byte length.
fn snapshot(path: &Path) -> Result<(String, u64), ReadError> {
    let meta = fs::metadata(path)
        .map_err(|e| ReadError::Unavailable(format!("{}: {e}", path.display())))?;
    Ok((format!("{}:{}", meta.dev(), meta.ino()), meta.len()))
}

fn encode_cursor(identity: &str, offset: u64) -> Cursor {
    format!("{identity}:{offset}")
}

fn decode_cursor(cursor: &str) -> Option<(String, u64)> {
    let (identity, offset) = cursor.rsplit_once(':')?;
    Some((identity.to_string(), offset.parse().ok()?))
}

/// Complete records from `offset` on, each with the byte offset it starts
/// at. A trailing line without its newline is still being written and is
/// left out; a complete line that is not a JSON object is skipped.
fn read_records(path: &Path, offset: u64) -> Result<Vec<(u64, Record)>, ReadError> {
    let mut file = fs::File::open(path)
        .map_err(|e| ReadError::Unavailable(format!("{}: {e}", path.display())))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| ReadError::Unavailable(format!("{}: {e}", path.display())))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| ReadError::Unavailable(format!("{}: {e}", path.display())))?;
    let mut records = Vec::new();
    let mut start = 0usize;
    while let Some(len) = bytes[start..].iter().position(|b| *b == b'\n') {
        let line = &bytes[start..start + len];
        if let Some(record) = std::str::from_utf8(line).ok().and_then(safe_json_loads) {
            records.push((offset + start as u64, record));
        }
        start += len + 1;
    }
    Ok(records)
}

fn record_type(record: &Record) -> &str {
    record.get("type").and_then(Value::as_str).unwrap_or("")
}

fn uuid_of(record: &Record) -> Option<&str> {
    record.get("uuid").and_then(Value::as_str)
}

fn session_of(record: &Record) -> Option<&str> {
    record
        .get("sessionId")
        .or_else(|| record.get("session_id"))
        .and_then(Value::as_str)
}

fn flag(record: &Record, key: &str) -> bool {
    record.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn message_id(record: &Record) -> Option<String> {
    record
        .get("message")
        .and_then(|m| m.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The text of a content field that is either a string or a block list;
/// non-text blocks contribute nothing.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The text of a `user` record that is an input (human or injected);
/// `None` for a tool result.
fn user_input_text(record: &Record) -> Option<String> {
    let content = record.get("message").and_then(|m| m.get("content"))?;
    if let Value::Array(blocks) = content {
        if blocks
            .iter()
            .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        {
            return None;
        }
    }
    Some(content_text(Some(content)))
}

/// Every text block of an assistant record, in order.
fn text_blocks(record: &Record) -> Vec<String> {
    match record.get("message").and_then(|m| m.get("content")) {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

const INTERRUPT_PREFIX: &str = "[Request interrupted by user";

/// The engine's own label for a human interrupt record, if this is one.
fn interrupt_reason(record: &Record) -> Option<String> {
    let text = user_input_text(record).unwrap_or_default();
    if text.starts_with(INTERRUPT_PREFIX) {
        return Some(text);
    }
    record
        .get("interruptedMessageId")
        .and_then(Value::as_str)
        .map(|id| format!("interrupted (message {id})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::{EnvGuard, CLAUDE_VARS};
    use serde_json::json;

    const SESSION: &str = "b50e8587-aec4-4b85-8bdd-db6d040d75eb";
    const CWD: &str = "/Users/dev/proj";
    const MARKER: &str = "nd-0123456789ab";

    /// A temp HOME whose `.claude/projects/<slug>/` holds the fixture; the
    /// adapter resolves the tree through HOME once the claude knobs are
    /// unset.
    struct Fixture {
        _env: EnvGuard,
        _tmp: tempfile::TempDir,
        path: PathBuf,
    }

    fn fixture() -> Fixture {
        let mut env = EnvGuard::cleared(&CLAUDE_VARS);
        let tmp = tempfile::tempdir().unwrap();
        env.set("HOME", tmp.path());
        let dir = tmp.path().join(".claude/projects/-Users-dev-proj");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{SESSION}.jsonl"));
        fs::write(&path, "").unwrap();
        Fixture {
            _env: env,
            _tmp: tmp,
            path,
        }
    }

    fn append(path: &Path, rows: &[Value]) {
        let text: String = rows.iter().map(|r| r.to_string() + "\n").collect();
        let mut existing = fs::read_to_string(path).unwrap();
        existing.push_str(&text);
        fs::write(path, existing).unwrap();
    }

    fn append_raw(path: &Path, text: &str) {
        let mut existing = fs::read_to_string(path).unwrap();
        existing.push_str(text);
        fs::write(path, existing).unwrap();
    }

    // --- record shapes, modelled on real transcripts (content redacted) ---

    fn input(uuid: &str, parent: Option<&str>, text: &str) -> Value {
        json!({
            "parentUuid": parent,
            "isSidechain": false,
            "type": "user",
            "message": {"role": "user", "content": text},
            "uuid": uuid,
            "timestamp": "2026-09-06T10:55:09.295Z",
            "origin": {"kind": "human"},
            "promptSource": "queued",
            "sessionKind": "bg",
            "cwd": CWD,
            "sessionId": SESSION,
            "version": "2.1.260",
        })
    }

    fn assistant(uuid: &str, parent: &str, msg: &str, stop: &str, block: Value) -> Value {
        json!({
            "parentUuid": parent,
            "isSidechain": false,
            "message": {
                "model": "claude-opus-5",
                "id": msg,
                "type": "message",
                "role": "assistant",
                "content": [block],
                "stop_reason": stop,
                "stop_sequence": null,
                "usage": {"input_tokens": 1, "output_tokens": 2},
            },
            "requestId": "req_x",
            "type": "assistant",
            "uuid": uuid,
            "timestamp": "2026-09-06T10:55:10.946Z",
            "session_id": SESSION,
            "cwd": CWD,
            "sessionId": SESSION,
            "version": "2.1.260",
        })
    }

    fn tool_result(uuid: &str, parent: &str, output: &str) -> Value {
        json!({
            "parentUuid": parent,
            "isSidechain": false,
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"tool_use_id": "toolu_1", "type": "tool_result", "content": output}],
            },
            "uuid": uuid,
            "timestamp": "2026-09-06T10:55:11.000Z",
            "toolUseResult": output,
            "sourceToolAssistantUUID": parent,
            "cwd": CWD,
            "sessionId": SESSION,
            "version": "2.1.260",
        })
    }

    fn attachment(uuid: &str, parent: &str, body: Value) -> Value {
        json!({
            "parentUuid": parent,
            "isSidechain": false,
            "attachment": body,
            "type": "attachment",
            "uuid": uuid,
            "timestamp": "2026-09-06T10:55:09.294Z",
            "cwd": CWD,
            "sessionId": SESSION,
            "version": "2.1.260",
        })
    }

    fn turn_duration(uuid: &str, parent: &str) -> Value {
        json!({
            "parentUuid": parent,
            "isSidechain": false,
            "type": "system",
            "subtype": "turn_duration",
            "durationMs": 3079,
            "messageCount": 21,
            "timestamp": "2026-09-06T10:55:12.375Z",
            "uuid": uuid,
            "isMeta": false,
            "cwd": CWD,
            "sessionId": SESSION,
            "version": "2.1.260",
        })
    }

    fn queue_op(operation: &str, content: Option<&str>, reason: Option<&str>) -> Value {
        let mut row = json!({
            "type": "queue-operation",
            "operation": operation,
            "timestamp": "2026-09-06T10:55:09.283Z",
            "sessionId": SESSION,
        });
        if let Some(content) = content {
            row["content"] = json!(content);
        }
        if let Some(reason) = reason {
            row["reason"] = json!(reason);
        }
        row
    }

    fn text(text: &str) -> Value {
        json!({"type": "text", "text": text})
    }

    fn thinking() -> Value {
        json!({"type": "thinking", "thinking": "…", "signature": "x"})
    }

    fn tool_use() -> Value {
        json!({"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"file_path": "/a"}})
    }

    fn envelope() -> String {
        format!("<HIVE to=hornet.bee artifact=/w/artifacts/tasks/bee-{MARKER}.md>\ntask {MARKER}\ndo the thing\n</HIVE>")
    }

    /// The previous turn, so the anchor has something to chain from and the
    /// cursor sits past real bytes.
    fn prior_turn(path: &Path) {
        append(
            path,
            &[
                input("u0", None, "<command-name>/hive:hive</command-name>"),
                assistant("a0", "u0", "msg_0", "end_turn", text("ready")),
                turn_duration("s0", "a0"),
            ],
        );
    }

    fn bind(cursor: &Cursor) -> TurnAnchor {
        match ClaudeTurnReader
            .find_input(SESSION, Some(CWD), MARKER, cursor)
            .unwrap()
        {
            InputBinding::Bound(anchor) => anchor,
            other => panic!("expected Bound, got {other:?}"),
        }
    }

    fn outcome(anchor: &TurnAnchor) -> Option<TurnOutcome> {
        ClaudeTurnReader.outcome(anchor, Some(CWD)).unwrap()
    }

    // --- cursor / find_input ---

    #[test]
    fn test_cursor_is_file_identity_and_length() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        let (identity, len) = decode_cursor(&cursor).unwrap();
        assert_eq!(len, fs::metadata(&fx.path).unwrap().len());
        assert_eq!(identity, snapshot(&fx.path).unwrap().0);
    }

    #[test]
    fn test_cursor_without_transcript_is_unavailable() {
        let _fx = fixture();
        assert!(matches!(
            ClaudeTurnReader.cursor("no-such-session", Some(CWD)),
            Err(ReadError::Unavailable(_))
        ));
    }

    #[test]
    fn test_find_input_binds_the_user_record_past_the_cursor() {
        let fx = fixture();
        prior_turn(&fx.path);
        // The marker before the cursor is not the dispatch.
        append(&fx.path, &[input("stale", Some("s0"), &envelope())]);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        assert_eq!(
            ClaudeTurnReader
                .find_input(SESSION, Some(CWD), MARKER, &cursor)
                .unwrap(),
            InputBinding::NotYet
        );
        append(
            &fx.path,
            &[
                queue_op("enqueue", Some(&envelope()), None),
                queue_op("dequeue", None, None),
                input("u1", Some("s0"), &envelope()),
            ],
        );
        let anchor = bind(&cursor);
        assert_eq!(anchor.session, SESSION);
        assert_eq!(anchor.turn, "u1");
        let (identity, offset) = decode_cursor(&anchor.cursor).unwrap();
        assert_eq!(identity, snapshot(&fx.path).unwrap().0);
        let (_, record) = read_records(&fx.path, offset).unwrap().remove(0);
        assert_eq!(uuid_of(&record), Some("u1"));
    }

    #[test]
    fn test_find_input_enqueue_alone_is_not_yet() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(&fx.path, &[queue_op("enqueue", Some(&envelope()), None)]);
        assert_eq!(
            ClaudeTurnReader
                .find_input(SESSION, Some(CWD), MARKER, &cursor)
                .unwrap(),
            InputBinding::NotYet
        );
    }

    #[test]
    fn test_find_input_folded_mid_turn_is_ambiguous() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(
            &fx.path,
            &[
                queue_op("enqueue", Some(&envelope()), None),
                attachment(
                    "at1",
                    "s0",
                    json!({
                        "type": "queued_command",
                        "prompt": envelope(),
                        "source_uuid": "q1",
                        "commandMode": "prompt",
                        "origin": {"kind": "human"},
                    }),
                ),
                queue_op("remove", Some(&envelope()), Some("absorbed_mid_turn")),
            ],
        );
        match ClaudeTurnReader
            .find_input(SESSION, Some(CWD), MARKER, &cursor)
            .unwrap()
        {
            InputBinding::Ambiguous(reason) => {
                assert!(reason.contains("queued_command"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn test_find_input_remove_without_attachment_is_ambiguous() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(
            &fx.path,
            &[
                queue_op("enqueue", Some(&envelope()), None),
                queue_op("remove", Some(&envelope()), None),
            ],
        );
        match ClaudeTurnReader
            .find_input(SESSION, Some(CWD), MARKER, &cursor)
            .unwrap()
        {
            InputBinding::Ambiguous(reason) => assert!(reason.contains("remove"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn test_find_input_marker_in_tool_result_is_not_an_input() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(
            &fx.path,
            &[
                input("u1", Some("s0"), "list the task files"),
                assistant("a1", "u1", "msg_1", "tool_use", tool_use()),
                tool_result("t1", "a1", &format!("bee-{MARKER}.md")),
            ],
        );
        assert_eq!(
            ClaudeTurnReader
                .find_input(SESSION, Some(CWD), MARKER, &cursor)
                .unwrap(),
            InputBinding::NotYet
        );
    }

    #[test]
    fn test_find_input_rescans_a_replaced_file_from_the_start() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        fs::remove_file(&fx.path).unwrap();
        fs::write(&fx.path, "").unwrap();
        append(&fx.path, &[input("u1", None, &envelope())]);
        let anchor = bind(&cursor);
        assert_eq!(anchor.turn, "u1");
    }

    #[test]
    fn test_find_input_ignores_a_partial_trailing_line() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        let whole = input("u1", Some("s0"), &envelope()).to_string();
        append_raw(&fx.path, &whole[..whole.len() - 10]);
        assert_eq!(
            ClaudeTurnReader
                .find_input(SESSION, Some(CWD), MARKER, &cursor)
                .unwrap(),
            InputBinding::NotYet
        );
        append_raw(&fx.path, &format!("{}\n", &whole[whole.len() - 10..]));
        assert_eq!(bind(&cursor).turn, "u1");
    }

    // --- outcome ---

    /// The probe-6406 shape: the final message is a thinking block record
    /// and a text block record sharing one message id, then turn_duration.
    #[test]
    fn test_outcome_completed_with_tool_rounds_and_multi_block_final() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(
            &fx.path,
            &[
                input("u1", Some("s0"), &envelope()),
                attachment(
                    "at1",
                    "u1",
                    json!({"type": "output_style", "style": "Concise"}),
                ),
            ],
        );
        let anchor = bind(&cursor);
        assert_eq!(outcome(&anchor), None);
        append(
            &fx.path,
            &[
                assistant("a1", "at1", "msg_1", "tool_use", thinking()),
                assistant("a2", "a1", "msg_1", "tool_use", text("looking")),
                assistant("a3", "a2", "msg_1", "tool_use", tool_use()),
                tool_result("t1", "a3", "file body"),
                assistant("a4", "t1", "msg_2", "end_turn", thinking()),
            ],
        );
        // end_turn on the thinking block: the text block is still coming.
        assert_eq!(outcome(&anchor), None);
        append(
            &fx.path,
            &[
                assistant("a5", "a4", "msg_2", "end_turn", text("first paragraph")),
                assistant("a6", "a5", "msg_2", "end_turn", text("second\nparagraph")),
            ],
        );
        assert_eq!(outcome(&anchor), None);
        append(&fx.path, &[turn_duration("s1", "a6")]);
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: "first paragraph\nsecond\nparagraph".into()
            })
        );
    }

    #[test]
    fn test_outcome_completed_closes_on_last_prompt_row() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(&fx.path, &[input("u1", Some("s0"), &envelope())]);
        let anchor = bind(&cursor);
        append(
            &fx.path,
            &[
                assistant("a1", "u1", "msg_1", "end_turn", text("done")),
                json!({"type": "cost-state", "sessionId": SESSION, "totalCostUSD": 0.1}),
            ],
        );
        assert_eq!(outcome(&anchor), None);
        append(
            &fx.path,
            &[
                json!({"type": "last-prompt", "lastPrompt": envelope(), "leafUuid": "a1", "sessionId": SESSION}),
            ],
        );
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: "done".into()
            })
        );
    }

    #[test]
    fn test_outcome_completed_with_no_text_block_is_empty() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(&fx.path, &[input("u1", Some("s0"), &envelope())]);
        let anchor = bind(&cursor);
        append(
            &fx.path,
            &[
                assistant("a1", "u1", "msg_1", "end_turn", thinking()),
                turn_duration("s1", "a1"),
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
    fn test_outcome_second_input_on_the_chain_is_ambiguous() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(&fx.path, &[input("u1", Some("s0"), &envelope())]);
        let anchor = bind(&cursor);
        append(
            &fx.path,
            &[
                assistant("a1", "u1", "msg_1", "tool_use", tool_use()),
                tool_result("t1", "a1", "ok"),
                input("u2", Some("t1"), "actually do something else"),
            ],
        );
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Ambiguous {
                reason: "second input merged into the turn".into()
            })
        );
    }

    #[test]
    fn test_outcome_meta_companion_rows_do_not_end_the_turn() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(&fx.path, &[input("u1", Some("s0"), &envelope())]);
        let anchor = bind(&cursor);
        let mut companion = input("u2", Some("u1"), "Base directory for this skill: /x");
        companion["isMeta"] = json!(true);
        companion["turnCompanion"] = json!(true);
        append(
            &fx.path,
            &[
                companion,
                assistant("a1", "u2", "msg_1", "end_turn", text("done")),
                turn_duration("s1", "a1"),
            ],
        );
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: "done".into()
            })
        );
    }

    #[test]
    fn test_outcome_max_tokens_is_failed() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(&fx.path, &[input("u1", Some("s0"), &envelope())]);
        let anchor = bind(&cursor);
        append(
            &fx.path,
            &[assistant("a1", "u1", "msg_1", "max_tokens", text("long…"))],
        );
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Failed {
                reason: "max_tokens".into()
            })
        );
    }

    /// Real shape: `isApiErrorMessage: true`, `error: server_error`,
    /// stop_reason stop_sequence, text `API Error: …`.
    #[test]
    fn test_outcome_api_error_record_is_failed() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(&fx.path, &[input("u1", Some("s0"), &envelope())]);
        let anchor = bind(&cursor);
        let mut retry = turn_duration("s1", "u1");
        retry["subtype"] = json!("api_error");
        retry["source"] = json!("request_retry");
        retry["retryAttempt"] = json!(1);
        let mut error = assistant(
            "a1",
            "s1",
            "5d94f3a1",
            "stop_sequence",
            text("API Error: 529 Overloaded"),
        );
        error["error"] = json!("server_error");
        error["isApiErrorMessage"] = json!(true);
        append(&fx.path, &[retry.clone()]);
        assert_eq!(outcome(&anchor), None);
        append(&fx.path, &[error]);
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Failed {
                reason: "server_error: API Error: 529 Overloaded".into()
            })
        );
    }

    #[test]
    fn test_outcome_refusal_stop_reason_is_failed() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(&fx.path, &[input("u1", Some("s0"), &envelope())]);
        let anchor = bind(&cursor);
        let mut refused = assistant("a1", "u1", "msg_1", "refusal", text(""));
        refused["message"]["stop_details"] =
            json!({"type": "refusal", "category": "cyber", "explanation": "blocked"});
        append(&fx.path, &[refused]);
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Failed {
                reason: "refusal: blocked".into()
            })
        );
    }

    /// Real shape: the retried message's parent was never written; the
    /// turn still ends with its end_turn.
    #[test]
    fn test_outcome_refusal_fallback_retry_repairs_the_chain() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(&fx.path, &[input("u1", Some("s0"), &envelope())]);
        let anchor = bind(&cursor);
        let mut fallback = turn_duration("s1", "u1");
        fallback["subtype"] = json!("model_refusal_fallback");
        fallback["direction"] = json!("retry");
        fallback["fallbackModel"] = json!("claude-opus-4-8");
        let mut retried = assistant("a2", "never-written", "msg_2", "end_turn", text("done"));
        retried["supersedesUuids"] = json!(["a1"]);
        append(&fx.path, &[fallback, retried, turn_duration("s2", "a2")]);
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: "done".into()
            })
        );
    }

    #[test]
    fn test_outcome_refusal_without_fallback_is_failed() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(&fx.path, &[input("u1", Some("s0"), &envelope())]);
        let anchor = bind(&cursor);
        let mut refusal = turn_duration("s1", "u1");
        refusal["subtype"] = json!("model_refusal_no_fallback");
        refusal["apiRefusalExplanation"] = json!("blocked");
        append(&fx.path, &[refusal]);
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Failed {
                reason: "model_refusal_no_fallback: blocked".into()
            })
        );
    }

    /// Escape: a `user` record with the text `[Request interrupted by
    /// user]`, chained to a record claude never wrote.
    #[test]
    fn test_outcome_escape_interrupt_with_unwritten_parent() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(&fx.path, &[input("u1", Some("s0"), &envelope())]);
        let anchor = bind(&cursor);
        let mut aborted = assistant("a1", "u1", "msg_1", "tool_use", thinking());
        aborted["isAbortedMidStream"] = json!(true);
        let interrupt = json!({
            "parentUuid": "never-written",
            "isSidechain": false,
            "type": "user",
            "message": {"role": "user", "content": [text("[Request interrupted by user]")]},
            "uuid": "i1",
            "timestamp": "2026-09-06T10:55:20.000Z",
            "interruptedMessageId": "msg_1",
            "sessionId": SESSION,
            "cwd": CWD,
        });
        append(&fx.path, &[aborted, interrupt]);
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Interrupted {
                reason: "[Request interrupted by user]".into()
            })
        );
    }

    /// A rejected tool call: a user-rejected tool_result then the
    /// `[Request interrupted by user for tool use]` record.
    #[test]
    fn test_outcome_tool_use_interrupt() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(&fx.path, &[input("u1", Some("s0"), &envelope())]);
        let anchor = bind(&cursor);
        let mut rejected = tool_result(
            "t1",
            "a1",
            "The user doesn't want to proceed with this tool use.",
        );
        rejected["toolDenialKind"] = json!("user-rejected");
        let mut interrupt = input("i1", Some("t1"), "");
        interrupt["message"]["content"] =
            json!([text("[Request interrupted by user for tool use]")]);
        append(
            &fx.path,
            &[
                assistant("a1", "u1", "msg_1", "tool_use", tool_use()),
                rejected,
                interrupt,
            ],
        );
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Interrupted {
                reason: "[Request interrupted by user for tool use]".into()
            })
        );
    }

    #[test]
    fn test_outcome_compaction_inside_the_turn_is_ambiguous() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(&fx.path, &[input("u1", Some("s0"), &envelope())]);
        let anchor = bind(&cursor);
        append(
            &fx.path,
            &[
                assistant("a1", "u1", "msg_1", "tool_use", tool_use()),
                tool_result("t1", "a1", "ok"),
                json!({
                    "parentUuid": null,
                    "logicalParentUuid": "t1",
                    "isSidechain": false,
                    "type": "system",
                    "subtype": "compact_boundary",
                    "content": "Conversation compacted",
                    "uuid": "c1",
                    "compactMetadata": {"trigger": "auto", "preTokens": 200000},
                    "sessionId": SESSION,
                }),
            ],
        );
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Ambiguous {
                reason: "compact_boundary (trigger=auto) inside the turn".into()
            })
        );
    }

    #[test]
    fn test_outcome_session_change_on_foreign_record() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(&fx.path, &[input("u1", Some("s0"), &envelope())]);
        let anchor = bind(&cursor);
        let mut foreign = assistant("a1", "u1", "msg_1", "end_turn", text("done"));
        foreign["sessionId"] = json!("other-session");
        foreign["session_id"] = json!("other-session");
        append(&fx.path, &[foreign]);
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::SessionChanged {
                reason: "record from session other-session".into()
            })
        );
    }

    #[test]
    fn test_outcome_session_change_when_file_is_replaced_or_truncated() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(&fx.path, &[input("u1", Some("s0"), &envelope())]);
        let anchor = bind(&cursor);
        // Truncated below the anchor: same inode, shorter.
        fs::write(&fx.path, "").unwrap();
        prior_turn(&fx.path);
        assert!(matches!(
            outcome(&anchor),
            Some(TurnOutcome::SessionChanged { .. })
        ));
        // Same length, a different record at the anchor offset.
        append(&fx.path, &[input("u9", Some("s0"), &envelope())]);
        assert!(matches!(
            outcome(&anchor),
            Some(TurnOutcome::SessionChanged { .. })
        ));
        // A new inode altogether.
        let tmp = fx.path.with_extension("tmp");
        fs::copy(&fx.path, &tmp).unwrap();
        fs::rename(&tmp, &fx.path).unwrap();
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::SessionChanged {
                reason: "transcript file replaced".into()
            })
        );
    }

    #[test]
    fn test_outcome_partial_trailing_line_is_not_yet() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(&fx.path, &[input("u1", Some("s0"), &envelope())]);
        let anchor = bind(&cursor);
        let whole = assistant("a1", "u1", "msg_1", "end_turn", text("done")).to_string();
        append_raw(&fx.path, &whole[..whole.len() - 5]);
        assert_eq!(outcome(&anchor), None);
        append_raw(&fx.path, &format!("{}\n", &whole[whole.len() - 5..]));
        assert_eq!(outcome(&anchor), None);
        let closer = turn_duration("s1", "a1").to_string();
        append_raw(&fx.path, &closer[..closer.len() - 5]);
        assert_eq!(outcome(&anchor), None);
        append_raw(&fx.path, &format!("{}\n", &closer[closer.len() - 5..]));
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Completed {
                text: "done".into()
            })
        );
    }

    #[test]
    fn test_outcome_unknown_stop_reason_is_ambiguous() {
        let fx = fixture();
        prior_turn(&fx.path);
        let cursor = ClaudeTurnReader.cursor(SESSION, Some(CWD)).unwrap();
        append(&fx.path, &[input("u1", Some("s0"), &envelope())]);
        let anchor = bind(&cursor);
        append(
            &fx.path,
            &[assistant("a1", "u1", "msg_1", "pause_turn", text("…"))],
        );
        assert_eq!(
            outcome(&anchor),
            Some(TurnOutcome::Ambiguous {
                reason: "assistant stop_reason pause_turn".into()
            })
        );
    }

    #[test]
    fn test_outcome_foreign_cursor_is_unsupported() {
        let _fx = fixture();
        let anchor = TurnAnchor {
            session: SESSION.into(),
            turn: "u1".into(),
            cursor: "not-ours".into(),
        };
        assert!(matches!(
            ClaudeTurnReader.outcome(&anchor, Some(CWD)),
            Err(ReadError::UnsupportedSchema(_))
        ));
    }
}
