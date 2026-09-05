//! Claude Code session adapter.
//!
//! Claude stores session history under `<claude-config>/projects/<cwd-slug>/<id>.jsonl`
//! (the tree `claude_sessions::_config_dir` resolves).
//! Every record carries `sessionId`, `cwd`, `parentUuid`, `timestamp` and
//! `gitBranch`; the `message.content` field is an Anthropic-style list of blocks
//! or (rarely) a plain string.
//!
//! Current-session resolution: a hive claude member is a bg job, so its pane's
//! job record answers directly (the live engine entry's sessionId, which follows
//! an in-session `/clear`). An interactive claude on the pane tty — a guest
//! session, a human's own pane — resolves through its `sessions/<pid>.json`
//! registry entry, which claude keeps current itself.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::base::{
    normalize_command_token, parse_iso_timestamp, read_json_object, safe_json_loads, str_or_none,
    Message, MessagePart, SessionAdapter, SessionMeta,
};

fn _claude_home() -> PathBuf {
    // One resolver for every reader of the claude config tree (delivery reads
    // the session registry through the same function).
    crate::adapters::claude_sessions::_config_dir()
}

pub struct ClaudeAdapter;

impl ClaudeAdapter {
    fn _projects_root(&self) -> PathBuf {
        _claude_home().join("projects")
    }
}

impl SessionAdapter for ClaudeAdapter {
    fn name(&self) -> &'static str {
        "claude"
    }

    // --- discovery ---

    fn resolve_current_session_id(&self, pane_id: &str) -> Option<String> {
        // A bg member pane answers from its job record (engine entry first —
        // it follows an in-session /clear — then the record's snapshot for a
        // parked engine).
        if let Some(session_id) = crate::adapters::claude_bg::session_id_for_pane(pane_id) {
            if !session_id.is_empty() {
                return Some(session_id);
            }
        }
        // Interactive claude on the pane tty (guest pane, a human's own
        // session): its registry entry is claude's own current-session truth.
        let sessions_dir = _claude_home().join("sessions");
        let tty = crate::tmux::get_pane_tty(pane_id).unwrap_or_default();
        for process in crate::tmux::list_tty_processes(&tty) {
            if !_is_claude_process(&process.command, &process.argv) {
                continue;
            }
            let payload =
                match read_json_object(&sessions_dir.join(format!("{}.json", process.pid))) {
                    Some(payload) if !payload.is_empty() => payload,
                    _ => continue,
                };
            if let Some(session_id) = str_or_none(payload.get("sessionId")) {
                return Some(session_id);
            }
        }
        None
    }

    fn find_session_file(&self, session_id: &str, cwd: Option<&str>) -> Option<PathBuf> {
        if session_id.is_empty() {
            return None;
        }
        let root = self._projects_root();
        if !root.is_dir() {
            return None;
        }
        let candidate = format!("{session_id}.jsonl");
        if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
            let direct = root.join(_cwd_slug(cwd)).join(&candidate);
            if direct.exists() {
                return Some(direct);
            }
        }
        // Per-send gate path: one stat per project dir before the deep walk,
        // which is the last resort for a session nested below a project dir.
        _stat_project_dirs(&root, &candidate).or_else(|| {
            let mut matches: Vec<PathBuf> = Vec::new();
            _walk_files(&root, &mut |path| {
                if path.file_name().and_then(|n| n.to_str()) == Some(candidate.as_str()) {
                    matches.push(path.to_path_buf());
                }
            });
            matches.into_iter().next()
        })
    }

    // --- reading ---

    fn read_meta(&self, path: &Path) -> Option<SessionMeta> {
        let file = fs::File::open(path).ok()?;
        let mut reader = BufReader::new(file);
        let mut session_id: Option<String> = None;
        let mut cwd: Option<String> = None;
        let mut model: Option<String> = None;
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
            if session_id.is_none() {
                session_id = str_or_none(payload.get("sessionId"));
            }
            if cwd.is_none() {
                cwd = str_or_none(payload.get("cwd"));
            }
            if model.is_none() {
                if let Some(Value::Object(msg)) = payload.get("message") {
                    model = str_or_none(msg.get("model"));
                }
            }
            if session_id.is_some() && cwd.is_some() && model.is_some() {
                break;
            }
        }
        let session_id = session_id?;
        Some(SessionMeta {
            session_id,
            cwd,
            model,
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
            if let Some(message) = _claude_message_from_payload(&payload) {
                return Some(message);
            }
        }))
    }
}

const _META_SCAN_LIMIT: usize = 20;

fn _claude_message_from_payload(payload: &Map<String, Value>) -> Option<Message> {
    let record_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    if record_type != "user" && record_type != "assistant" {
        return None;
    }
    let Some(Value::Object(msg)) = payload.get("message") else {
        return None;
    };
    Some(Message {
        message_id: str_or_none(payload.get("uuid")),
        parent_id: str_or_none(payload.get("parentUuid")),
        role: str_or_none(msg.get("role")).unwrap_or_else(|| record_type.to_string()),
        parts: _iter_claude_parts(msg.get("content")),
        timestamp: parse_iso_timestamp(payload.get("timestamp")),
        raw: payload.clone(),
    })
}

fn _iter_claude_parts(content: Option<&Value>) -> Vec<MessagePart> {
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
                    "text" => parts.push(MessagePart {
                        kind: "text".to_string(),
                        text: Some(str_or_none(map.get("text")).unwrap_or_default()),
                        raw: Some(block.clone()),
                        ..Default::default()
                    }),
                    "thinking" => parts.push(MessagePart {
                        kind: "thinking".to_string(),
                        text: Some(str_or_none(map.get("thinking")).unwrap_or_default()),
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
                    "tool_result" => {
                        let output_text = match map.get("content") {
                            Some(Value::Array(items)) => {
                                let texts: Vec<&str> = items
                                    .iter()
                                    .filter_map(|b| {
                                        if b.get("type").and_then(Value::as_str) == Some("text") {
                                            b.get("text").and_then(Value::as_str)
                                        } else {
                                            None
                                        }
                                    })
                                    .filter(|t| !t.is_empty())
                                    .collect();
                                Some(texts.join("\n"))
                            }
                            None | Some(Value::Null) => None,
                            Some(Value::String(s)) => Some(s.clone()),
                            Some(other) => Some(other.to_string()),
                        };
                        parts.push(MessagePart {
                            kind: "tool_result".to_string(),
                            tool_output: output_text,
                            raw: Some(block.clone()),
                            ..Default::default()
                        });
                    }
                    "image" => parts.push(MessagePart {
                        kind: "image".to_string(),
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

const _CLAUDE_TOKENS: [&str; 2] = ["claude", "claude.exe"];

/// Match the executable itself (ps comm / argv[0]), or the script-runtime
/// shape `node <.../claude> …`.
///
/// Later argv tokens are the process's own arguments — `rg claude src` is
/// a search, not a claude — so they are never scanned.
fn _is_claude_process(command: &str, argv: &str) -> bool {
    if _CLAUDE_TOKENS.contains(&normalize_command_token(command).as_str()) {
        return true;
    }
    let parts: Vec<&str> = argv.split_whitespace().collect();
    if parts.is_empty() {
        return false;
    }
    if _CLAUDE_TOKENS.contains(&normalize_command_token(parts[0]).as_str()) {
        return true;
    }
    parts.len() >= 2
        && normalize_command_token(parts[0]) == "node"
        && _CLAUDE_TOKENS.contains(&normalize_command_token(parts[1]).as_str())
}

/// Claude's project-dir slug for a cwd. Observed rule (from real
/// `~/.claude/projects` dirs against the `cwd` their transcripts record):
/// every character outside `[A-Za-z0-9]` becomes `-`, so `/`, `.` and `_`
/// all collapse — `/Users/x/.dotfiles/a_b` is `-Users-x--dotfiles-a-b`.
fn _cwd_slug(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Stat `<root>/<project>/<candidate>` for each top-level project dir; no
/// recursion, so a miss costs one readdir plus one stat per project.
/// Dot-dirs are skipped: project dirs are cwd slugs (`_cwd_slug`, never a
/// leading dot), so a dot-dir under `projects/` is foreign to Claude Code.
pub(crate) fn _stat_project_dirs(root: &Path, candidate: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path().join(candidate))
        .filter(|p| p.is_file())
        .collect();
    paths.sort();
    paths.into_iter().next()
}

/// Recursive file walk standing in for `Path.rglob`.
fn _walk_files(dir: &Path, visit: &mut dyn FnMut(&Path)) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            _walk_files(&path, visit);
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

    fn _write_jsonl(path: &Path, lines: &[Value]) {
        let text: String = lines.iter().map(|l| l.to_string() + "\n").collect();
        fs::write(path, text).unwrap();
    }

    // --- iter_messages ---

    #[test]
    fn test_claude_iter_messages_normalizes_text_and_tool_use() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("claude.jsonl");
        _write_jsonl(
            &path,
            &[
                json!({
                    "type": "user",
                    "uuid": "u1",
                    "parentUuid": null,
                    "sessionId": "s",
                    "cwd": "/w",
                    "timestamp": "2026-04-02T05:27:52.478Z",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": "hi"}],
                    },
                }),
                json!({
                    "type": "assistant",
                    "uuid": "u2",
                    "parentUuid": "u1",
                    "timestamp": "2026-04-02T05:27:53.000Z",
                    "message": {
                        "role": "assistant",
                        "content": [
                            {"type": "thinking", "thinking": "hmm"},
                            {"type": "text", "text": "ok"},
                            {"type": "tool_use", "name": "Read", "input": {"path": "/a"}},
                        ],
                    },
                }),
            ],
        );

        let messages: Vec<Message> = ClaudeAdapter.iter_messages(&path).collect();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].parts[0].text.as_deref(), Some("hi"));
        assert_eq!(messages[1].role, "assistant");
        let kinds: Vec<&str> = messages[1].parts.iter().map(|p| p.kind.as_str()).collect();
        assert_eq!(kinds, ["thinking", "text", "tool_use"]);
        assert_eq!(messages[1].parts[2].tool_name.as_deref(), Some("Read"));
        assert_eq!(messages[1].parent_id.as_deref(), Some("u1"));
    }

    #[test]
    fn test_claude_iter_messages_handles_string_content() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("claude.jsonl");
        _write_jsonl(
            &path,
            &[json!({
                "type": "user",
                "uuid": "u1",
                "sessionId": "s",
                "message": {"role": "user", "content": "plain"},
            })],
        );
        let messages: Vec<Message> = ClaudeAdapter.iter_messages(&path).collect();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].parts[0].kind, "text");
        assert_eq!(messages[0].parts[0].text.as_deref(), Some("plain"));
    }

    #[test]
    fn test_claude_iter_messages_skips_unknown_record_types() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("claude.jsonl");
        _write_jsonl(
            &path,
            &[
                json!({"type": "permission-mode", "permissionMode": "bypass"}),
                json!({"type": "file-history-snapshot", "messageId": "x"}),
                json!({
                    "type": "assistant",
                    "uuid": "u1",
                    "message": {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
                }),
            ],
        );
        let messages: Vec<Message> = ClaudeAdapter.iter_messages(&path).collect();
        assert_eq!(messages.len(), 1);
    }

    // --- find_session_file / session_meta ---

    fn _write_claude_jsonl(path: &Path, session_id: &str, cwd: &str) {
        fs::write(
            path,
            json!({
                "type": "user",
                "sessionId": session_id,
                "cwd": cwd,
                "parentUuid": null,
                "uuid": "uuid-1",
                "timestamp": "2026-04-02T05:27:52.478Z",
                "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            })
            .to_string()
                + "\n",
        )
        .unwrap();
    }

    #[test]
    fn test_stat_project_dirs_skips_dot_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        let hidden = root.join(".trash");
        fs::create_dir_all(&hidden).unwrap();
        fs::write(hidden.join("sid.jsonl"), "").unwrap();
        assert_eq!(_stat_project_dirs(&root, "sid.jsonl"), None);

        let real = root.join("-work-hive");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("sid.jsonl"), "").unwrap();
        assert_eq!(
            _stat_project_dirs(&root, "sid.jsonl"),
            Some(real.join("sid.jsonl"))
        );
    }

    #[test]
    fn test_claude_find_session_file_uses_cwd_slug() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("CLAUDE_HOME", tmp.path());
        let cwd = "/Users/notdp/Developer/hive/.claude/worktrees/wt_1";
        let projects = tmp
            .path()
            .join("projects")
            .join("-Users-notdp-Developer-hive--claude-worktrees-wt-1");
        fs::create_dir_all(&projects).unwrap();
        let target = projects.join("cafe-babe.jsonl");
        _write_claude_jsonl(&target, "cafe-babe", cwd);

        let adapter = ClaudeAdapter;
        assert_eq!(
            adapter.find_session_file("cafe-babe", Some(cwd)),
            Some(target.clone())
        );

        // Also resolves via the per-project stat when no cwd hint.
        assert_eq!(adapter.find_session_file("cafe-babe", None), Some(target));
    }

    #[test]
    fn test_claude_cwd_slug_collapses_non_alphanumerics() {
        assert_eq!(
            _cwd_slug("/Users/notdp/Developer/hive"),
            "-Users-notdp-Developer-hive"
        );
        assert_eq!(
            _cwd_slug("/Users/notdp/.github-runners/ordo_ai/_work"),
            "-Users-notdp--github-runners-ordo-ai--work"
        );
    }

    #[test]
    fn test_claude_find_session_file_dotted_cwd_hits_direct_without_walk() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("CLAUDE_HOME", tmp.path());
        let root = tmp.path().join("projects");
        let cwd = "/Users/notdp/Developer/hive/.claude/worktrees/wt-1";
        let projects = root.join("-Users-notdp-Developer-hive--claude-worktrees-wt-1");
        fs::create_dir_all(&projects).unwrap();
        let target = projects.join("cafe-babe.jsonl");
        _write_claude_jsonl(&target, "cafe-babe", cwd);
        // A same-named file nested deeper under another project sorts first
        // for the walk; the direct hit must win without ever reaching it.
        let decoy_dir = root.join("-Users-notdp-Developer-hive").join("nested");
        fs::create_dir_all(&decoy_dir).unwrap();
        let decoy = decoy_dir.join("cafe-babe.jsonl");
        _write_claude_jsonl(&decoy, "cafe-babe", "/Users/notdp/Developer/hive/nested");

        let adapter = ClaudeAdapter;
        assert_eq!(
            adapter.find_session_file("cafe-babe", Some(cwd)),
            Some(target)
        );
        // Without a cwd hint the per-project stat still skips the nested decoy.
        assert_eq!(
            adapter.find_session_file("cafe-babe", None),
            Some(projects.join("cafe-babe.jsonl"))
        );
        // The deep walk remains the last resort for a nested-only session.
        fs::remove_file(projects.join("cafe-babe.jsonl")).unwrap();
        assert_eq!(adapter.find_session_file("cafe-babe", None), Some(decoy));
    }

    #[test]
    fn test_claude_read_meta_scans_first_records() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("claude.jsonl");
        _write_jsonl(
            &path,
            &[
                json!({"type": "permission-mode", "permissionMode": "bypass"}),
                json!({
                    "type": "user",
                    "sessionId": "sess-c",
                    "cwd": "/work",
                    "parentUuid": null,
                    "uuid": "u1",
                    "timestamp": "2026-04-02T05:27:52.478Z",
                    "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                }),
                json!({
                    "type": "assistant",
                    "sessionId": "sess-c",
                    "cwd": "/work",
                    "parentUuid": "u1",
                    "uuid": "u2",
                    "timestamp": "2026-04-02T05:27:53.000Z",
                    "message": {"role": "assistant", "model": "claude-opus-4-6", "content": [{"type": "text", "text": "ok"}]},
                }),
            ],
        );
        let meta = ClaudeAdapter.read_meta(&path).expect("meta");
        assert_eq!(meta.session_id, "sess-c");
        assert_eq!(meta.cwd.as_deref(), Some("/work"));
        assert_eq!(meta.model.as_deref(), Some("claude-opus-4-6"));
    }

    #[test]
    fn test_claude_read_meta_missing_session_id_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("no-id.jsonl");
        _write_jsonl(&path, &[json!({"type": "permission-mode"})]);
        assert!(ClaudeAdapter.read_meta(&path).is_none());
    }
}
