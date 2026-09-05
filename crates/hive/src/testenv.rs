//! Process-env isolation for the crate's unit tests.
//!
//! Env vars are process-global. `cargo nextest run` gives every test its own
//! process, but plain `cargo test` runs them as threads of one process, so a
//! test that rewrites a variable holds [`EnvGuard`]: one crate-wide lock,
//! taken for the guard's lifetime, plus save-and-restore of every variable
//! the guard touched, so a test's leftovers never leak into the next one.
//!
//! A guard is not reentrant: a helper that builds one must hand it back to
//! the test instead of dropping it, and a test holds at most one.

use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());

/// The engine-identity vars a headless or out-of-tmux test must not inherit
/// from the developer's shell: the tmux client and pane, the codex thread,
/// the grok session, the claude inbox socket.
pub(crate) const IDENTITY_VARS: [&str; 5] = [
    "TMUX",
    "TMUX_PANE",
    "CODEX_THREAD_ID",
    "GROK_SESSION_ID",
    "CLAUDE_CODE_MESSAGING_SOCKET",
];

/// The claude config-tree knobs (`claude_sessions::_config_dir`) plus the
/// inbox socket that names the current session.
pub(crate) const CLAUDE_VARS: [&str; 3] = [
    "CLAUDE_HOME",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_CODE_MESSAGING_SOCKET",
];

/// Holds the crate-wide env lock and puts every variable it touched back on
/// drop.
pub(crate) struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(String, Option<OsString>)>,
}

impl EnvGuard {
    /// Take the lock; nothing is touched until `set`/`remove`.
    pub(crate) fn new() -> Self {
        EnvGuard {
            _lock: LOCK.lock().unwrap_or_else(|e| e.into_inner()),
            saved: Vec::new(),
        }
    }

    /// Take the lock and unset every var in *vars* (restored on drop).
    pub(crate) fn cleared(vars: &[&str]) -> Self {
        let mut env = Self::new();
        for key in vars {
            env.remove(key);
        }
        env
    }

    fn remember(&mut self, key: &str) {
        if !self.saved.iter().any(|(k, _)| k == key) {
            self.saved.push((key.to_string(), std::env::var_os(key)));
        }
    }

    pub(crate) fn set(&mut self, key: &str, value: impl AsRef<OsStr>) {
        self.remember(key);
        std::env::set_var(key, value);
    }

    pub(crate) fn remove(&mut self, key: &str) {
        self.remember(key);
        std::env::remove_var(key);
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_env_guard_restores_every_touched_var_on_drop() {
        // Baseline for this process: A set, B unset (names nothing else reads).
        std::env::set_var("HIVE_TESTENV_A", "before");
        std::env::remove_var("HIVE_TESTENV_B");
        {
            let mut env = EnvGuard::new();
            env.set("HIVE_TESTENV_A", "during");
            env.set("HIVE_TESTENV_B", "during");
            env.remove("HIVE_TESTENV_A"); // the first-seen value is what restores
            assert_eq!(std::env::var_os("HIVE_TESTENV_A"), None);
            assert_eq!(std::env::var("HIVE_TESTENV_B").unwrap(), "during");
        }
        assert_eq!(std::env::var("HIVE_TESTENV_A").unwrap(), "before");
        assert_eq!(std::env::var_os("HIVE_TESTENV_B"), None);
        std::env::remove_var("HIVE_TESTENV_A");
    }

    #[test]
    fn test_cleared_unsets_the_listed_vars_and_restores_them() {
        std::env::set_var("HIVE_TESTENV_C", "keep");
        std::env::remove_var("HIVE_TESTENV_D");
        {
            let env = EnvGuard::cleared(&["HIVE_TESTENV_C", "HIVE_TESTENV_D"]);
            assert_eq!(std::env::var_os("HIVE_TESTENV_C"), None);
            assert_eq!(std::env::var_os("HIVE_TESTENV_D"), None);
            assert_eq!(env.saved.len(), 2);
        }
        assert_eq!(std::env::var("HIVE_TESTENV_C").unwrap(), "keep");
        assert_eq!(std::env::var_os("HIVE_TESTENV_D"), None);
        std::env::remove_var("HIVE_TESTENV_C");
    }
}
