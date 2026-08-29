//! Grok session adapter.
//!
//! Grok stores every session as a directory under
//! `$GROK_HOME/sessions/<urllib.parse.quote(cwd, safe="")>/<session_id>/`. The
//! conversation lives in `chat_history.jsonl` — one record per line typed
//! `system` / `user` / `assistant` / `reasoning` / `tool_result` — and a
//! sibling `summary.json` carries title/model/timestamp once grok writes it.
//!
//! Unlike claude and codex the records carry no session id, cwd or uuid: the path
//! is the metadata, so `read_meta` reads the two enclosing directory names and
//! only falls back to the file for the model.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::base::{
    parse_iso_timestamp, safe_json_loads, safe_mtime, str_or_none, DateTime, Message, MessagePart,
    SessionAdapter, SessionMeta,
};

const _HISTORY_NAME: &str = "chat_history.jsonl";
const _META_SCAN_LIMIT: usize = 20;

fn _role_by_type(record_type: &str) -> Option<&'static str> {
    match record_type {
        "user" => Some("user"),
        "assistant" => Some("assistant"),
        "system" => Some("system"),
        _ => None,
    }
}

pub struct GrokAdapter;

impl GrokAdapter {
    fn _sessions_root(&self) -> PathBuf {
        crate::adapters::grok_leader::grok_home().join("sessions")
    }
}

impl SessionAdapter for GrokAdapter {
    fn name(&self) -> &'static str {
        "grok"
    }

    // --- discovery ---

    fn resolve_current_session_id(&self, pane_id: &str) -> Option<String> {
        // A grok session is owned by its per-pane leader daemon, which records
        // the minted session id in the pane session file.
        crate::adapters::grok_leader::session_id_for_pane(pane_id)
    }

    fn find_session_file(&self, session_id: &str, cwd: Option<&str>) -> Option<PathBuf> {
        if session_id.is_empty() {
            return None;
        }
        let root = self._sessions_root();
        if !root.is_dir() {
            return None;
        }
        if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
            let direct = root.join(_quote(cwd)).join(session_id).join(_HISTORY_NAME);
            if direct.exists() {
                return Some(direct);
            }
        }
        let mut matches: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = fs::read_dir(&root) {
            for entry in entries.flatten() {
                let candidate = entry.path().join(session_id).join(_HISTORY_NAME);
                if candidate.exists() {
                    matches.push(candidate);
                }
            }
        }
        matches.sort();
        matches.into_iter().next()
    }

    fn list_sessions(&self, cwd: Option<&str>, limit: Option<usize>) -> Vec<SessionMeta> {
        let root = self._sessions_root();
        if !root.is_dir() {
            return Vec::new();
        }
        let mut files: Vec<(f64, PathBuf)> = Vec::new();
        if let Ok(level1) = fs::read_dir(&root) {
            for cwd_dir in level1.flatten() {
                if let Ok(level2) = fs::read_dir(cwd_dir.path()) {
                    for session_dir in level2.flatten() {
                        let candidate = session_dir.path().join(_HISTORY_NAME);
                        if candidate.exists() {
                            files.push((safe_mtime(&candidate), candidate));
                        }
                    }
                }
            }
        }
        files.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut out: Vec<SessionMeta> = Vec::new();
        for (_, path) in files {
            let Some(meta) = self.read_meta(&path) else {
                continue;
            };
            if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
                if meta.cwd.as_deref() != Some(cwd) {
                    continue;
                }
            }
            out.push(meta);
            if let Some(limit) = limit {
                if out.len() >= limit {
                    break;
                }
            }
        }
        out
    }

    // --- reading ---

    fn read_meta(&self, path: &Path) -> Option<SessionMeta> {
        if path.file_name().and_then(|n| n.to_str()) != Some(_HISTORY_NAME) {
            return None;
        }
        let parent = path.parent()?;
        let session_id = parent
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if session_id.is_empty() {
            return None;
        }
        let summary: Map<String, Value> = fs::read_to_string(parent.join("summary.json"))
            .ok()
            .and_then(|text| safe_json_loads(&text))
            .unwrap_or_default();
        let mut started_at = parse_iso_timestamp(summary.get("timestamp"));
        if started_at.is_none() {
            let mtime = safe_mtime(path);
            if mtime >= 0.0 {
                started_at = Some(DateTime::from_timestamp_utc(mtime));
            }
        }
        let cwd_dir = parent
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("");
        Some(SessionMeta {
            session_id,
            cli_name: self.name().to_string(),
            cwd: Some(_unquote(cwd_dir)),
            title: str_or_none(summary.get("title")),
            started_at,
            jsonl_path: path.to_path_buf(),
            model: str_or_none(summary.get("model")).or_else(|| _first_assistant_model(path)),
        })
    }

    fn iter_messages(&self, path: &Path) -> Box<dyn Iterator<Item = Message>> {
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(_) => return Box::new(std::iter::empty()),
        };
        let mut lines = BufReader::new(file).lines();
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
            if let Some(message) = _message_from_record(&payload) {
                return Some(message);
            }
        }))
    }

    fn message_from_record(&self, payload: &Map<String, Value>) -> Option<Message> {
        _message_from_record(payload)
    }
}

fn _message_from_record(payload: &Map<String, Value>) -> Option<Message> {
    let record_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let content = payload.get("content");
    if let Some(role) = _role_by_type(record_type) {
        return Some(_message(role, _iter_grok_parts(content), payload));
    }
    if record_type == "reasoning" {
        let text = _text_of(content).or_else(|| str_or_none(payload.get("text")));
        return Some(_message(
            "assistant",
            vec![MessagePart {
                kind: "thinking".to_string(),
                text,
                raw: Some(Value::Object(payload.clone())),
                ..Default::default()
            }],
            payload,
        ));
    }
    if record_type == "tool_result" {
        return Some(_message(
            "tool",
            vec![MessagePart {
                kind: "tool_result".to_string(),
                tool_name: str_or_none(payload.get("tool_name")),
                tool_output: _text_of(content),
                raw: Some(Value::Object(payload.clone())),
                ..Default::default()
            }],
            payload,
        ));
    }
    None
}

fn _message(role: &str, parts: Vec<MessagePart>, payload: &Map<String, Value>) -> Message {
    Message {
        message_id: None,
        parent_id: None,
        role: role.to_string(),
        parts,
        timestamp: parse_iso_timestamp(payload.get("ts")),
        raw: payload.clone(),
    }
}

fn _iter_grok_parts(content: Option<&Value>) -> Vec<MessagePart> {
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
                if map.get("type").and_then(Value::as_str) == Some("text") {
                    parts.push(MessagePart {
                        kind: "text".to_string(),
                        text: Some(str_or_none(map.get("text")).unwrap_or_default()),
                        raw: Some(block.clone()),
                        ..Default::default()
                    });
                } else {
                    parts.push(MessagePart {
                        kind: "unknown".to_string(),
                        raw: Some(block.clone()),
                        ..Default::default()
                    });
                }
            }
        }
        _ => {}
    }
    parts
}

fn _text_of(content: Option<&Value>) -> Option<String> {
    match content {
        Some(Value::String(text)) => {
            if text.is_empty() {
                None
            } else {
                Some(text.clone())
            }
        }
        Some(Value::Array(blocks)) => {
            let chunks: Vec<String> = blocks
                .iter()
                .filter_map(|block| {
                    let map = block.as_object()?;
                    if map.get("type").and_then(Value::as_str) == Some("text") {
                        Some(str_or_none(map.get("text")).unwrap_or_default())
                    } else {
                        None
                    }
                })
                .filter(|chunk| !chunk.is_empty())
                .collect();
            let joined = chunks.join("\n");
            if joined.is_empty() {
                None
            } else {
                Some(joined)
            }
        }
        _ => None,
    }
}

fn _first_assistant_model(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    for _ in 0.._META_SCAN_LIMIT {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => return None,
        }
        let Some(payload) = safe_json_loads(line.trim()) else {
            continue;
        };
        if payload.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let model = str_or_none(payload.get("model_id"));
        if model.is_some() {
            return model;
        }
    }
    None
}

/// `urllib.parse.quote(value, safe="")`: percent-encode every byte outside
/// the unreserved set.
fn _quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `urllib.parse.unquote`: decode %XX sequences, leaving malformed ones as-is.
fn _unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::ffi::OsStrExt;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const CWD: &str = "/Users/dp/work/hive";
    const OTHER_CWD: &str = "/tmp/other";

    fn _write_session(home: &Path, session_id: &str, cwd: &str, records: &[Value]) -> PathBuf {
        let session_dir = home.join("sessions").join(_quote(cwd)).join(session_id);
        fs::create_dir_all(&session_dir).unwrap();
        let history = session_dir.join(_HISTORY_NAME);
        let text: String = records.iter().map(|r| r.to_string() + "\n").collect();
        fs::write(&history, text).unwrap();
        history
    }

    fn _assistant(text: &str, model_id: &str) -> Value {
        json!({"type": "assistant", "content": text, "model_id": model_id})
    }

    fn _default_assistant() -> Value {
        _assistant("ok", "grok-4.6-build")
    }

    fn _set_mtime(path: &Path, secs: i64) {
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let tv = libc::timeval {
            tv_sec: secs as libc::time_t,
            tv_usec: 0,
        };
        let times = [tv, tv];
        let rc = unsafe { libc::utimes(c_path.as_ptr(), times.as_ptr()) };
        assert_eq!(rc, 0);
    }

    // --- discovery -----------------------------------------------------------

    #[test]
    fn test_find_session_file_uses_quoted_cwd_directory() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(".grok");
        std::env::set_var("GROK_HOME", &home);
        let target = _write_session(&home, "sess-a", CWD, &[_default_assistant()]);
        _write_session(&home, "sess-b", OTHER_CWD, &[_default_assistant()]);

        assert_eq!(
            GrokAdapter.find_session_file("sess-a", Some(CWD)),
            Some(target)
        );
    }

    #[test]
    fn test_find_session_file_globs_when_cwd_is_unknown_or_wrong() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(".grok");
        std::env::set_var("GROK_HOME", &home);
        let target = _write_session(&home, "sess-a", CWD, &[_default_assistant()]);

        assert_eq!(
            GrokAdapter.find_session_file("sess-a", None),
            Some(target.clone())
        );
        assert_eq!(
            GrokAdapter.find_session_file("sess-a", Some("/nowhere")),
            Some(target)
        );
    }

    #[test]
    fn test_find_session_file_returns_none_when_missing() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(".grok");
        std::env::set_var("GROK_HOME", &home);
        _write_session(&home, "sess-a", CWD, &[_default_assistant()]);

        assert_eq!(GrokAdapter.find_session_file("sess-missing", None), None);
        assert_eq!(GrokAdapter.find_session_file("", None), None);
    }

    #[test]
    fn test_list_sessions_orders_by_mtime_and_filters_by_cwd() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(".grok");
        std::env::set_var("GROK_HOME", &home);
        let old = _write_session(&home, "sess-old", CWD, &[_default_assistant()]);
        let new = _write_session(&home, "sess-new", OTHER_CWD, &[_default_assistant()]);
        _set_mtime(&old, 1_700_000_000);
        _set_mtime(&new, 1_700_000_500);

        let adapter = GrokAdapter;
        let ids: Vec<String> = adapter
            .list_sessions(None, None)
            .into_iter()
            .map(|m| m.session_id)
            .collect();
        assert_eq!(ids, ["sess-new", "sess-old"]);
        let cwds: Vec<Option<String>> = adapter
            .list_sessions(Some(CWD), None)
            .into_iter()
            .map(|m| m.cwd)
            .collect();
        assert_eq!(cwds, [Some(CWD.to_string())]);
        let limited: Vec<String> = adapter
            .list_sessions(None, Some(1))
            .into_iter()
            .map(|m| m.session_id)
            .collect();
        assert_eq!(limited, ["sess-new"]);
        assert!(adapter
            .list_sessions(None, None)
            .iter()
            .all(|m| m.cli_name == "grok"));
    }

    // --- meta ----------------------------------------------------------------

    #[test]
    fn test_read_meta_prefers_summary_json() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(".grok");
        let history = _write_session(&home, "sess-a", CWD, &[_assistant("ok", "grok-4.6-build")]);
        fs::write(
            history.parent().unwrap().join("summary.json"),
            json!({
                "title": "nonce hunt",
                "model": "grok-4.6",
                "timestamp": "2026-08-23T18:12:34.567640+00:00",
            })
            .to_string(),
        )
        .unwrap();

        let meta = GrokAdapter.read_meta(&history).expect("meta");
        assert_eq!(meta.session_id, "sess-a");
        assert_eq!(meta.cwd.as_deref(), Some(CWD));
        assert_eq!(meta.model.as_deref(), Some("grok-4.6"));
        assert_eq!(meta.title.as_deref(), Some("nonce hunt"));
        let started_at = meta.started_at.expect("started_at");
        assert_eq!(started_at.year, 2026);
        assert_eq!(meta.jsonl_path, history);
    }

    #[test]
    fn test_read_meta_falls_back_to_mtime_and_first_assistant_model() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(".grok");
        let history = _write_session(
            &home,
            "sess-b",
            CWD,
            &[
                json!({"type": "system", "content": "You are Grok 4.6."}),
                json!({"type": "user", "content": [{"type": "text", "text": "hi"}]}),
                _assistant("ok", "grok-4.6-build"),
            ],
        );
        _set_mtime(&history, 1_700_000_000);

        let meta = GrokAdapter.read_meta(&history).expect("meta");
        assert_eq!(meta.model.as_deref(), Some("grok-4.6-build"));
        assert!(meta.title.is_none());
        let started_at = meta.started_at.expect("started_at");
        assert_eq!(started_at.timestamp(), 1_700_000_000.0);
    }

    #[test]
    fn test_read_meta_rejects_other_files() {
        let tmp = tempfile::tempdir().unwrap();
        let stray = tmp.path().join("rollout.jsonl");
        fs::write(
            &stray,
            json!({"type": "assistant", "content": "hi"}).to_string() + "\n",
        )
        .unwrap();
        assert!(GrokAdapter.read_meta(&stray).is_none());
    }

    // --- messages ------------------------------------------------------------

    #[test]
    fn test_iter_messages_maps_every_record_type() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(".grok");
        let history = _write_session(
            &home,
            "sess-c",
            CWD,
            &[
                json!({"type": "system", "content": "You are Grok 4.6."}),
                json!({"type": "user", "content": [{"type": "text", "text": "<user_query>\nhi\n</user_query>"}]}),
                json!({"type": "reasoning", "content": [{"type": "text", "text": "thinking hard"}]}),
                json!({"type": "tool_result", "tool_name": "read_file", "content": [{"type": "text", "text": "file body"}]}),
                _assistant("NONCE-7q3x", "grok-4.6-build"),
                json!({"type": "rewind_marker", "content": "ignored"}),
            ],
        );

        let messages: Vec<Message> = GrokAdapter.iter_messages(&history).collect();
        let roles: Vec<&str> = messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "assistant"]);
        let kinds: Vec<&str> = messages.iter().map(|m| m.parts[0].kind.as_str()).collect();
        assert_eq!(kinds, ["text", "text", "thinking", "tool_result", "text"]);
        assert_eq!(
            messages[1].parts[0].text.as_deref(),
            Some("<user_query>\nhi\n</user_query>")
        );
        assert_eq!(messages[2].parts[0].text.as_deref(), Some("thinking hard"));
        assert_eq!(messages[3].parts[0].tool_name.as_deref(), Some("read_file"));
        assert_eq!(
            messages[3].parts[0].tool_output.as_deref(),
            Some("file body")
        );
        assert_eq!(messages[4].parts[0].text.as_deref(), Some("NONCE-7q3x"));
        assert_eq!(
            messages[4].raw.get("model_id"),
            Some(&json!("grok-4.6-build"))
        );
    }

    #[test]
    fn test_message_from_record_handles_list_assistant_content_and_unknowns() {
        let adapter = GrokAdapter;
        let payload = match json!({
            "type": "assistant",
            "content": [{"type": "text", "text": "a"}, {"type": "image", "url": "x"}],
        }) {
            Value::Object(map) => map,
            _ => unreachable!(),
        };
        let listed = adapter.message_from_record(&payload).expect("message");
        assert_eq!(listed.role, "assistant");
        let kinds: Vec<&str> = listed.parts.iter().map(|p| p.kind.as_str()).collect();
        assert_eq!(kinds, ["text", "unknown"]);

        let rewind = match json!({"type": "rewind_marker"}) {
            Value::Object(map) => map,
            _ => unreachable!(),
        };
        assert!(adapter.message_from_record(&rewind).is_none());
        assert!(adapter.message_from_record(&Map::new()).is_none());
    }

    #[test]
    fn test_iter_messages_missing_file_yields_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let messages: Vec<Message> = GrokAdapter
            .iter_messages(&tmp.path().join("nope.jsonl"))
            .collect();
        assert!(messages.is_empty());
    }
}
