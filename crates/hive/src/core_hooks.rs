//! Home-directory paths shared by the plugin and asset code, plus the hook
//! removal that `plugin_manager::disable_plugin` runs for legacy installs.
//!
//! No shipped plugin installs hooks any more, so only the removal half of
//! the hook machinery survives: hook definitions are JSON
//! documents (`event -> [group, ...]`) that an older enable merged into
//! `~/.claude/settings.json` and `~/.codex/hooks.json`, and
//! `remove_hook_groups` strips exactly those groups back out.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Map, Value};

/// Event name -> array of hook groups.
pub type HookDefs = Map<String, Value>;

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

/// The claude config tree, resolved the one way hive resolves it
/// (`claude_sessions::config_dir`: CLAUDE_HOME, then CLAUDE_CONFIG_DIR).
pub fn claude_home() -> PathBuf {
    crate::adapters::claude_sessions::config_dir()
}

pub fn claude_settings_path() -> PathBuf {
    claude_home().join("settings.json")
}

pub fn codex_hooks_path() -> PathBuf {
    crate::adapters::codex_app_server::codex_home().join("hooks.json")
}

fn load_json_file(path: &Path) -> Map<String, Value> {
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

pub(crate) fn save_json_file(path: &Path, data: &Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // json.dumps(data, indent=2, ensure_ascii=False) + "\n"
    fs::write(path, format!("{}\n", serde_json::to_string_pretty(data)?))?;
    Ok(())
}

fn remove_hooks_in_data(data: &mut Map<String, Value>, hook_defs: &HookDefs) -> bool {
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

pub const CODEX_SUPPORTED_HOOK_EVENTS: [&str; 5] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
];

fn filter_hook_defs_for_codex(hook_defs: &HookDefs) -> HookDefs {
    hook_defs
        .iter()
        .filter(|(k, _)| CODEX_SUPPORTED_HOOK_EVENTS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

pub fn remove_hook_groups(hook_defs: &HookDefs) -> Result<()> {
    // Claude Code
    let claude_path = claude_settings_path();
    let mut claude_data = load_json_file(&claude_path);
    if remove_hooks_in_data(&mut claude_data, hook_defs) {
        save_json_file(&claude_path, &claude_data)?;
    }
    // Codex
    let codex_path = codex_hooks_path();
    let codex_defs = filter_hook_defs_for_codex(hook_defs);
    if !codex_defs.is_empty() {
        let mut codex_data = load_json_file(&codex_path);
        if remove_hooks_in_data(&mut codex_data, &codex_defs) {
            save_json_file(&codex_path, &codex_data)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;
    use serde_json::json;

    fn defs(v: Value) -> HookDefs {
        match v {
            Value::Object(m) => m,
            _ => unreachable!(),
        }
    }

    fn setup_homes() -> (tempfile::TempDir, EnvGuard) {
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("HIVE_HOME", tmp.path().join(".hive"));
        env.set("CLAUDE_HOME", tmp.path().join(".claude"));
        env.set("CODEX_HOME", tmp.path().join(".codex"));
        (tmp, env)
    }

    fn write_json(path: &Path, value: &Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_string(value).unwrap()).unwrap();
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn test_remove_hook_groups_strips_managed_groups_from_both_clis() {
        let (tmp, _guard) = setup_homes();
        let claude_settings = tmp.path().join(".claude").join("settings.json");
        let codex_hooks = tmp.path().join(".codex").join("hooks.json");
        let hook_defs = defs(json!({
            "Notification": [{"hooks": [{"type": "command", "command": "/tmp/notify-hook", "timeout": 5}]}],
            "Stop": [{"hooks": [{"type": "command", "command": "/tmp/notify-hook", "timeout": 5}]}],
        }));
        // what a legacy enable left behind: every group on claude, only the
        // codex-supported events on codex
        write_json(
            &claude_settings,
            &json!({"model": "opus", "hooks": {
                "Notification": hook_defs["Notification"],
                "Stop": hook_defs["Stop"],
            }}),
        );
        write_json(&codex_hooks, &json!({"hooks": {"Stop": hook_defs["Stop"]}}));

        remove_hook_groups(&hook_defs).unwrap();

        let claude = read_json(&claude_settings);
        assert!(claude.get("hooks").is_none());
        assert_eq!(claude["model"], "opus");
        assert!(read_json(&codex_hooks).get("hooks").is_none());
    }

    #[test]
    fn test_remove_hook_groups_keeps_unmanaged_groups_and_missing_files() {
        let (tmp, _guard) = setup_homes();
        let claude_settings = tmp.path().join(".claude").join("settings.json");
        let codex_hooks = tmp.path().join(".codex").join("hooks.json");
        let managed = json!({"hooks": [{"type": "command", "command": "/tmp/hive-notify-hook", "timeout": 5}]});
        let user =
            json!({"hooks": [{"type": "command", "command": "~/.dotfiles/bin/notify-hook"}]});
        write_json(
            &claude_settings,
            &json!({"hooks": {
                "Notification": [user, managed],
                "Stop": [{"hooks": [{"type": "command", "command": "/tmp/custom-hook"}]}],
            }}),
        );
        let hook_defs = defs(json!({
            "Notification": [managed],
            "Stop": [managed],
        }));

        remove_hook_groups(&hook_defs).unwrap();

        let claude = read_json(&claude_settings);
        assert_eq!(claude["hooks"]["Notification"], json!([user]));
        assert_eq!(
            claude["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/tmp/custom-hook"
        );
        // a codex home that never had hooks.json is left alone
        assert!(!codex_hooks.exists());
    }
}
