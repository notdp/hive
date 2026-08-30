//! Optional plugin enable/disable lifecycle.
//!
//! Port of `src/hive/plugin_manager.py`. Shipped plugin data lives in
//! `src/hive/plugins/` (Python reads it via importlib resources); here the
//! files are embedded at compile time via `include_str!` per PORTING.md
//! ("Rust embeds or locates them"). Installed state lives on disk under
//! `$HIVE_HOME/plugins/`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};

#[derive(Debug, Clone)]
pub struct PluginManifest {
    pub name: String,
    pub description: String,
}

/// One shipped plugin: package-relative file paths and their contents.
struct BuiltinPlugin {
    name: &'static str,
    files: &'static [(&'static str, &'static str)],
}

// The embedded mirror of `resources.files("hive.plugins")`.
static BUILTIN_PLUGINS: &[BuiltinPlugin] = &[BuiltinPlugin {
    name: "notify",
    files: &[(
        "plugin.json",
        include_str!("../assets/plugins/notify/plugin.json"),
    )],
}];

fn _state_path() -> PathBuf {
    crate::core_hooks::hive_home()
        .join("plugins")
        .join("state.json")
}

fn _installed_root() -> PathBuf {
    crate::core_hooks::hive_home()
        .join("plugins")
        .join("installed")
}

fn _default_state() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("plugins".to_string(), Value::Object(Map::new()));
    m
}

fn _load_state() -> Map<String, Value> {
    let path = _state_path();
    if !path.exists() {
        return _default_state();
    }
    match fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    {
        Some(Value::Object(m)) => m,
        _ => _default_state(),
    }
}

fn _save_state(data: &Map<String, Value>) -> Result<()> {
    crate::core_hooks::_save_json_file(&_state_path(), data)
}

fn _remove_path(path: &Path) {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || meta.is_file() => {
            let _ = fs::remove_file(path);
        }
        Ok(meta) if meta.is_dir() => {
            let _ = fs::remove_dir_all(path);
        }
        _ => {}
    }
}

fn _ensure_executable_if_script(path: &Path) {
    let Ok(bytes) = fs::read(path) else { return };
    let text = String::from_utf8_lossy(&bytes);
    let Some(first_line) = text.lines().next() else {
        return;
    };
    if first_line.starts_with("#!") {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    }
}

/// Write the embedded plugin files under `dst` (Python `_copy_tree` walking
/// the resource dir; the `__pycache__` skip is moot for embedded data).
fn _copy_tree(files: &[(&str, &str)], dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for (rel, content) in files {
        let target = dst.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, content)?;
        _ensure_executable_if_script(&target);
    }
    Ok(())
}

fn _plugin_resource_dir(name: &str) -> Result<&'static BuiltinPlugin> {
    BUILTIN_PLUGINS
        .iter()
        .find(|p| p.name == name && p.files.iter().any(|(rel, _)| *rel == "plugin.json"))
        .ok_or_else(|| anyhow!("plugin '{}' not found", name))
}

pub fn load_manifest(name: &str) -> Result<PluginManifest> {
    let plugin = _plugin_resource_dir(name)?;
    let raw = plugin
        .files
        .iter()
        .find(|(rel, _)| *rel == "plugin.json")
        .map(|(_, content)| *content)
        .expect("checked by _plugin_resource_dir");
    let data: Value = serde_json::from_str(raw)?;
    let manifest_name = data
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("plugin.json for '{}' missing 'name'", name))?;
    Ok(PluginManifest {
        name: manifest_name.to_string(),
        description: data
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

pub fn is_plugin_enabled(name: &str) -> bool {
    _load_state()
        .get("plugins")
        .and_then(|v| v.as_object())
        .map(|m| m.contains_key(name))
        .unwrap_or(false)
}

pub fn list_plugins() -> Result<Vec<Value>> {
    let state = _load_state();
    let empty = Map::new();
    let enabled = state
        .get("plugins")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);
    let mut names: Vec<&str> = BUILTIN_PLUGINS
        .iter()
        .filter(|p| p.files.iter().any(|(rel, _)| *rel == "plugin.json"))
        .map(|p| p.name)
        .collect();
    names.sort_unstable();
    let mut rows = Vec::new();
    for name in names {
        let manifest = load_manifest(name)?;
        rows.push(json!({
            "name": manifest.name,
            "description": manifest.description,
            "enabled": enabled.contains_key(&manifest.name),
        }));
    }
    Ok(rows)
}

fn _render_plugin_text(content: &str, install_dir: &Path) -> String {
    content.replace("${HIVE_PLUGIN_ROOT}", &install_dir.to_string_lossy())
}

fn _materialize_installed_commands(install_dir: &Path) -> Result<Vec<PathBuf>> {
    let commands_dir = install_dir.join("commands");
    if !commands_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(&commands_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    let mut materialized = Vec::new();
    for command_path in entries {
        let file_name = command_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if file_name.starts_with('.') {
            continue;
        }
        let content = fs::read_to_string(&command_path)?;
        fs::write(&command_path, _render_plugin_text(&content, install_dir))?;
        _ensure_executable_if_script(&command_path);
        materialized.push(command_path);
    }
    Ok(materialized)
}

fn _source_tmux_conf(conf: &Path) -> bool {
    if !conf.is_file() {
        return false;
    }
    crate::tmux::source_file(&conf.to_string_lossy())
}

fn _install_tmux_bindings(install_dir: &Path) -> bool {
    _source_tmux_conf(&install_dir.join("tmux").join("enable.conf"))
}

fn _uninstall_tmux_bindings(install_dir: &Path) -> bool {
    _source_tmux_conf(&install_dir.join("tmux").join("disable.conf"))
}

/// True if *path* is a symlink pointing into the hive plugin installed tree.
///
/// No shipped plugin installs skills anymore; this guard remains so
/// `disable_plugin` can clean up legacy state entries (e.g. a retired
/// `code-review` install) without touching user-owned skill directories.
fn _is_plugin_managed_skill(path: &Path) -> bool {
    let is_link = fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if !is_link {
        return false;
    }
    let Ok(target) = fs::canonicalize(path) else {
        return false;
    };
    // canonicalize the root too: on macOS tempdirs, the resolved target is
    // /private/var/... while the raw root string is /var/...
    let root = _installed_root();
    let root = fs::canonicalize(&root).unwrap_or(root);
    target.starts_with(&root)
}

fn _substitute_hook_value(value: &Value, install_dir: &Path) -> Value {
    match value {
        Value::String(s) => {
            Value::String(s.replace("${HIVE_PLUGIN_ROOT}", &install_dir.to_string_lossy()))
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| _substitute_hook_value(item, install_dir))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), _substitute_hook_value(item, install_dir)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn _plugin_hook_defs(install_dir: &Path) -> Result<Map<String, Value>> {
    let path = install_dir.join("hooks").join("hooks.json");
    if !path.exists() {
        return Ok(Map::new());
    }
    let data: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    match _substitute_hook_value(&data, install_dir) {
        Value::Object(m) => Ok(m),
        _ => Err(anyhow!("hooks.json must be a JSON object")),
    }
}

/// Python truthiness for the `tmux` state flag.
fn _truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(Value::Number(n)) => n.as_f64().map_or(true, |f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn _string_paths(plugin_state: &Map<String, Value>, key: &str) -> Vec<PathBuf> {
    plugin_state
        .get(key)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

pub fn disable_plugin(name: &str, missing_ok: bool) -> Result<Value> {
    let mut state = _load_state();
    if !state.get("plugins").map_or(false, |v| v.is_object()) {
        state.insert("plugins".to_string(), Value::Object(Map::new()));
    }
    let plugin_state = state
        .get("plugins")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get(name))
        .cloned();
    let Some(plugin_state) = plugin_state else {
        if missing_ok {
            return Ok(json!({"name": name, "enabled": false}));
        }
        return Err(anyhow!("plugin '{}' is not enabled", name));
    };
    let plugin_state = plugin_state.as_object().cloned().unwrap_or_default();

    for path in _string_paths(&plugin_state, "commands") {
        _remove_path(&path);
    }
    for skill_path in _string_paths(&plugin_state, "skills") {
        if skill_path.exists() && !_is_plugin_managed_skill(&skill_path) {
            continue;
        }
        _remove_path(&skill_path);
    }
    if let Some(Value::Object(hook_defs)) = plugin_state.get("hooks") {
        if !hook_defs.is_empty() {
            crate::core_hooks::remove_hook_groups(hook_defs)?;
        }
    }
    let install_root = plugin_state
        .get("installRoot")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    if let Some(install_root) = &install_root {
        if _truthy(plugin_state.get("tmux")) {
            _uninstall_tmux_bindings(install_root);
        }
        _remove_path(install_root);
    }
    if let Some(plugins) = state.get_mut("plugins").and_then(|v| v.as_object_mut()) {
        plugins.remove(name);
    }
    _save_state(&state)?;
    Ok(json!({"name": name, "enabled": false}))
}

pub const RETIRED_PLUGINS: [&str; 3] = ["cvim", "fork", "code-review"];

/// Disable any retired plugin left over from an older install.
///
/// cvim/fork were promoted into core hive; code-review was removed.
/// Called during `hive create` so users who previously enabled them have
/// their legacy command shims, skill symlinks, install root and state
/// entries cleaned up automatically.
pub fn cleanup_retired_plugins() -> Result<Vec<String>> {
    let state = _load_state();
    let names: Vec<String> = state
        .get("plugins")
        .and_then(|v| v.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let mut removed = Vec::new();
    for name in names {
        if RETIRED_PLUGINS.contains(&name.as_str()) {
            disable_plugin(&name, true)?;
            removed.push(name);
        }
    }
    Ok(removed)
}

pub fn enable_plugin(name: &str) -> Result<Value> {
    if RETIRED_PLUGINS.contains(&name) {
        return Err(anyhow!(
            "plugin '{}' is retired — nothing to enable. Run `hive create` to clean up any legacy plugin state.",
            name
        ));
    }
    let manifest = load_manifest(name)?;
    disable_plugin(name, true)?;

    let install_dir = _installed_root().join(name);
    _remove_path(&install_dir);
    if let Some(parent) = install_dir.parent() {
        fs::create_dir_all(parent)?;
    }
    _copy_tree(_plugin_resource_dir(name)?.files, &install_dir)?;

    _materialize_installed_commands(&install_dir)?;
    let hook_defs = _plugin_hook_defs(&install_dir)?;
    if !hook_defs.is_empty() {
        crate::core_hooks::merge_hook_groups(&hook_defs)?;
    }
    let has_tmux = _install_tmux_bindings(&install_dir);

    let mut state = _load_state();
    let mut plugin_state = Map::new();
    plugin_state.insert(
        "installRoot".to_string(),
        json!(install_dir.to_string_lossy()),
    );
    plugin_state.insert("hooks".to_string(), Value::Object(hook_defs));
    plugin_state.insert(
        "enabledAt".to_string(),
        json!(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)),
    );
    if has_tmux {
        plugin_state.insert("tmux".to_string(), Value::Bool(true));
    }
    if !state.get("plugins").map_or(false, |v| v.is_object()) {
        state.insert("plugins".to_string(), Value::Object(Map::new()));
    }
    state
        .get_mut("plugins")
        .and_then(|v| v.as_object_mut())
        .expect("plugins ensured above")
        .insert(name.to_string(), Value::Object(plugin_state));
    _save_state(&state)?;

    Ok(json!({
        "name": manifest.name,
        "description": manifest.description,
        "enabled": true,
        "installRoot": install_dir.to_string_lossy(),
        "tmux": has_tmux,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::TEST_ENV_LOCK;
    use std::os::unix::fs::symlink;
    use std::sync::MutexGuard;

    fn setup() -> (tempfile::TempDir, MutexGuard<'static, ()>) {
        let guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path().join(".hive"));
        std::env::set_var("CLAUDE_HOME", tmp.path().join(".claude"));
        std::env::set_var("CODEX_HOME", tmp.path().join(".codex"));
        (tmp, guard)
    }

    #[test]
    fn test_plugin_list_does_not_offer_retired_plugins() {
        let (_tmp, _guard) = setup();
        let names: Vec<String> = list_plugins()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap().to_string())
            .collect();
        assert!(!names.contains(&"cvim".to_string()));
        assert!(!names.contains(&"fork".to_string()));
        assert!(!names.contains(&"code-review".to_string()));
        assert!(names.contains(&"notify".to_string()));
    }

    #[test]
    fn test_plugin_enable_rejects_retired_plugins() {
        let (_tmp, _guard) = setup();
        for retired in ["cvim", "fork", "code-review"] {
            let err = enable_plugin(retired).unwrap_err();
            assert!(err.to_string().contains("retired"), "{}", err);
        }
    }

    #[test]
    fn test_init_retired_cleanup_removes_legacy_code_review_install() {
        let (tmp, _guard) = setup();
        let hive_home = tmp.path().join(".hive");
        let claude_home = tmp.path().join(".claude");
        let codex_home = tmp.path().join(".codex");

        // Legacy on-disk layout left behind by an old `hive plugin enable code-review`.
        let install_root = hive_home
            .join("plugins")
            .join("installed")
            .join("code-review");
        let skill_src = install_root.join("skills").join("code-review");
        fs::create_dir_all(&skill_src).unwrap();
        fs::write(skill_src.join("SKILL.md"), "legacy\n").unwrap();
        let mut plugin_links = Vec::new();
        for home in [&claude_home, &codex_home] {
            let link = home.join("skills").join("code-review");
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            symlink(&skill_src, &link).unwrap();
            plugin_links.push(link);
        }

        // A user-owned (non-symlink) skill listed in state must survive cleanup.
        let user_skill = claude_home.join("skills").join("review");
        fs::create_dir_all(&user_skill).unwrap();
        fs::write(
            user_skill.join("SKILL.md"),
            "---\nname: review\ndescription: user custom\n---\n",
        )
        .unwrap();

        let state_path = hive_home.join("plugins").join("state.json");
        let mut skills: Vec<String> = plugin_links
            .iter()
            .map(|link| link.to_string_lossy().to_string())
            .collect();
        skills.push(user_skill.to_string_lossy().to_string());
        fs::write(
            &state_path,
            serde_json::to_string(&json!({
                "plugins": {
                    "code-review": {
                        "installRoot": install_root.to_string_lossy(),
                        "skills": skills,
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let removed = cleanup_retired_plugins().unwrap();

        assert_eq!(removed, vec!["code-review".to_string()]);
        for link in &plugin_links {
            assert!(!link.exists() && fs::symlink_metadata(link).is_err());
        }
        assert!(!install_root.exists());
        assert!(user_skill.is_dir());
        assert!(!fs::symlink_metadata(&user_skill)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(fs::read_to_string(user_skill.join("SKILL.md"))
            .unwrap()
            .starts_with("---\nname: review"));
        let state: Value = serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
        assert!(state["plugins"].get("code-review").is_none());
    }
}
