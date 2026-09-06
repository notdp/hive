//! Optional plugin enable/disable lifecycle, plus the local marketplace
//! that ships the Claude/Codex plugin payload with the binary.
//!
//! Three source trees are embedded at compile time via `include_str!`:
//! `crates/hive/assets/plugins/` (the shipped hive plugins, `BUILTIN_PLUGINS`),
//! `crates/hive/assets/marketplace/` (the two marketplace manifests), and
//! the repo-level `plugins/hive/` (the plugin payload: its two manifests,
//! the skill with its references, and the `hive-node` agent). Enabled state
//! lives on disk under `$HIVE_HOME/plugins/`. The one shipped
//! plugin (`notify`) is a manifest-only toggle the hived reads through
//! `is_plugin_enabled`, so enabling copies the manifest under
//! `installed/<name>/` and records the state entry, nothing more.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// local marketplace (skills ride the binary)
// ---------------------------------------------------------------------------

// The Claude/Codex marketplace payload is embedded here and materialized under
// `$HIVE_HOME/core_assets/marketplace/` heal-on-drift, like the cvim
// toolkit. `hive plugin sync` is the command-source entry Claude
// re-runs once per session: it heals the tree and prints the payload
// directory, so the installed skill content always matches this binary —
// there is no remote update channel and no version bookkeeping on the Claude
// side. The codex marketplace is a directory source over the same payload;
// its cache is keyed by the manifest version, which tracks the crate version.

const _MP_CLAUDE: &str = include_str!("../assets/marketplace/claude-marketplace.json");
const _MP_CODEX: &str = include_str!("../assets/marketplace/codex-marketplace.json");
const _PAYLOAD: &[(&str, &str, bool)] = &[
    (
        ".claude-plugin/plugin.json",
        include_str!("../../../plugins/hive/.claude-plugin/plugin.json"),
        false,
    ),
    (
        ".codex-plugin/plugin.json",
        include_str!("../../../plugins/hive/.codex-plugin/plugin.json"),
        false,
    ),
    (
        "skills/hive/SKILL.md",
        include_str!("../../../plugins/hive/skills/hive/SKILL.md"),
        false,
    ),
    (
        "skills/hive/references/orchestration.md",
        include_str!("../../../plugins/hive/skills/hive/references/orchestration.md"),
        false,
    ),
    (
        "skills/hive/references/worktree.md",
        include_str!("../../../plugins/hive/skills/hive/references/worktree.md"),
        false,
    ),
    (
        "agents/hive-node.md",
        include_str!("../../../plugins/hive/agents/hive-node.md"),
        false,
    ),
];

/// Relative payload location inside the marketplace tree: the codex
/// marketplace's directory source points at it, and `hive plugin sync`
/// prints it for Claude's command source.
const _PAYLOAD_SUBDIR: &str = "codex/plugins/hive";

/// Codex has no command-source plugins and its plugin hooks sit behind a
/// hook-review dialog, so the codex plugin ships no hooks at all; lockstep
/// is re-established from hive's own codex launch path instead — before the
/// engine starts, so the session being launched already loads the refreshed
/// plugin. When the codex plugin cache has no entry for this binary's
/// version, heal the local marketplace and re-add (re-adding is codex's
/// upgrade verb). A codex that never registered the marketplace fails the
/// add silently — setup stays explicit.
pub fn ensure_codex_plugin_current() {
    let home = std::env::var("CODEX_HOME")
        .unwrap_or_else(|_| format!("{}/.codex", std::env::var("HOME").unwrap_or_default()));
    let cache = Path::new(&home)
        .join("plugins/cache/hive/hive")
        .join(env!("CARGO_PKG_VERSION"));
    if cache.is_dir() || materialize_marketplace().is_err() {
        return;
    }
    let _ = std::process::Command::new("codex")
        .args(["plugin", "add", "hive@hive"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Write the embedded marketplace tree under
/// `$HIVE_HOME/core_assets/marketplace/` (heal-on-drift) and return the
/// payload plugin directory.
pub fn materialize_marketplace() -> Result<PathBuf> {
    let root = crate::paths::hive_home()
        .join("core_assets")
        .join("marketplace");
    let mut files: Vec<(String, &str, bool)> = vec![
        (
            "claude/.claude-plugin/marketplace.json".to_string(),
            _MP_CLAUDE,
            false,
        ),
        (
            "codex/.claude-plugin/marketplace.json".to_string(),
            _MP_CODEX,
            false,
        ),
    ];
    for (rel, content, executable) in _PAYLOAD {
        files.push((format!("{_PAYLOAD_SUBDIR}/{rel}"), content, *executable));
    }
    let borrowed: Vec<(&str, &str, bool)> = files
        .iter()
        .map(|(rel, content, executable)| (rel.as_str(), *content, *executable))
        .collect();
    crate::assets::materialize_asset_tree(&root, &borrowed)?;
    Ok(root.join(_PAYLOAD_SUBDIR))
}

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

// The shipped plugins, embedded from `crates/hive/assets/plugins/<name>/`.
static BUILTIN_PLUGINS: &[BuiltinPlugin] = &[BuiltinPlugin {
    name: "notify",
    files: &[(
        "plugin.json",
        include_str!("../assets/plugins/notify/plugin.json"),
    )],
}];

fn state_path() -> PathBuf {
    crate::paths::hive_home().join("plugins").join("state.json")
}

fn installed_root() -> PathBuf {
    crate::paths::hive_home().join("plugins").join("installed")
}

fn default_state() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("plugins".to_string(), Value::Object(Map::new()));
    m
}

fn load_state() -> Map<String, Value> {
    let path = state_path();
    if !path.exists() {
        return default_state();
    }
    match fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    {
        Some(Value::Object(m)) => m,
        _ => default_state(),
    }
}

fn save_json_file(path: &Path, data: &Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format!("{}\n", serde_json::to_string_pretty(data)?))?;
    Ok(())
}

fn save_state(data: &Map<String, Value>) -> Result<()> {
    save_json_file(&state_path(), data)
}

fn remove_path(path: &Path) {
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

/// Write the embedded plugin files under `dst`.
fn copy_tree(files: &[(&str, &str)], dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for (rel, content) in files {
        let target = dst.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, content)?;
    }
    Ok(())
}

fn plugin_resource_dir(name: &str) -> Result<&'static BuiltinPlugin> {
    BUILTIN_PLUGINS
        .iter()
        .find(|p| p.name == name && p.files.iter().any(|(rel, _)| *rel == "plugin.json"))
        .ok_or_else(|| anyhow!("plugin '{}' not found", name))
}

pub fn load_manifest(name: &str) -> Result<PluginManifest> {
    let plugin = plugin_resource_dir(name)?;
    let raw = plugin
        .files
        .iter()
        .find(|(rel, _)| *rel == "plugin.json")
        .map(|(_, content)| *content)
        .expect("checked by plugin_resource_dir");
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
    load_state()
        .get("plugins")
        .and_then(|v| v.as_object())
        .map(|m| m.contains_key(name))
        .unwrap_or(false)
}

pub fn list_plugins() -> Result<Vec<Value>> {
    let state = load_state();
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

pub fn disable_plugin(name: &str, missing_ok: bool) -> Result<Value> {
    let mut state = load_state();
    if !state.get("plugins").is_some_and(|v| v.is_object()) {
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

    let install_root = plugin_state
        .get("installRoot")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    if let Some(install_root) = &install_root {
        remove_path(install_root);
    }
    if let Some(plugins) = state.get_mut("plugins").and_then(|v| v.as_object_mut()) {
        plugins.remove(name);
    }
    save_state(&state)?;
    Ok(json!({"name": name, "enabled": false}))
}

pub fn enable_plugin(name: &str) -> Result<Value> {
    let manifest = load_manifest(name)?;
    disable_plugin(name, true)?;

    let install_dir = installed_root().join(name);
    remove_path(&install_dir);
    if let Some(parent) = install_dir.parent() {
        fs::create_dir_all(parent)?;
    }
    copy_tree(plugin_resource_dir(name)?.files, &install_dir)?;

    let mut state = load_state();
    let mut plugin_state = Map::new();
    plugin_state.insert(
        "installRoot".to_string(),
        json!(install_dir.to_string_lossy()),
    );
    plugin_state.insert(
        "enabledAt".to_string(),
        json!(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)),
    );
    if !state.get("plugins").is_some_and(|v| v.is_object()) {
        state.insert("plugins".to_string(), Value::Object(Map::new()));
    }
    state
        .get_mut("plugins")
        .and_then(|v| v.as_object_mut())
        .expect("plugins ensured above")
        .insert(name.to_string(), Value::Object(plugin_state));
    save_state(&state)?;

    Ok(json!({
        "name": manifest.name,
        "description": manifest.description,
        "enabled": true,
        "installRoot": install_dir.to_string_lossy(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;
    use std::os::unix::fs::PermissionsExt;

    fn setup() -> (tempfile::TempDir, EnvGuard) {
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("HIVE_HOME", tmp.path().join(".hive"));
        env.set("CLAUDE_HOME", tmp.path().join(".claude"));
        env.set("CODEX_HOME", tmp.path().join(".codex"));
        (tmp, env)
    }

    #[test]
    fn test_materialize_marketplace_writes_and_heals_the_tree() {
        let (_tmp, _guard) = setup();
        let payload = materialize_marketplace().unwrap();
        assert!(payload.ends_with("core_assets/marketplace/codex/plugins/hive"));
        assert!(payload.join(".claude-plugin/plugin.json").is_file());
        assert!(payload.join("skills/hive/SKILL.md").is_file());

        // both marketplace manifests parse; the claude one is a command source
        let root = payload
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let claude: Value = serde_json::from_str(
            &fs::read_to_string(root.join("claude/.claude-plugin/marketplace.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(claude["plugins"][0]["source"]["source"], json!("command"));
        assert_eq!(
            claude["plugins"][0]["source"]["command"],
            json!("hive plugin sync")
        );
        let codex: Value = serde_json::from_str(
            &fs::read_to_string(root.join("codex/.claude-plugin/marketplace.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(codex["plugins"][0]["source"], json!("./plugins/hive"));

        // the payload manifest version matches the crate version (codex cache key)
        let manifest: Value = serde_json::from_str(
            &fs::read_to_string(payload.join(".claude-plugin/plugin.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["version"], json!(env!("CARGO_PKG_VERSION")));

        // the plugin ships no hooks at all: codex gates plugin hooks behind a
        // review dialog, and the claude side needs none — sync is the command
        // source, presence hints died with the last hook
        assert!(!payload.join("hooks").exists());
        assert!(!payload.join("scripts").exists());

        // heal-on-drift: a tampered file is rewritten on the next call
        let skill = payload.join("skills/hive/SKILL.md");
        fs::write(&skill, "tampered").unwrap();
        materialize_marketplace().unwrap();
        assert_ne!(fs::read_to_string(&skill).unwrap(), "tampered");
    }

    #[test]
    fn test_ensure_codex_plugin_current_readds_only_on_version_drift() {
        let (tmp, mut env) = setup();
        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let log = tmp.path().join("codex.log");
        let stub = bin.join("codex");
        fs::write(
            &stub,
            format!("#!/bin/sh\necho \"$*\" >> {}\n", log.display()),
        )
        .unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        env.set("PATH", format!("{}:/usr/bin:/bin", bin.display()));

        // cache missing -> marketplace healed + one re-add
        ensure_codex_plugin_current();
        assert_eq!(fs::read_to_string(&log).unwrap(), "plugin add hive@hive\n");
        assert!(crate::paths::hive_home()
            .join("core_assets/marketplace/codex/plugins/hive/.codex-plugin/plugin.json")
            .is_file());

        // cache present for this binary's version -> no-op
        fs::create_dir_all(
            PathBuf::from(std::env::var("CODEX_HOME").unwrap())
                .join("plugins/cache/hive/hive")
                .join(env!("CARGO_PKG_VERSION")),
        )
        .unwrap();
        ensure_codex_plugin_current();
        assert_eq!(fs::read_to_string(&log).unwrap(), "plugin add hive@hive\n");
    }
}
