//! The local marketplace that ships the Claude/Codex plugin payload with
//! the binary.
//!
//! Two source trees are embedded at compile time via `include_str!`:
//! `crates/hive/assets/marketplace/` (the two marketplace manifests) and
//! the repo-level `plugins/hive/` (the plugin payload: its two manifests,
//! the skill with its references, and the `hive-node` agent).

use std::path::{Path, PathBuf};

use anyhow::Result;

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

const MP_CLAUDE: &str = include_str!("../assets/marketplace/claude-marketplace.json");
const MP_CODEX: &str = include_str!("../assets/marketplace/codex-marketplace.json");
const PAYLOAD: &[(&str, &str, bool)] = &[
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
const PAYLOAD_SUBDIR: &str = "codex/plugins/hive";

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
            MP_CLAUDE,
            false,
        ),
        (
            "codex/.claude-plugin/marketplace.json".to_string(),
            MP_CODEX,
            false,
        ),
    ];
    for (rel, content, executable) in PAYLOAD {
        files.push((format!("{PAYLOAD_SUBDIR}/{rel}"), content, *executable));
    }
    let borrowed: Vec<(&str, &str, bool)> = files
        .iter()
        .map(|(rel, content, executable)| (rel.as_str(), *content, *executable))
        .collect();
    crate::assets::materialize_asset_tree(&root, &borrowed)?;
    Ok(root.join(PAYLOAD_SUBDIR))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;
    use serde_json::{json, Value};
    use std::fs;
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
