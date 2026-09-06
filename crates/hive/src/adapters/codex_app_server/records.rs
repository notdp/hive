use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::{json, Value};

pub fn codex_home() -> PathBuf {
    match env::var("CODEX_HOME") {
        Ok(value) => PathBuf::from(value),
        Err(_) => PathBuf::from(env::var("HOME").unwrap_or_default()).join(".codex"),
    }
}

/// The shared daemon's socket under the real CODEX_HOME.
///
/// Lives under `app-server-control/` (a real directory codex itself uses, so
/// it is never a symlink — codex rejects a symlinked socket parent, e.g.
/// `/tmp` on macOS). The path carries no per-pane or per-worktree component:
/// unix socket paths cap at ~104 bytes (SUN_LEN) and there is exactly one
/// daemon per CODEX_HOME.
pub fn shared_socket_path() -> PathBuf {
    codex_home()
        .join("app-server-control")
        .join("hive-shared.sock")
}

pub fn shared_pidfile_path() -> PathBuf {
    shared_socket_path().with_extension("pid")
}

/// Per-pane record of the thread hive bound to this pane.
pub fn pane_thread_path(pane: &str) -> PathBuf {
    let slug = pane.replace('%', "");
    let slug = if slug.is_empty() {
        "default"
    } else {
        slug.as_str()
    };
    codex_home()
        .join("app-server-control")
        .join(format!("hive-pane-{slug}.thread"))
}

pub fn write_pane_thread(pane: &str, thread_id: &str, cwd: &str) -> Result<()> {
    let path = pane_thread_path(pane);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &path,
        json!({"threadId": thread_id, "cwd": cwd}).to_string(),
    )?;
    Ok(())
}

/// The pane→thread binding hive wrote at spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneThread {
    pub thread_id: String,
    pub cwd: String,
}

pub fn read_pane_thread(pane: &str) -> Option<PaneThread> {
    let text = fs::read_to_string(pane_thread_path(pane)).ok()?;
    let data: Value = serde_json::from_str(&text).ok()?;
    let obj = data.as_object()?;
    let thread_id = obj
        .get("threadId")
        .and_then(Value::as_str)
        .filter(|tid| !tid.is_empty())?;
    let cwd = obj.get("cwd").and_then(Value::as_str).unwrap_or("");
    Some(PaneThread {
        thread_id: thread_id.to_string(),
        cwd: cwd.to_string(),
    })
}

pub fn clear_pane_thread(pane: &str) -> Result<()> {
    match fs::remove_file(pane_thread_path(pane)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub fn thread_id_for_pane(pane: &str) -> Option<String> {
    read_pane_thread(pane).map(|record| record.thread_id)
}

/// Inverse of [`pane_thread_path`]: `hive-pane-19.thread` -> `%19`.
fn pane_from_record_name(name: &str) -> Option<String> {
    let slug = name.strip_prefix("hive-pane-")?.strip_suffix(".thread")?;
    if slug.is_empty() || slug == "default" {
        return None;
    }
    Some(format!("%{slug}"))
}

/// Pane ids that currently have a thread record on disk.
pub fn list_recorded_panes() -> Vec<String> {
    let root = codex_home().join("app-server-control");
    if !root.is_dir() {
        return Vec::new();
    }
    let mut panes = Vec::new();
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if let Some(pane) = pane_from_record_name(name) {
                    panes.push(pane);
                }
            }
        }
    }
    panes
}

/// Pane recorded for *thread_id*, or None.
///
/// The reverse lookup behind tool-side identity: a `hive` invocation inside a
/// codex tool carries `CODEX_THREAD_ID` (injected per thread by codex), and
/// this maps it back to the tmux pane hive bound the thread to.
pub fn pane_for_thread(thread_id: &str) -> Option<String> {
    if thread_id.is_empty() {
        return None;
    }
    for pane in list_recorded_panes() {
        if let Some(record) = read_pane_thread(&pane) {
            if record.thread_id == thread_id {
                return Some(pane);
            }
        }
    }
    None
}

// --------------------------------------------------------------------------
// directory trust (config.toml)
// --------------------------------------------------------------------------

/// Matches `^\s*trust_level\s*=`.
fn trust_level_line(line: &str) -> bool {
    match line.trim_start().strip_prefix("trust_level") {
        Some(rest) => rest.trim_start().starts_with('='),
        None => false,
    }
}

/// Header spellings that name *directory*'s [projects] entry.
///
/// Codex writes the TOML basic-string form; the literal-string form is also
/// matched (when representable) so a hand-edited entry is not duplicated — a
/// duplicate table would make the whole config.toml unparsable.
fn trusted_section_headers(directory: &str) -> Vec<String> {
    let escaped = directory.replace('\\', "\\\\").replace('"', "\\\"");
    let mut headers = vec![format!("[projects.\"{escaped}\"]")];
    if !directory.contains('\'') {
        headers.push(format!("[projects.'{directory}']"));
    }
    headers
}

/// Split into lines, each keeping its terminator (`\n`, `\r\n`, or `\r`).
fn split_keepends(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                lines.push(text[start..=i].to_string());
                i += 1;
                start = i;
            }
            b'\r' => {
                let mut end = i + 1;
                if end < bytes.len() && bytes[end] == b'\n' {
                    end += 1;
                }
                lines.push(text[start..end].to_string());
                i = end;
                start = end;
            }
            _ => i += 1,
        }
    }
    if start < bytes.len() {
        lines.push(text[start..].to_string());
    }
    lines
}

/// Converge `[projects."<dir>"] trust_level = "trusted"` in config.toml.
///
/// Remote-mode directory trust is judged from the daemon's config.toml on
/// disk (`-c` overrides do not apply), so every new cwd must be trusted
/// before its thread starts. Idempotent line-level edit: read, minimally
/// patch, write only on change; an unreadable config is left alone.
pub fn ensure_dir_trusted(directory: &str) -> Result<()> {
    let config_path = codex_home().join("config.toml");
    let mut content = String::new();
    if config_path.exists() {
        match fs::read_to_string(&config_path) {
            Ok(text) => content = text,
            Err(_) => return Ok(()),
        }
    }
    let original = content.clone();
    let headers = trusted_section_headers(directory);
    let lines = split_keepends(&content);
    let mut start = None;
    for (i, line) in lines.iter().enumerate() {
        let stripped = line.trim();
        if headers.iter().any(|h| {
            stripped == h
                || stripped.starts_with(&format!("{h} "))
                || stripped.starts_with(&format!("{h}#"))
        }) {
            start = Some(i + 1);
            break;
        }
    }
    match start {
        None => {
            let section = format!("{}\ntrust_level = \"trusted\"\n", headers[0]);
            if content.is_empty() {
                content = section;
            } else if content.ends_with('\n') {
                content.push('\n');
                content.push_str(&section);
            } else {
                content.push_str("\n\n");
                content.push_str(&section);
            }
        }
        Some(start) => {
            let mut end = start;
            while end < lines.len() && !lines[end].trim().starts_with('[') {
                end += 1;
            }
            let mut body: Vec<String> = lines[start..end].to_vec();
            let mut replaced = false;
            for line in body.iter_mut() {
                if trust_level_line(line) {
                    if line.trim() == "trust_level = \"trusted\"" {
                        return Ok(());
                    }
                    *line = "trust_level = \"trusted\"\n".to_string();
                    replaced = true;
                    break;
                }
            }
            if !replaced {
                body.insert(0, "trust_level = \"trusted\"\n".to_string());
            }
            let mut rebuilt: Vec<String> = lines[..start].to_vec();
            rebuilt.extend(body);
            rebuilt.extend_from_slice(&lines[end..]);
            content = rebuilt.concat();
        }
    }
    if content != original {
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&config_path, content)?;
    }
    Ok(())
}
