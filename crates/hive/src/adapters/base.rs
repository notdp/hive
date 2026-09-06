//! Base types and trait for agent CLI session adapters.
//!
//! Adapters normalize the CLIs (claude/codex/grok) around a single interface
//! so callers can discover, locate, and read session JSONL files without
//! knowing the per-CLI on-disk layout.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

// --- timestamps -------------------------------------------------------------

/// A parsed ISO-8601 timestamp: civil fields plus the UTC offset it carried
/// (`None` when it carried none).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DateTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub microsecond: u32,
    pub utc_offset_secs: Option<i32>,
}

impl DateTime {
    /// Seconds since the Unix epoch.
    // ponytail: an offset-less timestamp is treated as UTC (the general case
    // would apply the local zone);
    // hive transcripts always carry an offset, so the naive branch never fires.
    pub fn timestamp(&self) -> f64 {
        let days = days_from_civil(self.year, self.month, self.day);
        let mut secs =
            days * 86_400 + self.hour as i64 * 3600 + self.minute as i64 * 60 + self.second as i64;
        if let Some(off) = self.utc_offset_secs {
            secs -= off as i64;
        }
        secs as f64 + self.microsecond as f64 / 1_000_000.0
    }
}

fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = (if m <= 2 { y - 1 } else { y }) as i64;
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = ((m as i64) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + (d as i64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// --- core records -----------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct SessionMeta {
    pub session_id: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
}

/// kind: "text" | "tool_use" | "tool_result" | "thinking" | "image" | "unknown"
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MessagePart {
    pub kind: String,
    pub text: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Option<Map<String, Value>>,
    pub tool_output: Option<String>,
    pub raw: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub message_id: Option<String>,
    pub parent_id: Option<String>,
    /// "user" | "assistant" | "system" | "developer" | "tool"
    pub role: String,
    pub parts: Vec<MessagePart>,
    pub timestamp: Option<DateTime>,
    pub raw: Map<String, Value>,
}

pub trait SessionAdapter {
    fn name(&self) -> &'static str;

    /// Return the id of the session currently running in `pane_id`.
    fn resolve_current_session_id(&self, pane_id: &str) -> Option<String>;

    /// Locate the JSONL file backing `session_id`.
    ///
    /// `cwd` is an optional hint; claude stores files under a cwd-slug
    /// directory while codex partitions by date, so the hint speeds up the
    /// former and is ignored by the latter.
    fn find_session_file(&self, session_id: &str, cwd: Option<&str>) -> Option<PathBuf>;

    /// Parse the meta header of a JSONL session file.
    fn read_meta(&self, path: &Path) -> Option<SessionMeta>;

    /// Yield normalized [`Message`] records from a JSONL session file.
    fn iter_messages(&self, path: &Path) -> Box<dyn Iterator<Item = Message>>;
}

// --- shared helpers ---------------------------------------------------------

/// The spawner's env with every inherited identity marker washed, for a
/// daemon that serves tool shells of its own: the spawner may itself run
/// inside another member's engine (an orch's `hive workflow run`), and an
/// inherited CLAUDE_CODE_MESSAGING_SOCKET — or any other CLAUDE*/ANTHROPIC*
/// marker — would make every hive call from those shells resolve to the
/// *spawner*.
/// *drop* names the caller's further markers to wash by exact key.
pub(crate) fn washed_spawner_env(drop: &[&str]) -> HashMap<String, String> {
    std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| {
            !(key.starts_with("CLAUDE")
                || key.starts_with("ANTHROPIC")
                || drop.contains(&key.as_str()))
        })
        .collect()
}

pub fn parse_iso_timestamp(value: Option<&Value>) -> Option<DateTime> {
    let raw = match value {
        Some(Value::String(s)) if !s.is_empty() => s,
        _ => return None,
    };
    let raw = if raw.ends_with('Z') {
        raw.replace('Z', "+00:00")
    } else {
        raw.clone()
    };
    parse_isoformat(&raw)
}

fn parse_isoformat(s: &str) -> Option<DateTime> {
    let bytes = s.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: i32 = parse_digits(s.get(0..4)?)?;
    let month: u32 = parse_digits(s.get(5..7)?)?;
    let day: u32 = parse_digits(s.get(8..10)?)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut dt = DateTime {
        year,
        month,
        day,
        hour: 0,
        minute: 0,
        second: 0,
        microsecond: 0,
        utc_offset_secs: None,
    };
    if bytes.len() == 10 {
        return Some(dt);
    }
    if bytes[10] != b'T' && bytes[10] != b' ' {
        return None;
    }
    let rest = s.get(11..)?;
    let (time_part, offset) = match rest.find(['+', '-']) {
        Some(i) => (&rest[..i], Some(&rest[i..])),
        None => (rest, None),
    };
    let (hms, frac) = match time_part.find('.') {
        Some(i) => (&time_part[..i], Some(&time_part[i + 1..])),
        None => (time_part, None),
    };
    let mut it = hms.split(':');
    dt.hour = parse_digits(it.next()?)?;
    if let Some(minute) = it.next() {
        dt.minute = parse_digits(minute)?;
    }
    if let Some(second) = it.next() {
        dt.second = parse_digits(second)?;
    }
    if it.next().is_some() || dt.hour > 23 || dt.minute > 59 || dt.second > 59 {
        return None;
    }
    if let Some(frac) = frac {
        if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut digits: String = frac.chars().take(6).collect();
        while digits.len() < 6 {
            digits.push('0');
        }
        dt.microsecond = digits.parse().ok()?;
    }
    if let Some(offset) = offset {
        let sign: i32 = if offset.starts_with('-') { -1 } else { 1 };
        let body = &offset[1..];
        let fields: Vec<&str> = body.split(':').collect();
        let (oh, om, os): (i32, i32, i32) = match fields.len() {
            1 => match fields[0].len() {
                2 => (parse_digits(fields[0])?, 0, 0),
                4 => (
                    parse_digits(fields[0].get(0..2)?)?,
                    parse_digits(fields[0].get(2..4)?)?,
                    0,
                ),
                _ => return None,
            },
            2 => (parse_digits(fields[0])?, parse_digits(fields[1])?, 0),
            3 => (
                parse_digits(fields[0])?,
                parse_digits(fields[1])?,
                parse_digits(fields[2].split('.').next()?)?,
            ),
            _ => return None,
        };
        dt.utc_offset_secs = Some(sign * (oh * 3600 + om * 60 + os));
    }
    Some(dt)
}

fn parse_digits<T: std::str::FromStr>(s: &str) -> Option<T> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

pub fn safe_json_loads(line: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(line) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// The JSON object stored at *path*; None when the file is unreadable,
/// unparseable, or holds anything but an object.
pub fn read_json_object(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    safe_json_loads(&text)
}

/// Normalize a process command/argv token for CLI matching.
pub fn normalize_command_token(value: &str) -> String {
    let value = value.trim().to_lowercase();
    let last = value.rsplit('/').next().unwrap_or("");
    last.trim_start_matches('-').to_string()
}

/// Coerce a value to str, returning None for empty/None.
pub fn str_or_none(value: Option<&Value>) -> Option<String> {
    let text = match value? {
        Value::Null => return None,
        Value::String(s) => s.clone(),
        Value::Bool(b) => (if *b { "True" } else { "False" }).to_string(),
        Value::Number(n) => n.to_string(),
        // ponytail: containers render as JSON; real payloads only put
        // scalars in these fields.
        other => other.to_string(),
    };
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

// --- Send gate helpers ------------------------------------------------------
// Detect whether the target agent is waiting for a user answer
// (AskUserQuestion) before allowing message injection.

const ASK_TOOL_NAMES: [&str; 2] = ["AskUserQuestion", "request_user_input"];

const MAX_TAIL_BYTES: u64 = 128 * 1024; // 128KB upper bound for tail reads

/// status: "waiting" | "clear" | "unknown"
#[derive(Debug, Clone, PartialEq)]
pub struct GateResult {
    pub status: &'static str,
    pub reason: String,
}

/// Claude's `message.content` block list (codex records never carry one;
/// their `response_item` payload is read directly by the caller).
fn extract_content_blocks(payload: &Map<String, Value>) -> &[Value] {
    if let Some(Value::Object(msg)) = payload.get("message") {
        if let Some(Value::Array(content)) = msg.get("content") {
            return content;
        }
    }
    &[]
}

fn is_ask_tool(name: Option<&Value>) -> bool {
    matches!(name, Some(Value::String(s)) if ASK_TOOL_NAMES.contains(&s.as_str()))
}

/// Check whether a raw JSONL record is an assistant turn with AskUserQuestion.
///
/// Handles both CLI formats:
/// - claude: {"type": "assistant", "message": {"role": "assistant", "content": [...]}}
/// - codex: {"type": "response_item", "payload": {"type": "function_call", "name": ...}}
fn is_assistant_ask(payload: &Map<String, Value>) -> bool {
    let record_type = payload.get("type").and_then(Value::as_str).unwrap_or("");

    // claude: type == "assistant"
    if record_type == "assistant" {
        return extract_content_blocks(payload).iter().any(|block| {
            block.get("type").and_then(Value::as_str) == Some("tool_use")
                && is_ask_tool(block.get("name"))
        });
    }

    // codex: type == "response_item", payload.type == "function_call"
    if record_type == "response_item" {
        if let Some(Value::Object(inner)) = payload.get("payload") {
            return inner.get("type").and_then(Value::as_str) == Some("function_call")
                && is_ask_tool(inner.get("name"));
        }
        return false;
    }

    false
}

/// Check whether a raw JSONL record is a function_call_output (codex tool result).
fn is_function_call_output(payload: &Map<String, Value>) -> bool {
    if payload.get("type").and_then(Value::as_str) == Some("response_item") {
        if let Some(Value::Object(inner)) = payload.get("payload") {
            return inner.get("type").and_then(Value::as_str) == Some("function_call_output");
        }
    }
    false
}

/// Check whether a raw JSONL record represents a user turn.
///
/// Checks both CLI formats; only one will match for any given file.
fn is_user_turn(payload: &Map<String, Value>) -> bool {
    let record_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    // claude: {"type": "user", ...}
    if record_type == "user" {
        return true;
    }
    // codex: {"type": "response_item", "payload": {"type": "message", "role": "user", ...}}
    if record_type == "response_item" {
        if let Some(Value::Object(inner)) = payload.get("payload") {
            return inner.get("type").and_then(Value::as_str) == Some("message")
                && inner.get("role").and_then(Value::as_str) == Some("user");
        }
    }
    false
}

/// Check if the agent owning `path` is waiting for a user answer.
///
/// Reads the tail of the JSONL file, expanding the window if no relevant
/// record is found (8KB → 16KB → ... → 128KB).
///
/// Returns GateResult with status:
///   - "waiting": last relevant record is an unanswered AskUserQuestion
///   - "clear": last relevant record is a user turn (question answered)
///   - "unknown": could not determine (file missing, empty, parse issues)
pub fn check_input_gate(path: &Path) -> GateResult {
    let file_size = match path.metadata() {
        Ok(meta) => meta.len(),
        Err(e) => {
            return GateResult {
                status: "unknown",
                reason: format!("cannot stat file: {e}"),
            }
        }
    };
    if file_size == 0 {
        return GateResult {
            status: "unknown",
            reason: "empty transcript".to_string(),
        };
    }

    let mut chunk: u64 = 8192;
    while chunk <= MAX_TAIL_BYTES {
        let offset = file_size.saturating_sub(chunk);
        let mut raw = Vec::new();
        let read = File::open(path).and_then(|mut f| {
            f.seek(SeekFrom::Start(offset))?;
            f.read_to_end(&mut raw)
        });
        if let Err(e) = read {
            return GateResult {
                status: "unknown",
                reason: format!("read error: {e}"),
            };
        }
        let data = String::from_utf8_lossy(&raw);

        let mut lines: Vec<&str> = data.split('\n').collect();
        // First line may be partial if we seeked mid-line; skip it unless offset == 0
        if offset > 0 && !lines.is_empty() {
            lines.remove(0);
        }

        // Parse every complete line in the window
        let mut records: Vec<Map<String, Value>> = Vec::new();
        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(parsed) = safe_json_loads(line) {
                records.push(parsed);
            }
        }

        // Scan in reverse for the last relevant record
        for record in records.iter().rev() {
            if is_user_turn(record) || is_function_call_output(record) {
                return GateResult {
                    status: "clear",
                    reason: "last record is user response".to_string(),
                };
            }
            if is_assistant_ask(record) {
                return GateResult {
                    status: "waiting",
                    reason: "AskUserQuestion pending".to_string(),
                };
            }
        }

        // No relevant record found — expand window if possible
        if offset == 0 {
            break; // Already read the entire file
        }
        chunk *= 2;
    }

    GateResult {
        status: "unknown",
        reason: "no relevant record found".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => unreachable!("test payloads are objects"),
        }
    }

    fn write_jsonl(path: &Path, records: &[Value]) {
        let mut file = File::create(path).unwrap();
        let text: String = records.iter().map(|r| r.to_string() + "\n").collect();
        file.write_all(text.as_bytes()).unwrap();
    }

    // --- is_assistant_ask: claude format ---

    #[test]
    fn test_is_assistant_ask_claude() {
        let payload = obj(json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Let me ask you something."},
                    {"type": "tool_use", "name": "AskUserQuestion", "input": {"question": "Continue?"}},
                ],
            },
        }));
        assert!(is_assistant_ask(&payload));
    }

    // --- is_assistant_ask: codex format ---

    #[test]
    fn test_is_assistant_ask_codex() {
        let payload = obj(json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "AskUserQuestion",
                "arguments": "{\"question\": \"proceed?\"}",
            },
        }));
        assert!(is_assistant_ask(&payload));
    }

    // --- rejects other tools ---

    #[test]
    fn test_is_assistant_ask_codex_request_user_input() {
        // Codex uses request_user_input instead of AskUserQuestion.
        let payload = obj(json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "request_user_input",
                "arguments": "{\"prompt\": \"choose option\"}",
            },
        }));
        assert!(is_assistant_ask(&payload));
    }

    #[test]
    fn test_rejects_other_tools() {
        let payload = obj(json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "name": "Read", "input": {"file_path": "/tmp/x"}},
                    {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}},
                ],
            },
        }));
        assert!(!is_assistant_ask(&payload));
    }

    #[test]
    fn test_rejects_user_turn() {
        let payload = obj(json!({"type": "user", "message": {"role": "user", "content": "hello"}}));
        assert!(!is_assistant_ask(&payload));
    }

    // --- check_input_gate: end-to-end on JSONL files ---

    #[test]
    fn test_waiting_when_ask_is_last() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        write_jsonl(
            &path,
            &[
                json!({"type": "user", "message": {"role": "user", "content": "do something"}}),
                json!({
                    "type": "assistant",
                    "message": {
                        "role": "assistant",
                        "content": [
                            {"type": "tool_use", "name": "AskUserQuestion", "input": {"question": "sure?"}},
                        ],
                    },
                }),
            ],
        );
        let result = check_input_gate(&path);
        assert_eq!(result.status, "waiting");
    }

    #[test]
    fn test_not_waiting_when_answered() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        write_jsonl(
            &path,
            &[
                json!({
                    "type": "assistant",
                    "message": {
                        "role": "assistant",
                        "content": [
                            {"type": "tool_use", "name": "AskUserQuestion", "input": {"question": "sure?"}},
                        ],
                    },
                }),
                json!({"type": "user", "message": {"role": "user", "content": "yes"}}),
            ],
        );
        let result = check_input_gate(&path);
        assert_eq!(result.status, "clear");
    }

    #[test]
    fn test_clear_after_codex_function_call_output() {
        // Codex answers come as function_call_output, not user messages.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        write_jsonl(
            &path,
            &[
                json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "request_user_input",
                        "call_id": "call_123",
                        "arguments": "{\"prompt\": \"choose\"}",
                    },
                }),
                json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call_123",
                        "output": "option A",
                    },
                }),
            ],
        );
        let result = check_input_gate(&path);
        assert_eq!(result.status, "clear");
    }

    #[test]
    fn test_waiting_codex_request_user_input() {
        // Codex request_user_input without answer should block.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        write_jsonl(
            &path,
            &[json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "request_user_input",
                    "call_id": "call_456",
                    "arguments": "{\"prompt\": \"choose\"}",
                },
            })],
        );
        let result = check_input_gate(&path);
        assert_eq!(result.status, "waiting");
    }

    #[test]
    fn test_clear_when_no_ask() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        write_jsonl(
            &path,
            &[
                json!({"type": "user", "message": {"role": "user", "content": "hello"}}),
                json!({
                    "type": "assistant",
                    "message": {
                        "role": "assistant",
                        "content": [{"type": "text", "text": "hi there"}],
                    },
                }),
                json!({"type": "user", "message": {"role": "user", "content": "thanks"}}),
            ],
        );
        let result = check_input_gate(&path);
        assert_eq!(result.status, "clear");
    }

    #[test]
    fn test_fail_open_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nonexistent.jsonl");
        let result = check_input_gate(&path);
        assert_eq!(result.status, "unknown");
    }

    #[test]
    fn test_fail_open_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("empty.jsonl");
        std::fs::write(&path, "").unwrap();
        let result = check_input_gate(&path);
        assert_eq!(result.status, "unknown");
    }

    #[test]
    fn test_unicode_transcript_does_not_raise() {
        // UTF-8 multibyte characters must not break decoding (fail-open).
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        // Build a transcript >8KB with Chinese text so seek lands mid-character
        let chinese_text = "你好世界".repeat(500); // ~6000 bytes of Chinese
        write_jsonl(
            &path,
            &[
                json!({"type": "user", "message": {"role": "user", "content": chinese_text}}),
                json!({
                    "type": "assistant",
                    "message": {
                        "role": "assistant",
                        "content": [
                            {"type": "tool_use", "name": "AskUserQuestion", "input": {"question": "继续吗？"}},
                        ],
                    },
                }),
            ],
        );
        let result = check_input_gate(&path);
        assert!(matches!(result.status, "waiting" | "unknown")); // must not raise
    }

    #[test]
    fn test_expands_window_for_large_records() {
        // When the last relevant record is larger than initial 8KB chunk,
        // the function should expand the read window to find it.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        // Write a user turn, then a large assistant ask that exceeds 8KB
        let large_text = "x".repeat(12000);
        write_jsonl(
            &path,
            &[
                json!({"type": "user", "message": {"role": "user", "content": "start"}}),
                json!({
                    "type": "assistant",
                    "message": {
                        "role": "assistant",
                        "content": [
                            {"type": "text", "text": large_text},
                            {"type": "tool_use", "name": "AskUserQuestion", "input": {"question": "ok?"}},
                        ],
                    },
                }),
            ],
        );
        let result = check_input_gate(&path);
        assert_eq!(result.status, "waiting");
    }

    // --- cross-CLI parity ---

    #[test]
    fn test_all_adapters_return_messages_with_uniform_shape() {
        // Regardless of CLI, every Message yields parts with .kind and .role in expected set.
        use crate::adapters::claude::ClaudeAdapter;
        use crate::adapters::codex::CodexAdapter;

        let tmp = tempfile::tempdir().unwrap();
        let claude_path = tmp.path().join("claude.jsonl");
        write_jsonl(
            &claude_path,
            &[json!({
                "type": "user",
                "uuid": "u1",
                "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            })],
        );
        let codex_path = tmp.path().join("codex.jsonl");
        write_jsonl(
            &codex_path,
            &[
                json!({"type": "session_meta", "payload": {"id": "s", "cwd": "/w"}}),
                json!({
                    "type": "response_item",
                    "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                }),
            ],
        );

        let allowed_kinds = [
            "text",
            "thinking",
            "tool_use",
            "tool_result",
            "image",
            "unknown",
        ];
        let allowed_roles = [
            "user",
            "assistant",
            "system",
            "developer",
            "tool",
            "unknown",
        ];

        let cases: [(&str, &dyn SessionAdapter, &Path); 2] = [
            ("claude", &ClaudeAdapter, &claude_path),
            ("codex", &CodexAdapter, &codex_path),
        ];
        for (name, adapter, path) in cases {
            let msgs: Vec<Message> = adapter.iter_messages(path).collect();
            assert!(!msgs.is_empty(), "{name} yielded no messages");
            for msg in msgs {
                assert!(allowed_roles.contains(&msg.role.as_str()));
                for part in msg.parts {
                    assert!(allowed_kinds.contains(&part.kind.as_str()));
                }
            }
        }
    }
}
