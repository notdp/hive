//! Where hive lives on this machine: its home directory, its own
//! binary, and the file primitives every layer writes with. Depends on
//! nothing in the crate.

use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

/// `$HIVE_HOME`, read per call so tests can redirect it (nextest runs one
/// process per test).
pub fn hive_home() -> PathBuf {
    let home = std::env::var("HIVE_HOME")
        .unwrap_or_else(|_| format!("{}/.hive", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(home)
}

/// The hive binary that tmux hooks, the flow dock and the cvim asset call
/// back into. HIVE_BIN overrides `current_exe` — `hive cvim` exports it for
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
