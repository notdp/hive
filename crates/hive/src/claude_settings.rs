//! The one key hive sets in Claude Code's own user settings.
//!
//! Claude Code gates function hooks (the Claude Mods primitive, 2.1.278)
//! behind `CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1`. An engine hive spawns
//! gets it as a `--settings` flag (`claude_bg::FUNCTION_HOOKS_SETTINGS`);
//! a desktop session is not spawned by hive, and reads the switch from
//! `~/.claude/settings.json` under `env` — so `hive plugin setup`, the
//! explicit registration step a human runs once, writes that key there,
//! and `hive doctor` reports whether it is present. The file is Claude's:
//! it is read and written as a `serde_json::Value`, every other key kept,
//! and replaced atomically. Once Claude Code ships mods without the
//! switch this module goes, per the no-compatibility rule.

use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde_json::{Map, Value};

pub const FUNCTION_HOOKS_ENV: &str = "CLAUDE_CODE_ENABLE_FUNCTION_HOOKS";

/// `<claude config dir>/settings.json`, the user-level settings file.
pub fn settings_path() -> PathBuf {
    crate::adapters::claude_sessions::config_dir().join("settings.json")
}

fn read_settings(path: &PathBuf) -> Result<Map<String, Value>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(anyhow!("cannot read {}: {e}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(anyhow!("{} is not a JSON object", path.display())),
        Err(e) => Err(anyhow!("{} is not valid JSON: {e}", path.display())),
    }
}

/// Whether the settings file switches function hooks on (`env` holds the
/// key with the value `"1"`). False when the file is missing or unreadable.
pub fn function_hooks_enabled() -> bool {
    read_settings(&settings_path())
        .ok()
        .and_then(|s| {
            s.get("env")?
                .get(FUNCTION_HOOKS_ENV)
                .map(|v| v == &Value::from("1"))
        })
        .unwrap_or(false)
}

/// Switch function hooks on in the settings file; true when the file
/// changed, false when the key was already set. A file that is not a JSON
/// object is left alone and reported.
pub fn enable_function_hooks() -> Result<bool> {
    let path = settings_path();
    let mut settings = read_settings(&path)?;
    let env = match settings.get_mut("env") {
        Some(Value::Object(env)) => env,
        Some(_) => return Err(anyhow!("{}: `env` is not an object", path.display())),
        None => {
            settings.insert("env".to_string(), Value::Object(Map::new()));
            match settings.get_mut("env") {
                Some(Value::Object(env)) => env,
                _ => unreachable!("just inserted"),
            }
        }
    };
    if env.get(FUNCTION_HOOKS_ENV) == Some(&Value::from("1")) {
        return Ok(false);
    }
    env.insert(FUNCTION_HOOKS_ENV.to_string(), Value::from("1"));
    let text = serde_json::to_string_pretty(&Value::Object(settings))
        .map_err(|e| anyhow!("cannot serialize settings: {e}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow!("cannot create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let written = fs::write(&tmp, format!("{text}\n")).and_then(|_| fs::rename(&tmp, &path));
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(anyhow!("cannot write {}: {e}", path.display()));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;

    fn lane() -> (EnvGuard, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let mut guard = EnvGuard::new();
        guard.set("CLAUDE_CONFIG_DIR", tmp.path().to_str().unwrap());
        guard.remove("CLAUDE_HOME");
        (guard, tmp)
    }

    #[test]
    fn test_enable_creates_the_file_and_is_idempotent() {
        let (_guard, tmp) = lane();
        assert!(!function_hooks_enabled());
        assert!(enable_function_hooks().unwrap());
        assert!(function_hooks_enabled());
        assert!(
            !enable_function_hooks().unwrap(),
            "already set: nothing to write"
        );
        let text = fs::read_to_string(tmp.path().join("settings.json")).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["env"][FUNCTION_HOOKS_ENV], Value::from("1"));
        assert!(fs::read_dir(tmp.path())
            .unwrap()
            .all(|e| { !e.unwrap().file_name().to_string_lossy().ends_with(".tmp") }));
    }

    #[test]
    fn test_enable_keeps_every_other_key_and_their_order() {
        let (_guard, tmp) = lane();
        let path = tmp.path().join("settings.json");
        fs::write(
            &path,
            r#"{"model":"opus","env":{"FOO":"bar"},"permissions":{"allow":["Bash(ls *)"]},"hooks":{}}"#,
        )
        .unwrap();
        assert!(enable_function_hooks().unwrap());
        let text = fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["model", "env", "permissions", "hooks"]);
        assert_eq!(v["env"]["FOO"], Value::from("bar"));
        assert_eq!(v["env"][FUNCTION_HOOKS_ENV], Value::from("1"));
        assert_eq!(v["permissions"]["allow"][0], Value::from("Bash(ls *)"));
    }

    #[test]
    fn test_enable_leaves_a_file_that_is_not_an_object_alone() {
        let (_guard, tmp) = lane();
        let path = tmp.path().join("settings.json");
        fs::write(&path, "[1, 2]").unwrap();
        assert!(enable_function_hooks().is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "[1, 2]");
        fs::write(&path, r#"{"env": "not an object"}"#).unwrap();
        assert!(enable_function_hooks().is_err());
        assert!(!function_hooks_enabled());
    }
}
