//! The tmux server hive brings up must not keep the caller's working
//! directory: tmux forks the server out of the first client, and a server
//! whose cwd is later deleted skips the `chdir` for every `-c` it is
//! given (spawn.c gates it on its own `getcwd()`), so every new pane is
//! born in the dead directory.

mod common;

use std::path::PathBuf;
use std::process::{Command, Stdio};

use common::require_tmux;

struct Rig {
    tmp: tempfile::TempDir,
    team: String,
}

impl Rig {
    fn socket(&self) -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;
        let dir = self
            .tmp
            .path()
            .join(format!("tmux-{}", unsafe { libc::getuid() }));
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir);
        dir.join("default")
    }

    fn tmux(&self, args: &[&str]) -> std::process::Output {
        Command::new("tmux")
            .arg("-u")
            .arg("-S")
            .arg(self.socket())
            .args(args)
            .env("TMUX_TMPDIR", self.tmp.path())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .expect("tmux runs")
    }

    /// The binary under test, run from *cwd* with every root under the
    /// temp dir and no inherited engine or tmux identity.
    fn hive(&self, args: &[&str], cwd: &std::path::Path) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_hive"))
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .env("HIVE_HOME", self.tmp.path().join(".hive"))
            .env("CLAUDE_CONFIG_DIR", self.tmp.path().join("claude"))
            .env("CLAUDE_HOME", self.tmp.path().join("claude-home"))
            .env("CODEX_HOME", self.tmp.path().join("codex"))
            .env("GROK_HOME", self.tmp.path().join("grok"))
            .env("XDG_CACHE_HOME", self.tmp.path().join("cache"))
            .env("TMUX_TMPDIR", self.tmp.path())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("CODEX_THREAD_ID")
            .env_remove("GROK_SESSION_ID")
            .env_remove("CLAUDE_CODE_MESSAGING_SOCKET")
            .env_remove("CLAUDE_CODE_HOST_SESSION_ID")
            .output()
            .expect("hive runs")
    }

    fn tmux_ok(&self, args: &[&str]) -> String {
        let out = self.tmux(args);
        assert!(
            out.status.success(),
            "tmux {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}

/// The process's working directory, when the platform exposes it.
fn process_cwd(pid: &str) -> Option<PathBuf> {
    if let Ok(link) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
        return Some(link);
    }
    let out = Command::new("lsof")
        .args(["-a", "-p", pid, "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| line.strip_prefix('n'))
        .map(PathBuf::from)
}

impl Drop for Rig {
    // Best effort, panic or not: the team (and its hived) released, then
    // the private server with everything on it.
    fn drop(&mut self) {
        let root = self.tmp.path().to_path_buf();
        let _ = self.hive(&["delete", &self.team, "--delete-workspace"], &root);
        let _ = self.tmux(&["kill-server"]);
    }
}

#[test]
fn test_hive_create_starts_the_tmux_server_outside_the_callers_directory() {
    require_tmux();
    let tmp = tempfile::tempdir().expect("temp dir");
    let rig = Rig {
        tmp,
        team: format!("hivetest-servercwd-{}", std::process::id()),
    };
    // The caller's cwd: a directory that goes away after the server is up,
    // as a removed worktree does.
    let caller = rig.tmp.path().join("caller");
    std::fs::create_dir(&caller).expect("caller dir");
    let out = rig.hive(&["create", &rig.team], &caller);
    assert!(
        out.status.success(),
        "hive create failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::remove_dir_all(&caller).expect("caller dir removed");

    // The server's own directory is hive's root, not the caller's.
    let hive_home = std::fs::canonicalize(rig.tmp.path().join(".hive")).expect("hive home");
    let pid = rig.tmux_ok(&["display", "-p", "#{pid}"]);
    if let Some(server_cwd) = process_cwd(&pid) {
        assert_eq!(server_cwd, hive_home, "server did not start from HIVE_HOME");
    }

    // A pane asked for elsewhere lands there. On a server stuck in the
    // deleted directory the `-c` is skipped and the pane inherits it.
    let elsewhere = rig.tmp.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("elsewhere dir");
    let pane = rig.tmux_ok(&[
        "split-window",
        "-d",
        "-t",
        &format!("{}:", rig.team),
        "-c",
        elsewhere.to_str().unwrap(),
        "-P",
        "-F",
        "#{pane_id}",
        "sleep 30",
    ]);
    let landed = rig.tmux_ok(&["display", "-p", "-t", &pane, "#{pane_current_path}"]);
    let landed = std::fs::canonicalize(&landed).unwrap_or_else(|_| PathBuf::from(&landed));
    let elsewhere = std::fs::canonicalize(&elsewhere).expect("elsewhere resolves");
    assert_eq!(landed, elsewhere, "server kept the caller's deleted cwd");
}
