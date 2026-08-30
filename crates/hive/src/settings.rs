//! User-level settings stored at `$HIVE_HOME/settings.json`.
//!
//! Dot-path keys (e.g. `spawn.defaultCli`) map to nested JSON. Missing file or
//! unreadable JSON returns an empty map — settings are entirely optional.

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use serde_json::{Map, Value};

fn _hive_home() -> PathBuf {
    let home = std::env::var("HIVE_HOME")
        .unwrap_or_else(|_| format!("{}/.hive", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(home)
}

fn _settings_path() -> PathBuf {
    _hive_home().join("settings.json")
}

fn key_parts(key: &str) -> Vec<&str> {
    key.split('.').filter(|p| !p.is_empty()).collect()
}

pub fn load_user_settings() -> Map<String, Value> {
    let text = match fs::read_to_string(_settings_path()) {
        Ok(t) => t,
        Err(_) => return Map::new(),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(o)) => o,
        _ => Map::new(),
    }
}

/// The value at *key*, or None when any path segment is missing
/// (Python's `default` argument maps to `Option` here).
pub fn get_setting(key: &str) -> Option<Value> {
    let parts = key_parts(key);
    if parts.is_empty() {
        return None;
    }
    let mut node = Value::Object(load_user_settings());
    for part in parts {
        node = match node {
            Value::Object(mut o) => o.remove(part)?,
            _ => return None,
        };
    }
    Some(node)
}

pub fn set_setting(key: &str, value: Value) -> Result<()> {
    let parts = key_parts(key);
    if parts.is_empty() {
        bail!("empty key");
    }
    let mut data = load_user_settings();
    {
        let mut node: &mut Map<String, Value> = &mut data;
        for part in &parts[..parts.len() - 1] {
            if !node.get(*part).map_or(false, Value::is_object) {
                node.insert((*part).to_string(), Value::Object(Map::new()));
            }
            node = node
                .get_mut(*part)
                .and_then(Value::as_object_mut)
                .expect("just inserted an object");
        }
        node.insert(parts[parts.len() - 1].to_string(), value);
    }
    _write_atomic(&data)
}

pub fn unset_setting(key: &str) -> Result<bool> {
    let parts = key_parts(key);
    if parts.is_empty() {
        return Ok(false);
    }
    let mut data = load_user_settings();
    {
        let mut node: &mut Map<String, Value> = &mut data;
        for part in &parts[..parts.len() - 1] {
            node = match node.get_mut(*part).and_then(Value::as_object_mut) {
                Some(o) => o,
                None => return Ok(false),
            };
        }
        if node.remove(parts[parts.len() - 1]).is_none() {
            return Ok(false);
        }
    }
    _write_atomic(&data)?;
    Ok(true)
}

fn _write_atomic(data: &Map<String, Value>) -> Result<()> {
    let path = _settings_path();
    let parent = path.parent().context("settings path has no parent")?;
    fs::create_dir_all(parent)?;
    let (mut file, tmp) = crate::registry::mkstemp_in(parent, ".settings.", ".json.tmp")?;
    // json.dump(..., indent=2, sort_keys=True). (Python escapes non-ASCII
    // here; serde writes UTF-8 — readers all parse JSON, so the documents
    // are equivalent.)
    let result = (|| -> Result<()> {
        let sorted = crate::registry::sort_keys(&Value::Object(data.clone()));
        let mut text = serde_json::to_string_pretty(&sorted)?;
        text.push('\n');
        file.write_all(text.as_bytes())?;
        fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::TEST_ENV_LOCK;
    use serde_json::json;
    use std::sync::MutexGuard;

    fn setup() -> (tempfile::TempDir, MutexGuard<'static, ()>) {
        let guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path());
        (tmp, guard)
    }

    // No Python unit tests exist for settings (only cli-level config tests);
    // these are minimal self-checks of the dot-path logic.

    #[test]
    fn test_set_get_unset_round_trip() {
        let (_tmp, _guard) = setup();
        set_setting("spawn.defaultCli", json!("claude")).unwrap();
        set_setting("tunables.delay", json!(42)).unwrap();
        assert_eq!(get_setting("spawn.defaultCli"), Some(json!("claude")));
        assert_eq!(get_setting("tunables.delay"), Some(json!(42)));
        assert_eq!(get_setting("spawn"), Some(json!({"defaultCli": "claude"})));
        assert_eq!(get_setting("spawn.missing"), None);
        assert_eq!(get_setting(""), None);

        assert!(unset_setting("spawn.defaultCli").unwrap());
        assert_eq!(get_setting("spawn.defaultCli"), None);
        assert!(!unset_setting("spawn.defaultCli").unwrap());
        assert!(!unset_setting("missing.key").unwrap());
    }

    #[test]
    fn test_missing_or_corrupt_settings_load_empty() {
        let (tmp, _guard) = setup();
        assert!(load_user_settings().is_empty());
        fs::write(tmp.path().join("settings.json"), "not json").unwrap();
        assert!(load_user_settings().is_empty());
        assert_eq!(get_setting("any.key"), None);
        // set_setting replaces a non-dict intermediate with a fresh object
        set_setting("a", json!("leaf")).unwrap();
        set_setting("a.b", json!(1)).unwrap();
        assert_eq!(get_setting("a.b"), Some(json!(1)));
        assert!(set_setting("", json!(1)).is_err());
    }
}
