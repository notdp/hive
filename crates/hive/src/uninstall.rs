//! Remove the running installation. Team refusal and binary validation happen
//! before mutations; cleanup failures accumulate so later steps still run.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Result};

pub(crate) fn run(target: &Path, force: bool, purge: bool) -> Result<bool> {
    let before = crate::paths::regular_binary_metadata(target).map_err(anyhow::Error::msg)?;
    let entries = crate::registry::list_entries();
    let names: Vec<&str> = entries
        .iter()
        .filter_map(|entry| entry.get("team").and_then(|v| v.as_str()))
        .collect();
    if !force && !names.is_empty() {
        bail!(
            "teams still registered: {}. Run hive delete <team> --down for each team, or use --force",
            names.join(", ")
        );
    }

    let home = crate::paths::hive_home();
    if purge && (home.as_os_str().is_empty() || home.parent().is_none()) {
        bail!("refusing to purge {}: not a hive directory", home.display());
    }
    if purge && std::env::var_os("HOME").is_some_and(|user_home| home == PathBuf::from(user_home)) {
        bail!("refusing to purge the user home {}", home.display());
    }

    let binary = target.canonicalize()?;
    let containing_home = if purge && fs::symlink_metadata(&home).is_ok_and(|m| m.is_dir()) {
        home.canonicalize()
            .ok()
            .filter(|root| binary.starts_with(root))
    } else {
        None
    };

    let mut success = true;
    for name in names {
        success &= report(
            &format!("team {name}"),
            crate::team::delete_team(name, "", false, true),
        );
    }
    success &= remove_plugins();
    success &= report(
        "codex daemon",
        crate::adapters::codex_app_server::uninstall_daemon().map_err(anyhow::Error::msg),
    );
    if purge {
        success &= report(
            &format!("data {}", home.display()),
            match &containing_home {
                Some(root) => purge_except_binary(root, &binary),
                None => remove_if_present(&home, true),
            },
        );
    } else {
        println!(
            "uninstall: data {}: kept (use --purge to remove)",
            home.display()
        );
    }
    let receipt = receipt_path();
    success &= report(
        &format!("receipt {}", receipt.display()),
        remove_if_present(&receipt, false),
    );

    // Do not unlink a replacement that landed while the CLIs were cleaning up.
    let remove_binary = (|| {
        let after = crate::paths::regular_binary_metadata(target).map_err(anyhow::Error::msg)?;
        if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
            bail!(
                "{} changed during uninstall; left in place",
                target.display()
            );
        }
        fs::remove_file(target)?;
        if let Some(root) = &containing_home {
            let mut parent = binary.parent();
            while let Some(dir) = parent.filter(|dir| dir.starts_with(root)) {
                fs::remove_dir(dir)?;
                parent = dir.parent();
            }
        }
        Ok(())
    })();
    success &= report(&format!("binary {}", target.display()), remove_binary);
    println!("uninstall: shell: remove the hive shell-init line from your shell rc file manually");
    Ok(success)
}

/// A custom installation may put the binary inside HIVE_HOME. Keep that
/// file and its parents until the final unlink, even when purging the data.
fn purge_except_binary(root: &Path, binary: &Path) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path == binary {
            continue;
        }
        if binary.starts_with(&path) {
            purge_except_binary(&path, binary)?;
        } else {
            remove_if_present(&path, entry.file_type()?.is_dir())?;
        }
    }
    Ok(())
}

fn receipt_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"))
        .join("hive/hive-receipt.json")
}

fn remove_if_present(path: &Path, directory: bool) -> Result<()> {
    let result = if directory {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn report(label: &str, result: Result<()>) -> bool {
    match result {
        Ok(()) => {
            println!("uninstall: {label}: ok");
            true
        }
        Err(error) => {
            println!("uninstall: {label}: failed ({error})");
            false
        }
    }
}

fn remove_plugins() -> bool {
    let mut success = true;
    for (cli, remove, plugin_absent, marketplace_absent) in [
        (
            "claude",
            "uninstall",
            "Plugin \"hive@hive\" not found in installed plugins",
            "Marketplace 'hive' not found",
        ),
        (
            "codex",
            "remove",
            "",
            "marketplace `hive` is not configured or installed",
        ),
    ] {
        if !Command::new("sh")
            .args(["-c", &format!("command -v {cli} >/dev/null 2>&1")])
            .status()
            .is_ok_and(|s| s.success())
        {
            println!("uninstall: {cli}: not on PATH, skipped");
            continue;
        }
        let scope = if cli == "claude" {
            &["--scope", "user"][..]
        } else {
            &[]
        };
        let mut plugin = vec!["plugin", remove, "hive@hive"];
        plugin.extend(scope);
        success &= report(
            &format!("{cli} plugin"),
            remove_registration(cli, &plugin, plugin_absent),
        );
        let mut marketplace = vec!["plugin", "marketplace", "remove", "hive"];
        marketplace.extend(scope);
        success &= report(
            &format!("{cli} marketplace"),
            remove_registration(cli, &marketplace, marketplace_absent),
        );
    }
    success
}

fn remove_registration(cli: &str, args: &[&str], absent: &str) -> Result<()> {
    let output = Command::new(cli).args(args).output()?;
    if output.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    });
    let last = text.trim().lines().last().unwrap_or("");
    // These absence errors were checked against Claude 2.1.263 and Codex
    // 0.153.4. Unknown errors stay failures, including changed CLI wording.
    if output.status.code() == Some(1) && !absent.is_empty() && last.ends_with(absent) {
        return Ok(());
    }
    Err(anyhow!("{}: {last}", output.status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;
    use std::os::unix::fs::{symlink, PermissionsExt};

    struct Bed {
        env: EnvGuard,
        tmp: tempfile::TempDir,
        target: PathBuf,
        home: PathBuf,
    }

    impl Bed {
        fn new() -> Self {
            let mut env = EnvGuard::cleared(&crate::testenv::IDENTITY_VARS);
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().join("hive");
            for (key, leaf) in [
                ("HOME", "user"),
                ("HIVE_HOME", "hive"),
                ("CLAUDE_HOME", "claude"),
                ("CLAUDE_CONFIG_DIR", "claude"),
                ("CODEX_HOME", "codex"),
                ("XDG_CONFIG_HOME", "config"),
                ("GROK_HOME", "grok"),
            ] {
                let path = tmp.path().join(leaf);
                fs::create_dir_all(&path).unwrap();
                env.set(key, path);
            }
            env.set("PATH", tmp.path());
            let target = tmp.path().join("hive-binary");
            fs::write(&target, "binary").unwrap();
            fs::write(home.join("settings.json"), "{}").unwrap();
            let receipt = receipt_path();
            fs::create_dir_all(receipt.parent().unwrap()).unwrap();
            fs::write(receipt, "{}").unwrap();
            Self {
                env,
                tmp,
                target,
                home,
            }
        }

        fn cli(&mut self, name: &str, body: &str) {
            let bin = self.tmp.path().join("bin");
            fs::create_dir_all(&bin).unwrap();
            let path = bin.join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
            self.env
                .set("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        }
    }

    #[test]
    fn test_uninstall_keeps_data_by_default_and_removes_binary_and_receipt() {
        let bed = Bed::new();
        assert!(run(&bed.target, false, false).unwrap());
        assert!(bed.home.join("settings.json").exists());
        assert!(!bed.target.exists());
        assert!(!receipt_path().exists());
    }

    #[test]
    fn test_uninstall_purge_removes_hive_home_but_keeps_agent_settings() {
        let bed = Bed::new();
        let settings = bed.tmp.path().join("codex/config.toml");
        fs::write(&settings, "model = 'kept'").unwrap();
        assert!(run(&bed.target, false, true).unwrap());
        assert!(!bed.home.exists());
        assert!(!bed.target.exists());
        assert!(settings.exists());
    }

    #[test]
    fn test_uninstall_purge_defers_a_binary_installed_inside_hive_home() {
        let bed = Bed::new();
        let binary = bed.home.join("bin/hive");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::rename(&bed.target, &binary).unwrap();
        assert!(run(&binary, false, true).unwrap());
        assert!(!bed.home.exists());
        assert!(!receipt_path().exists());
    }

    #[test]
    fn test_uninstall_daemon_refuses_an_unrelated_recorded_process() {
        let mut bed = Bed::new();
        bed.env.set("PATH", "/usr/bin:/bin");
        let pidfile = crate::adapters::codex_app_server::shared_pidfile_path();
        fs::create_dir_all(pidfile.parent().unwrap()).unwrap();
        fs::write(&pidfile, std::process::id().to_string()).unwrap();
        // Losing the identity check would signal this test process, never
        // a developer's daemon or another test's process.
        assert!(crate::adapters::codex_app_server::uninstall_daemon().is_err());
        assert!(pidfile.exists());
    }

    #[test]
    fn test_uninstall_refuses_registered_teams_before_cleanup() {
        let bed = Bed::new();
        crate::registry::record_team("probe", "", "100", &[], "").unwrap();
        let error = run(&bed.target, false, true).unwrap_err().to_string();
        assert!(
            error.contains("probe") && error.contains("--down"),
            "{error}"
        );
        assert!(bed.target.exists());
        assert!(receipt_path().exists());
        assert!(bed.home.join("settings.json").exists());
        assert!(crate::registry::load("probe").is_some());
    }

    #[test]
    fn test_uninstall_refuses_symlink_before_cleanup() {
        let bed = Bed::new();
        let link = bed.tmp.path().join("link");
        symlink(&bed.target, &link).unwrap();
        let error = run(&link, false, true).unwrap_err().to_string();
        assert!(error.contains("not a regular file"), "{error}");
        assert!(error.contains(link.to_str().unwrap()), "{error}");
        assert!(bed.target.exists());
        assert!(receipt_path().exists());
        assert!(bed.home.exists());
    }

    #[test]
    fn test_uninstall_plugin_failure_does_not_skip_later_steps() {
        let mut bed = Bed::new();
        let log = bed.tmp.path().join("calls");
        let quoted = crate::shell::shlex_quote(log.to_str().unwrap());
        bed.cli(
            "claude",
            &format!("echo \"claude $*\" >> {quoted}\necho denied >&2\nexit 1"),
        );
        bed.cli("codex", &format!("echo \"codex $*\" >> {quoted}"));
        assert!(!run(&bed.target, false, false).unwrap());
        assert_eq!(fs::read_to_string(log).unwrap().lines().count(), 4);
        assert!(!bed.target.exists());
        assert!(!receipt_path().exists());
        assert!(bed.home.exists());
    }

    #[test]
    fn test_uninstall_accepts_only_known_absence_errors() {
        let mut bed = Bed::new();
        bed.cli(
            "claude",
            "echo 'error: Marketplace '\"'hive'\"' not found' >&2\nexit 1",
        );
        assert!(remove_registration("claude", &[], "Marketplace 'hive' not found").is_ok());
        assert!(remove_registration("claude", &[], "another absence").is_err());
    }

    #[test]
    fn test_uninstall_force_deletes_teams_with_down() {
        let mut display = crate::testkit::display_env_outside();
        for key in [
            "HOME",
            "CLAUDE_HOME",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
            "GROK_HOME",
            "XDG_CONFIG_HOME",
        ] {
            display.env.set(key, display._tmp.path().join(key));
        }
        display.env.set("PATH", display._tmp.path());
        let target = display._tmp.path().join("binary");
        fs::write(&target, "binary").unwrap();
        crate::registry::record_team("probe", "", "100", &[], "").unwrap();
        let argv = crate::testkit::fake_tmux_sessions("", &[], &[], &["probe"]);
        assert!(run(&target, true, false).unwrap());
        assert!(crate::registry::load("probe").is_none());
        assert!(crate::testkit::has_row(
            &argv,
            &["kill-session", "-t", "=probe"]
        ));
        assert!(!target.exists());
    }
}
