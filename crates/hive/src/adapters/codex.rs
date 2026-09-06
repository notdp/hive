//! Codex session adapter.
//!
//! Codex stores every session as a JSONL file under
//! `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<timestamp>-<session_id>.jsonl`.
//! Unlike claude, the on-disk layout is partitioned by *date* rather
//! than by cwd, so `find_session_file(session_id, cwd=...)` ignores the cwd hint
//! and walks the sessions tree.
//!
//! The first line of each file is `{"type": "session_meta", "payload": {...}}`
//! carrying the session id, cwd, model provider and base instructions. Subsequent
//! lines are `response_item` records whose `payload` mirrors the OpenAI
//! Responses API shape; we currently normalize `message` items and surface
//! `reasoning`, tool calls (`function_call` / `custom_tool_call`) and their
//! outputs (`function_call_output` / `custom_tool_call_output`) as
//! best-effort parts, everything else degrades to `kind="unknown"` with the
//! raw payload preserved.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::base::{
    parse_iso_timestamp, safe_json_loads, str_or_none, DateTime, Message, MessagePart,
    SessionAdapter, SessionMeta,
};

pub struct CodexAdapter;

impl CodexAdapter {
    fn sessions_root(&self) -> PathBuf {
        crate::adapters::codex_app_server::codex_home().join("sessions")
    }
}

impl SessionAdapter for CodexAdapter {
    fn name(&self) -> &'static str {
        "codex"
    }

    // --- discovery ---

    fn resolve_current_session_id(&self, pane_id: &str) -> Option<String> {
        // A codex session is owned by its app-server daemon: the pane's
        // thread record is the whole answer. An embedded codex (no daemon
        // socket) is deliberately unsupported: hive rejects it at team entry,
        // so with no daemon to ask there is no session to report.
        crate::adapters::codex_app_server::session_id_for_pane(pane_id)
    }

    fn find_session_file(&self, session_id: &str, cwd: Option<&str>) -> Option<PathBuf> {
        let _ = cwd; // date-partitioned layout: the cwd hint is ignored
        if session_id.is_empty() {
            return None;
        }
        let root = self.sessions_root();
        if !root.is_dir() {
            return None;
        }
        let suffix = format!("-{session_id}.jsonl");
        let mut matches: Vec<PathBuf> = Vec::new();
        walk_files(&root, &mut |path| {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if is_rollout_name(name) && name.ends_with(&suffix) {
                    matches.push(path.to_path_buf());
                }
            }
        });
        matches.into_iter().next()
    }

    // --- reading ---

    fn read_meta(&self, path: &Path) -> Option<SessionMeta> {
        let file = fs::File::open(path).ok()?;
        let mut reader = BufReader::new(file);
        let mut first_line = String::new();
        if reader.read_line(&mut first_line).is_err() {
            return None;
        }
        let payload = safe_json_loads(first_line.trim())?;
        if payload.get("type").and_then(Value::as_str) != Some("session_meta") {
            return None;
        }
        let Some(Value::Object(body)) = payload.get("payload") else {
            return None;
        };
        let mut model: Option<String> = None;
        let mut line = String::new();
        for _ in 0..20 {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => return None,
            }
            let Some(extra) = safe_json_loads(line.trim()) else {
                continue;
            };
            if extra.get("type").and_then(Value::as_str) == Some("turn_context") {
                if let Some(Value::Object(payload)) = extra.get("payload") {
                    model = str_or_none(payload.get("model"));
                    if model.is_some() {
                        break;
                    }
                }
            }
        }
        let session_id = str_or_none(body.get("id"))?;
        Some(SessionMeta {
            session_id,
            cwd: str_or_none(body.get("cwd")),
            model: model.or_else(|| str_or_none(body.get("model"))),
        })
    }

    fn iter_messages(&self, path: &Path) -> Box<dyn Iterator<Item = Message>> {
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(_) => return Box::new(std::iter::empty()),
        };
        let mut lines = BufReader::new(file).lines();
        let mut current_turn_id: Option<String> = None;
        Box::new(std::iter::from_fn(move || loop {
            let line = match lines.next()? {
                Ok(line) => line,
                Err(_) => return None,
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some(payload) = safe_json_loads(line) else {
                continue;
            };
            let item_kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
            if item_kind == "event_msg" || item_kind == "turn_context" {
                if let Some(Value::Object(body)) = payload.get("payload") {
                    if let Some(Value::String(turn_id)) = body.get("turn_id") {
                        if !turn_id.is_empty() {
                            current_turn_id = Some(turn_id.clone());
                        }
                    }
                }
                continue;
            }
            if item_kind != "response_item" {
                continue;
            }
            let Some(Value::Object(body)) = payload.get("payload") else {
                continue;
            };
            let timestamp = parse_iso_timestamp(payload.get("timestamp"));
            let mut raw_payload = payload.clone();
            if let Some(turn_id) = &current_turn_id {
                raw_payload.insert("turn_id".to_string(), Value::String(turn_id.clone()));
            }
            return Some(codex_message_from_body(body, timestamp, raw_payload));
        }))
    }
}

fn codex_message_from_body(
    body: &Map<String, Value>,
    timestamp: Option<DateTime>,
    raw: Map<String, Value>,
) -> Message {
    let item_type = body.get("type").and_then(Value::as_str).unwrap_or("");
    match item_type {
        "message" => Message {
            message_id: None,
            parent_id: None,
            role: str_or_none(body.get("role")).unwrap_or_else(|| "unknown".to_string()),
            parts: iter_codex_message_parts(body.get("content")),
            timestamp,
            raw,
        },
        "reasoning" => Message {
            message_id: None,
            parent_id: None,
            role: "assistant".to_string(),
            parts: vec![MessagePart {
                kind: "thinking".to_string(),
                text: extract_reasoning_text(body),
                raw: Some(Value::Object(body.clone())),
                ..Default::default()
            }],
            timestamp,
            raw,
        },
        "function_call" | "custom_tool_call" => {
            let tool_input: Option<Map<String, Value>> = match body.get("arguments") {
                Some(Value::Object(args)) => Some(args.clone()),
                Some(Value::String(args)) => safe_json_loads(args),
                _ => None,
            };
            Message {
                message_id: str_or_none(body.get("call_id")),
                parent_id: None,
                role: "assistant".to_string(),
                parts: vec![MessagePart {
                    kind: "tool_use".to_string(),
                    tool_name: str_or_none(body.get("name")),
                    tool_input,
                    raw: Some(Value::Object(body.clone())),
                    ..Default::default()
                }],
                timestamp,
                raw,
            }
        }
        "function_call_output" | "custom_tool_call_output" => {
            let output_text = match body.get("output") {
                Some(Value::Object(output)) => {
                    str_or_none(first_truthy(output.get("content"), output.get("text")))
                }
                other => str_or_none(other),
            };
            Message {
                message_id: str_or_none(body.get("call_id")),
                parent_id: None,
                role: "tool".to_string(),
                parts: vec![MessagePart {
                    kind: "tool_result".to_string(),
                    tool_output: output_text,
                    raw: Some(Value::Object(body.clone())),
                    ..Default::default()
                }],
                timestamp,
                raw,
            }
        }
        _ => Message {
            message_id: None,
            parent_id: None,
            role: "unknown".to_string(),
            parts: vec![MessagePart {
                kind: "unknown".to_string(),
                raw: Some(Value::Object(body.clone())),
                ..Default::default()
            }],
            timestamp,
            raw,
        },
    }
}

fn iter_codex_message_parts(content: Option<&Value>) -> Vec<MessagePart> {
    let mut parts: Vec<MessagePart> = Vec::new();
    match content {
        Some(Value::String(text)) => {
            parts.push(MessagePart {
                kind: "text".to_string(),
                text: Some(text.clone()),
                ..Default::default()
            });
        }
        Some(Value::Array(blocks)) => {
            for block in blocks {
                let Value::Object(map) = block else {
                    continue;
                };
                let kind = map.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "input_text" | "output_text" | "text" => parts.push(MessagePart {
                        kind: "text".to_string(),
                        text: Some(str_or_none(map.get("text")).unwrap_or_default()),
                        raw: Some(block.clone()),
                        ..Default::default()
                    }),
                    "image" | "input_image" => parts.push(MessagePart {
                        kind: "image".to_string(),
                        raw: Some(block.clone()),
                        ..Default::default()
                    }),
                    "tool_use" => parts.push(MessagePart {
                        kind: "tool_use".to_string(),
                        tool_name: str_or_none(map.get("name")),
                        tool_input: match map.get("input") {
                            Some(Value::Object(input)) => Some(input.clone()),
                            _ => None,
                        },
                        raw: Some(block.clone()),
                        ..Default::default()
                    }),
                    "tool_result" => parts.push(MessagePart {
                        kind: "tool_result".to_string(),
                        tool_output: str_or_none(first_truthy(map.get("content"), map.get("text"))),
                        raw: Some(block.clone()),
                        ..Default::default()
                    }),
                    _ => parts.push(MessagePart {
                        kind: "unknown".to_string(),
                        raw: Some(block.clone()),
                        ..Default::default()
                    }),
                }
            }
        }
        _ => {}
    }
    parts
}

fn extract_reasoning_text(body: &Map<String, Value>) -> Option<String> {
    if let Some(Value::Array(summary)) = body.get("summary") {
        let chunks: Vec<&str> = summary
            .iter()
            .filter_map(|s| {
                s.as_object()
                    .map(|m| m.get("text").and_then(Value::as_str).unwrap_or(""))
            })
            .filter(|c| !c.is_empty())
            .collect();
        let joined = chunks.join("\n");
        if !joined.is_empty() {
            return Some(joined);
        }
    }
    match body.get("text") {
        Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

/// `a` when it is present and truthy (non-null, not `false`, `0`, or empty),
/// else `b`. Callers read a tool result's `content` and fall back to its
/// `text` when `content` is absent or empty.
fn first_truthy<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    match a {
        Some(value) if crate::pyval::truthy(Some(value)) => a,
        _ => b,
    }
}

/// fnmatch `rollout-*.jsonl`.
fn is_rollout_name(name: &str) -> bool {
    name.len() >= "rollout-.jsonl".len() && name.starts_with("rollout-") && name.ends_with(".jsonl")
}

/// Recursive file walk standing in for `Path.rglob`.
fn walk_files(dir: &Path, visit: &mut dyn FnMut(&Path)) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            walk_files(&path, visit);
        } else {
            visit(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;
    use serde_json::json;

    fn write_jsonl(path: &Path, lines: &[Value]) {
        let text: String = lines.iter().map(|l| l.to_string() + "\n").collect();
        fs::write(path, text).unwrap();
    }

    // --- iter_messages ---

    #[test]
    fn test_codex_iter_messages_normalizes_message_reasoning_function_call() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("codex.jsonl");
        write_jsonl(
            &path,
            &[
                json!({"type": "session_meta", "payload": {"id": "s", "cwd": "/w"}}),
                json!({
                    "type": "response_item",
                    "timestamp": "2026-04-02T05:27:52.478Z",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "hi"}],
                    },
                }),
                json!({
                    "type": "response_item",
                    "timestamp": "2026-04-02T05:27:53.000Z",
                    "payload": {
                        "type": "reasoning",
                        "summary": [{"type": "summary_text", "text": "plan"}],
                    },
                }),
                json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "done"}],
                    },
                }),
                json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "shell",
                        "call_id": "call-1",
                        "arguments": json!({"cmd": "ls"}).to_string(),
                    },
                }),
                json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call-1",
                        "output": "a\nb",
                    },
                }),
            ],
        );

        let messages: Vec<Message> = CodexAdapter.iter_messages(&path).collect();
        let roles: Vec<&str> = messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(
            roles,
            ["user", "assistant", "assistant", "assistant", "tool"]
        );

        assert_eq!(messages[0].parts[0].kind, "text");
        assert_eq!(messages[0].parts[0].text.as_deref(), Some("hi"));

        assert_eq!(messages[1].parts[0].kind, "thinking");
        assert_eq!(messages[1].parts[0].text.as_deref(), Some("plan"));

        assert_eq!(messages[2].parts[0].text.as_deref(), Some("done"));

        assert_eq!(messages[3].parts[0].kind, "tool_use");
        assert_eq!(messages[3].parts[0].tool_name.as_deref(), Some("shell"));
        let expected_input = match json!({"cmd": "ls"}) {
            Value::Object(map) => map,
            _ => unreachable!(),
        };
        assert_eq!(messages[3].parts[0].tool_input, Some(expected_input));
        assert_eq!(messages[3].message_id.as_deref(), Some("call-1"));

        assert_eq!(messages[4].parts[0].kind, "tool_result");
        assert_eq!(messages[4].parts[0].tool_output.as_deref(), Some("a\nb"));
        assert_eq!(messages[4].message_id.as_deref(), Some("call-1"));
    }

    #[test]
    fn test_codex_iter_messages_unknown_item_becomes_unknown_part() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("codex.jsonl");
        write_jsonl(
            &path,
            &[
                json!({"type": "session_meta", "payload": {"id": "s", "cwd": "/w"}}),
                json!({
                    "type": "response_item",
                    "payload": {"type": "some_future_item", "detail": 42},
                }),
            ],
        );
        let messages: Vec<Message> = CodexAdapter.iter_messages(&path).collect();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "unknown");
        assert_eq!(messages[0].parts[0].kind, "unknown");
        assert_eq!(
            messages[0].parts[0].raw,
            Some(json!({"type": "some_future_item", "detail": 42}))
        );
    }

    #[test]
    fn test_codex_iter_messages_normalizes_custom_tool_calls() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("codex-custom.jsonl");
        write_jsonl(
            &path,
            &[
                json!({"type": "session_meta", "payload": {"id": "s", "cwd": "/w"}}),
                json!({
                    "type": "response_item",
                    "payload": {
                        "type": "custom_tool_call",
                        "name": "apply_patch",
                        "call_id": "call-2",
                        "arguments": {"patch": "..."},
                    },
                }),
                json!({
                    "type": "response_item",
                    "payload": {
                        "type": "custom_tool_call_output",
                        "call_id": "call-2",
                        "output": {"text": "done"},
                    },
                }),
            ],
        );

        let messages: Vec<Message> = CodexAdapter.iter_messages(&path).collect();
        let roles: Vec<&str> = messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["assistant", "tool"]);
        assert_eq!(messages[0].parts[0].kind, "tool_use");
        assert_eq!(
            messages[0].parts[0].tool_name.as_deref(),
            Some("apply_patch")
        );
        let expected_input = match json!({"patch": "..."}) {
            Value::Object(map) => map,
            _ => unreachable!(),
        };
        assert_eq!(messages[0].parts[0].tool_input, Some(expected_input));
        assert_eq!(messages[0].message_id.as_deref(), Some("call-2"));
        assert_eq!(messages[1].parts[0].kind, "tool_result");
        assert_eq!(messages[1].parts[0].tool_output.as_deref(), Some("done"));
        assert_eq!(messages[1].message_id.as_deref(), Some("call-2"));
    }

    // --- find_session_file / meta ---

    #[test]
    fn test_codex_find_session_file_ignores_cwd_hint() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("CODEX_HOME", tmp.path());
        let root = tmp
            .path()
            .join("sessions")
            .join("2026")
            .join("04")
            .join("02");
        fs::create_dir_all(&root).unwrap();
        let target =
            root.join("rollout-2026-04-02T00-00-00-019d4864-462c-7d41-bbb1-b00b17cdd0b2.jsonl");
        write_jsonl(
            &target,
            &[json!({
                "type": "session_meta",
                "payload": {"id": "019d4864-462c-7d41-bbb1-b00b17cdd0b2", "cwd": "/any"},
            })],
        );

        let resolved = CodexAdapter
            .find_session_file("019d4864-462c-7d41-bbb1-b00b17cdd0b2", Some("/nowhere"));
        assert_eq!(resolved, Some(target));
    }

    #[test]
    fn test_codex_read_meta_parses_session_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("rollout-2026-04-02T00-00-00-deadbeef-dead-beef-dead-beefdeadbeef.jsonl");
        write_jsonl(
            &path,
            &[
                json!({
                    "timestamp": "2026-04-02T00:00:00.000Z",
                    "type": "session_meta",
                    "payload": {
                        "id": "deadbeef-dead-beef-dead-beefdeadbeef",
                        "cwd": "/work",
                    },
                }),
                json!({
                    "timestamp": "2026-04-02T00:00:01.000Z",
                    "type": "turn_context",
                    "payload": {"model": "gpt-5.4"},
                }),
            ],
        );
        let meta = CodexAdapter.read_meta(&path).expect("meta");
        assert_eq!(meta.session_id, "deadbeef-dead-beef-dead-beefdeadbeef");
        assert_eq!(meta.cwd.as_deref(), Some("/work"));
        assert_eq!(meta.model.as_deref(), Some("gpt-5.4"));
    }

    #[test]
    fn test_codex_read_meta_rejects_non_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        write_jsonl(&path, &[json!({"type": "response_item", "payload": {}})]);
        assert!(CodexAdapter.read_meta(&path).is_none());
    }
}
