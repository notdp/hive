//! Hook-group management for the Claude Code and Codex settings files.
//!
//! Port of `src/hive/core_hooks.py`. Hook definitions are JSON documents
//! (`event -> [group, ...]`) merged into `~/.claude/settings.json` and
//! `~/.codex/hooks.json`; the codex path also converges the
//! `[features].hooks = true` flag in `~/.codex/config.toml`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Map, Value};

/// `dict[str, list[dict[str, Any]]]` in Python: event name -> array of groups.
pub type HookDefs = Map<String, Value>;

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

// Python delegates to `team.HIVE_HOME`, an env-derived module constant; like
// context.rs, it is a per-call env read here so tests can redirect it.
pub fn hive_home() -> PathBuf {
    PathBuf::from(std::env::var("HIVE_HOME").unwrap_or_else(|_| format!("{}/.hive", home())))
}

/// Write an embedded asset tree under `root`, rewriting any file whose
/// on-disk copy drifted from the embedded content (heal-on-drift). Used by
/// the cvim toolkit and the plugin marketplace materialization.
pub(crate) fn materialize_asset_tree(root: &Path, files: &[(&str, &str, bool)]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for (rel, content, executable) in files {
        let path = root.join(rel);
        if fs::read_to_string(&path).ok().as_deref() != Some(*content) {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, content)?;
        }
        if *executable {
            let mut perms = fs::metadata(&path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms)?;
        }
    }
    Ok(())
}

pub fn claude_home() -> PathBuf {
    PathBuf::from(std::env::var("CLAUDE_HOME").unwrap_or_else(|_| format!("{}/.claude", home())))
}

pub fn claude_settings_path() -> PathBuf {
    claude_home().join("settings.json")
}

pub fn codex_home() -> PathBuf {
    PathBuf::from(std::env::var("CODEX_HOME").unwrap_or_else(|_| format!("{}/.codex", home())))
}

pub fn codex_hooks_path() -> PathBuf {
    codex_home().join("hooks.json")
}

pub(crate) fn _load_json_file(path: &Path) -> Map<String, Value> {
    if !path.exists() {
        return Map::new();
    }
    match fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

pub(crate) fn _save_json_file(path: &Path, data: &Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // json.dumps(data, indent=2, ensure_ascii=False) + "\n"
    fs::write(path, format!("{}\n", serde_json::to_string_pretty(data)?))?;
    Ok(())
}

fn _merge_hooks_in_data(data: &mut Map<String, Value>, hook_defs: &HookDefs) -> bool {
    if !data.contains_key("hooks") {
        data.insert("hooks".to_string(), Value::Object(Map::new()));
    }
    let Some(hooks) = data.get_mut("hooks").and_then(|v| v.as_object_mut()) else {
        return false; // non-object "hooks" in a corrupt settings file
    };
    let mut changed = false;
    for (event, groups) in hook_defs {
        let groups = groups.as_array().cloned().unwrap_or_default();
        if !hooks.contains_key(event) {
            hooks.insert(event.clone(), Value::Array(Vec::new()));
        }
        let Some(existing) = hooks.get_mut(event).and_then(|v| v.as_array_mut()) else {
            continue;
        };
        for group in groups {
            if existing.contains(&group) {
                continue;
            }
            existing.push(group);
            changed = true;
        }
    }
    changed
}

fn _remove_hooks_in_data(data: &mut Map<String, Value>, hook_defs: &HookDefs) -> bool {
    let Some(hooks) = data.get_mut("hooks").and_then(|v| v.as_object_mut()) else {
        return false;
    };
    let mut changed = false;
    for (event, groups) in hook_defs {
        let group_list: &[Value] = groups.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
        let Some(existing) = hooks.get(event).and_then(|v| v.as_array()) else {
            continue;
        };
        let new_existing: Vec<Value> = existing
            .iter()
            .filter(|g| !group_list.contains(g))
            .cloned()
            .collect();
        // filter only removes elements, so a length change is equality change
        if new_existing.len() != existing.len() {
            changed = true;
            if !new_existing.is_empty() {
                hooks.insert(event.clone(), Value::Array(new_existing));
            } else {
                hooks.remove(event);
            }
        }
    }
    let hooks_empty = hooks.is_empty();
    if changed && hooks_empty {
        data.remove("hooks");
    }
    changed
}

/// `^\s*<key>\s*=` — anchored at the start of the assignment: `hooks = true`
/// is a suffix of the legacy `codex_hooks = true`, so substring checks are
/// forbidden.
fn _is_assignment(line: &str, key: &str) -> bool {
    match line.trim_start().strip_prefix(key) {
        Some(rest) => rest.trim_start().starts_with('='),
        None => false,
    }
}

/// Python `str.splitlines(keepends=True)`.
// ponytail: only \n / \r\n / \r line breaks (the exotic Python boundaries
// never occur in config.toml); extend if one ever shows up.
fn splitlines_keepends(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        cur.push(c);
        let is_break = match c {
            '\n' => true,
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    cur.push(chars.next().unwrap());
                }
                true
            }
            _ => false,
        };
        if is_break {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `[start, end)` line span of the `[features]` section body.
fn _features_span(lines: &[String]) -> Option<(usize, usize)> {
    let mut start: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let stripped = line.trim();
        match start {
            None => {
                if let Some(rest) = stripped.strip_prefix("[features]") {
                    let rest = rest.trim();
                    if rest.is_empty() || rest.starts_with('#') {
                        start = Some(i + 1);
                    }
                }
            }
            Some(s) => {
                if stripped.starts_with('[') {
                    return Some((s, i));
                }
            }
        }
    }
    start.map(|s| (s, lines.len()))
}

/// Converge `[features].hooks = true`; `codex_hooks` is the retired spelling.
fn _ensure_codex_hooks_enabled() -> Result<()> {
    let config_path = codex_home().join("config.toml");
    let mut content = String::new();
    if config_path.exists() {
        match fs::read_to_string(&config_path) {
            Ok(text) => content = text,
            Err(_) => return Ok(()),
        }
    }
    let original = content.clone();
    let mut lines = splitlines_keepends(&content);
    match _features_span(&lines) {
        None => {
            let section = "[features]\nhooks = true\n";
            if content.is_empty() {
                content = section.to_string();
            } else if content.ends_with('\n') {
                content.push_str(section);
            } else {
                content.push('\n');
                content.push_str(section);
            }
        }
        Some((start, end)) => {
            let body = &lines[start..end];
            let mut kept: Vec<String> = body
                .iter()
                .filter(|line| !_is_assignment(line, "codex_hooks"))
                .cloned()
                .collect();
            let mut changed = kept.len() != body.len();
            if !kept.iter().any(|line| _is_assignment(line, "hooks")) {
                kept.insert(0, "hooks = true\n".to_string());
                changed = true;
            }
            if !changed {
                return Ok(());
            }
            if start > 0 && !lines[start - 1].ends_with('\n') {
                lines[start - 1].push('\n');
            }
            lines.splice(start..end, kept);
            content = lines.concat();
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

pub const CODEX_SUPPORTED_HOOK_EVENTS: [&str; 5] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
];

fn _filter_hook_defs_for_codex(hook_defs: &HookDefs) -> HookDefs {
    hook_defs
        .iter()
        .filter(|(k, _)| CODEX_SUPPORTED_HOOK_EVENTS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

pub fn merge_hook_groups(hook_defs: &HookDefs) -> Result<HookDefs> {
    // Claude Code
    let claude_path = claude_settings_path();
    let mut claude_data = _load_json_file(&claude_path);
    if _merge_hooks_in_data(&mut claude_data, hook_defs) {
        _save_json_file(&claude_path, &claude_data)?;
    }
    // Codex
    let codex_path = codex_hooks_path();
    let codex_defs = _filter_hook_defs_for_codex(hook_defs);
    if !codex_defs.is_empty() {
        let mut codex_data = _load_json_file(&codex_path);
        if _merge_hooks_in_data(&mut codex_data, &codex_defs) {
            _save_json_file(&codex_path, &codex_data)?;
        }
        _ensure_codex_hooks_enabled()?;
    }
    // Python appends every group unconditionally, so `added` == the input.
    Ok(hook_defs.clone())
}

pub fn remove_hook_groups(hook_defs: &HookDefs) -> Result<()> {
    // Claude Code
    let claude_path = claude_settings_path();
    let mut claude_data = _load_json_file(&claude_path);
    if _remove_hooks_in_data(&mut claude_data, hook_defs) {
        _save_json_file(&claude_path, &claude_data)?;
    }
    // Codex
    let codex_path = codex_hooks_path();
    let codex_defs = _filter_hook_defs_for_codex(hook_defs);
    if !codex_defs.is_empty() {
        let mut codex_data = _load_json_file(&codex_path);
        if _remove_hooks_in_data(&mut codex_data, &codex_defs) {
            _save_json_file(&codex_path, &codex_data)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::TEST_ENV_LOCK;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::MutexGuard;

    fn defs(v: Value) -> HookDefs {
        match v {
            Value::Object(m) => m,
            _ => unreachable!(),
        }
    }

    fn codex_hook() -> HookDefs {
        defs(json!({
            "Stop": [{"hooks": [{"type": "command", "command": "/tmp/notify-hook", "timeout": 5}]}]
        }))
    }

    fn setup_homes() -> (tempfile::TempDir, MutexGuard<'static, ()>) {
        let guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path().join(".hive"));
        std::env::set_var("CLAUDE_HOME", tmp.path().join(".claude"));
        std::env::set_var("CODEX_HOME", tmp.path().join(".codex"));
        (tmp, guard)
    }

    fn merge_with_config(
        pre: Option<&str>,
    ) -> (tempfile::TempDir, PathBuf, MutexGuard<'static, ()>) {
        let (tmp, guard) = setup_homes();
        let config_path = tmp.path().join(".codex").join("config.toml");
        if let Some(pre) = pre {
            fs::create_dir_all(config_path.parent().unwrap()).unwrap();
            fs::write(&config_path, pre).unwrap();
        }
        merge_hook_groups(&codex_hook()).unwrap();
        (tmp, config_path, guard)
    }

    /// Minimal TOML reader for the flat `section -> key = raw-value` shape the
    /// codex config uses (the pytest suite parses with tomllib; the raw value
    /// strings `true` / `false` / `"on"` are compared here instead).
    fn toml_lite(text: &str) -> BTreeMap<String, BTreeMap<String, String>> {
        let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let mut section = String::new();
        for line in text.lines() {
            let s = line.trim();
            if s.is_empty() || s.starts_with('#') {
                continue;
            }
            if s.starts_with('[') && s.ends_with(']') {
                section = s[1..s.len() - 1].to_string();
                out.entry(section.clone()).or_default();
                continue;
            }
            if let Some((k, v)) = s.split_once('=') {
                out.entry(section.clone())
                    .or_default()
                    .insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        out
    }

    fn _features_body(text: &str) -> String {
        let lines: Vec<String> = text.lines().map(|l| format!("{}\n", l)).collect();
        let (start, end) = _features_span(&lines).expect("no [features] section");
        lines[start..end]
            .iter()
            .map(|l| l.trim_end_matches('\n'))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn _hooks_assignment_count(section_body: &str) -> usize {
        section_body
            .lines()
            .filter(|l| _is_assignment(l, "hooks"))
            .count()
    }

    #[test]
    fn test_codex_features_flag_written_for_fresh_home() {
        let (_tmp, config_path, _guard) = merge_with_config(None);
        let text = fs::read_to_string(&config_path).unwrap();
        assert_eq!(toml_lite(&text)["features"]["hooks"], "true");
        assert!(!text.contains("codex_hooks"));
        assert_eq!(_hooks_assignment_count(&_features_body(&text)), 1);
    }

    #[test]
    fn test_codex_features_flag_migrates_legacy_and_keeps_user_lines() {
        let pre =
            "[features]\ncodex_hooks = true\nweb_search = \"on\"\n\n[tui]\ntheme = \"dark\"\n";
        let (_tmp, config_path, _guard) = merge_with_config(Some(pre));
        let text = fs::read_to_string(&config_path).unwrap();
        let data = toml_lite(&text);
        assert_eq!(data["features"]["hooks"], "true");
        assert!(!data["features"].contains_key("codex_hooks"));
        assert_eq!(data["features"]["web_search"], "\"on\"");
        assert_eq!(data["tui"]["theme"], "\"dark\"");
        assert_eq!(_hooks_assignment_count(&_features_body(&text)), 1);
    }

    #[test]
    fn test_codex_features_flag_respects_explicit_value() {
        for value in ["true", "false"] {
            let pre = format!("[features]\nhooks = {}\n", value);
            let (_tmp, config_path, _guard) = merge_with_config(Some(&pre));
            assert_eq!(fs::read_to_string(&config_path).unwrap(), pre);
        }
    }

    #[test]
    fn test_codex_features_flag_migration_keeps_explicit_false() {
        let pre = "[features]\nhooks = false\ncodex_hooks = true\n";
        let (_tmp, config_path, _guard) = merge_with_config(Some(pre));
        let text = fs::read_to_string(&config_path).unwrap();
        let mut expected = BTreeMap::new();
        expected.insert("hooks".to_string(), "false".to_string());
        assert_eq!(toml_lite(&text)["features"], expected);
        assert_eq!(_hooks_assignment_count(&_features_body(&text)), 1);
    }

    #[test]
    fn test_codex_features_flag_ignores_suffix_and_other_sections() {
        // `hooks = true` is a suffix of `codex_hooks = true`, and other
        // sections may legitimately own a `hooks` key: neither may satisfy or
        // be edited
        let pre = "[features]\ncodex_hooks = true\n\n[mcp_servers.x]\nhooks = false\n";
        let (_tmp, config_path, _guard) = merge_with_config(Some(pre));
        let data = toml_lite(&fs::read_to_string(&config_path).unwrap());
        let mut expected = BTreeMap::new();
        expected.insert("hooks".to_string(), "true".to_string());
        assert_eq!(data["features"], expected);
        assert_eq!(data["mcp_servers.x"]["hooks"], "false");
    }

    #[test]
    fn test_codex_features_flag_ignores_commented_assignments() {
        let pre = "[features]\n# codex_hooks = true\n# hooks = false\n";
        let (_tmp, config_path, _guard) = merge_with_config(Some(pre));
        let text = fs::read_to_string(&config_path).unwrap();
        let mut expected = BTreeMap::new();
        expected.insert("hooks".to_string(), "true".to_string());
        assert_eq!(toml_lite(&text)["features"], expected);
        assert!(text.contains("# codex_hooks = true"));
        assert!(text.contains("# hooks = false"));
    }

    #[test]
    fn test_codex_features_flag_appends_section_preserving_layout() {
        // insertion-only migration: existing bytes survive verbatim, including
        // trailing blank lines the user left on purpose
        let pre = "[tui]\ntheme = \"dark\"\n\n# keep trailing layout\n\n";
        let (_tmp, config_path, _guard) = merge_with_config(Some(pre));
        let text = fs::read_to_string(&config_path).unwrap();
        assert_eq!(text, format!("{}[features]\nhooks = true\n", pre));
        assert_eq!(toml_lite(&text)["features"]["hooks"], "true");
    }

    #[test]
    fn test_codex_features_flag_converges_idempotently() {
        let pre = "[features]\ncodex_hooks = true\nweb_search = \"on\"\n";
        let (_tmp, config_path, _guard) = merge_with_config(Some(pre));
        let first = fs::read_to_string(&config_path).unwrap();
        merge_hook_groups(&codex_hook()).unwrap();
        assert_eq!(fs::read_to_string(&config_path).unwrap(), first);
    }

    #[test]
    fn test_merge_and_remove_hook_groups_round_trip() {
        let (tmp, _guard) = setup_homes();
        let claude_home = tmp.path().join(".claude");
        let codex_home = tmp.path().join(".codex");
        let hook_defs = defs(json!({
            "Notification": [{"hooks": [{"type": "command", "command": "/tmp/notify-hook", "timeout": 5}]}],
            "Stop": [{"hooks": [{"type": "command", "command": "/tmp/notify-hook", "timeout": 5}]}],
        }));

        merge_hook_groups(&hook_defs).unwrap();

        let claude_settings: Value =
            serde_json::from_str(&fs::read_to_string(claude_home.join("settings.json")).unwrap())
                .unwrap();
        let codex_hooks: Value =
            serde_json::from_str(&fs::read_to_string(codex_home.join("hooks.json")).unwrap())
                .unwrap();

        assert_eq!(
            claude_settings["hooks"]["Notification"],
            hook_defs["Notification"]
        );
        assert_eq!(claude_settings["hooks"]["Stop"], hook_defs["Stop"]);
        assert_eq!(codex_hooks["hooks"]["Stop"], hook_defs["Stop"]);
        assert!(codex_hooks["hooks"].get("Notification").is_none());

        remove_hook_groups(&hook_defs).unwrap();

        let claude_settings: Value =
            serde_json::from_str(&fs::read_to_string(claude_home.join("settings.json")).unwrap())
                .unwrap();
        let codex_hooks: Value =
            serde_json::from_str(&fs::read_to_string(codex_home.join("hooks.json")).unwrap())
                .unwrap();
        assert!(claude_settings.get("hooks").is_none());
        assert!(codex_hooks.get("hooks").is_none());
    }

    #[test]
    fn test_merge_hook_groups_preserves_unmanaged_entries() {
        let (tmp, _guard) = setup_homes();
        let claude_home = tmp.path().join(".claude");
        let settings_path = claude_home.join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(
            &settings_path,
            serde_json::to_string(&json!({
                "hooks": {
                    "Notification": [{"hooks": [{"type": "command", "command": "~/.dotfiles/bin/notify-hook"}]}],
                    "Stop": [{"hooks": [{"type": "command", "command": "/tmp/custom-hook"}]}],
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let hook_defs = defs(json!({
            "Notification": [{"hooks": [{"type": "command", "command": "/tmp/hive-notify-hook", "timeout": 5}]}],
            "Stop": [{"hooks": [{"type": "command", "command": "/tmp/hive-notify-hook", "timeout": 5}]}],
        }));

        merge_hook_groups(&hook_defs).unwrap();
        let settings: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();

        assert_eq!(
            settings["hooks"]["Notification"][0]["hooks"][0]["command"],
            "~/.dotfiles/bin/notify-hook"
        );
        assert_eq!(
            settings["hooks"]["Notification"][1]["hooks"][0]["command"],
            "/tmp/hive-notify-hook"
        );
        assert_eq!(
            settings["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/tmp/custom-hook"
        );
        assert_eq!(
            settings["hooks"]["Stop"][1]["hooks"][0]["command"],
            "/tmp/hive-notify-hook"
        );
    }
}
