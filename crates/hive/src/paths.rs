//! Where hive lives on this machine: its home directory, its own
//! binary, and the file primitives every layer writes with. Depends on
//! nothing in the crate.

use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

/// `$HIVE_HOME`, read per call so tests can redirect it (nextest runs one
/// process per test).
pub fn hive_home() -> PathBuf {
    let home = std::env::var("HIVE_HOME")
        .unwrap_or_else(|_| format!("{}/.hive", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(home)
}

/// The hive binary that tmux hooks and the cvim asset call back into. HIVE_BIN overrides `current_exe` — `hive cvim` exports it for
/// the bash asset, and integration tests (whose current_exe is the test
/// harness) point hooks at the real binary with it.
pub fn self_exe() -> String {
    let overridden = std::env::var("HIVE_BIN").unwrap_or_default();
    if !overridden.is_empty() {
        return overridden;
    }
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hive".to_string())
}

/// Expand a leading `~` (bare `~` and `~/...` forms).
pub fn expanduser(path: &str) -> String {
    if path == "~" {
        return std::env::var("HOME").unwrap_or_default();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

/// An exclusive 0600 temp file in *dir*, for atomic rename-into-place.
pub fn mkstemp_in(dir: &Path, prefix: &str, suffix: &str) -> Result<(std::fs::File, PathBuf)> {
    for attempt in 0..128u32 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let candidate = dir.join(format!(
            "{prefix}{}-{nanos:x}-{attempt}{suffix}",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(f) => return Ok((f, candidate)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("could not create temp file in {}", dir.display());
}

/// An exclusive 0700 directory in *dir*, named `<prefix><random hex>`:
/// the private staging area a caller unpacks into before committing one
/// file out of it. The caller removes it.
pub fn mkdtemp_in(dir: &Path, prefix: &str) -> Result<PathBuf> {
    for _ in 0..128u32 {
        let suffix: String = crate::naming::os_random_bytes(8)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let candidate = dir.join(format!("{prefix}{suffix}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("could not create temp dir in {}", dir.display());
}

/// `$HIVE_HOME/state/locks`, created on demand: the one place cross-process
/// flocks that belong to no workspace live.
pub fn locks_dir() -> Result<PathBuf> {
    let dir = hive_home().join("state").join("locks");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The current directory as a string, empty when it cannot be read.
pub(crate) fn getcwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn test_mkdtemp_in_makes_a_private_directory_per_call() {
        let tmp = tempfile::tempdir().unwrap();
        let first = mkdtemp_in(tmp.path(), "hive-update-").unwrap();
        let second = mkdtemp_in(tmp.path(), "hive-update-").unwrap();
        assert_ne!(first, second);
        for dir in [&first, &second] {
            assert_eq!(dir.parent().unwrap(), tmp.path());
            let meta = std::fs::symlink_metadata(dir).unwrap();
            assert!(meta.is_dir());
            assert_eq!(meta.permissions().mode() & 0o777, 0o700);
            assert!(dir
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("hive-update-"));
        }
    }

    #[test]
    fn test_mkdtemp_in_reports_a_missing_parent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(mkdtemp_in(&tmp.path().join("absent"), "x-").is_err());
    }
}
