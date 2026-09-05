//! Real-tmux e2e for eager display: `hive create` outside tmux builds a
//! detached session named after the team, `hive attach` rebuilds a missing
//! window (in the caller's session inside tmux, in a fresh team session
//! outside), `hive delete` closes what hive built and leaves what a human's
//! session lent. Every test runs the built binary against a private tmux
//! server (its own `TMUX_TMPDIR`) and a temp `HIVE_HOME`, so neither the
//! user's server nor their registry ever sees a session or a team.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

mod common;
use common::require_tmux;

/// Env markers that would give the binary an engine or tmux identity.
const IDENTITY_VARS: &[&str] = &[
    "TMUX",
    "TMUX_PANE",
    "CODEX_THREAD_ID",
    "GROK_SESSION_ID",
    "CLAUDE_CODE_MESSAGING_SOCKET",
];

struct Rig {
    tmp: tempfile::TempDir,
    team: String,
}

impl Rig {
    fn new(tag: &str) -> Self {
        require_tmux();
        let tmp = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(tmp.path().join("ws")).expect("workspace dir");
        Rig {
            tmp,
            team: format!("hivetest-{tag}-{}", std::process::id()),
        }
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join(".hive")
    }

    fn ws(&self) -> PathBuf {
        self.tmp.path().join("ws")
    }

    /// Where tmux keeps the private server's socket (created on first use).
    fn socket_dir(&self) -> PathBuf {
        self.tmp
            .path()
            .join(format!("tmux-{}", unsafe { libc::getuid() }))
    }

    fn registry_entry(&self) -> Option<Value> {
        let path = self.home().join("teams").join(&self.team).join("team.json");
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// A tmux client on the private server (never the user's). `-u`
    /// because a client without a UTF-8 locale gets its output sanitized
    /// (the tab separators below would come back as `_`); the binary under
    /// test fixes its own locale up in `main`, this client has to ask.
    fn tmux(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new("tmux");
        cmd.arg("-u").args(args).env("TMUX_TMPDIR", self.tmp.path());
        for key in IDENTITY_VARS {
            cmd.env_remove(key);
        }
        cmd.output().expect("tmux runs")
    }

    fn tmux_ok(&self, args: &[&str]) -> String {
        let out = self.tmux(args);
        assert!(
            out.status.success(),
            "tmux {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .trim_end_matches('\n')
            .to_string()
    }

    /// `(session_name, window_id)` of every window tagged for the team;
    /// none when the private server itself is gone (its last session
    /// closed).
    fn team_windows(&self) -> Vec<(String, String)> {
        let out = self.tmux(&[
            "list-windows",
            "-a",
            "-F",
            "#{session_name}\t#{window_id}\t#{@hive-team}",
        ]);
        if !out.status.success() {
            return Vec::new();
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('\t');
                let session = parts.next()?.to_string();
                let window_id = parts.next()?.to_string();
                (parts.next()? == self.team).then_some((session, window_id))
            })
            .collect()
    }

    /// The built hive binary, homed under the rig, on the private server.
    /// `inside` = (socket path, pane id) puts the call inside that pane the
    /// way a tmux client's shell would see it.
    fn hive(&self, args: &[&str], inside: Option<(&str, &str)>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_hive"));
        cmd.args(args)
            .current_dir(self.tmp.path())
            .stdin(Stdio::null())
            .env("HIVE_HOME", self.home())
            .env("CLAUDE_CONFIG_DIR", self.tmp.path().join("claude"))
            .env("CLAUDE_HOME", self.tmp.path().join("claude-home"))
            .env("CODEX_HOME", self.tmp.path().join("codex"))
            .env("GROK_HOME", self.tmp.path().join("grok"))
            .env("XDG_CACHE_HOME", self.tmp.path().join("cache"))
            .env("TMUX_TMPDIR", self.tmp.path());
        for key in IDENTITY_VARS {
            cmd.env_remove(key);
        }
        if let Some((socket, pane)) = inside {
            cmd.env("TMUX", format!("{socket},{},0", std::process::id()))
                .env("TMUX_PANE", pane);
        }
        cmd.output().expect("hive runs")
    }

    fn hive_ok(&self, args: &[&str], inside: Option<(&str, &str)>) -> String {
        let out = self.hive(args, inside);
        assert!(
            out.status.success(),
            "hive {args:?} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// `hive create` from outside tmux; returns the team window's id.
    fn create_outside_tmux(&self) -> String {
        let ws = self.ws();
        let stdout = self.hive_ok(
            &["create", &self.team, "--workspace", ws.to_str().unwrap()],
            None,
        );
        assert!(
            stdout.contains(&format!("Team '{}' created", self.team)),
            "create stdout: {stdout}"
        );
        let windows = self.team_windows();
        assert_eq!(windows.len(), 1, "team windows: {windows:?}");
        let (session, window_id) = windows.into_iter().next().unwrap();
        assert_eq!(
            session, self.team,
            "the team window lives in the team session"
        );
        let entry = self
            .registry_entry()
            .expect("registry entry written at create");
        assert_eq!(entry["display"], Value::String(window_id.clone()));
        assert_eq!(
            entry["workspace"],
            Value::String(ws.to_string_lossy().into_owned())
        );
        window_id
    }

    /// The team's hived socket — `hive attach` starts the hived and returns
    /// only after it answers, so the socket is an immediate oracle.
    fn hived_socket_exists(&self) -> bool {
        hive::hived::_socket_path(self.ws().to_str().unwrap()).exists()
    }

    fn delete(&self) {
        self.hive_ok(&["delete", &self.team, "--delete-workspace"], None);
        assert!(
            self.registry_entry().is_none(),
            "registry entry outlived delete"
        );
        assert!(!self.ws().exists(), "workspace outlived delete");
        assert!(
            !self.hived_socket_exists(),
            "the team's hived outlived delete"
        );
    }
}

impl Drop for Rig {
    // Best effort, panic or not: the team (and its hived) released, then
    // the private server with everything on it.
    fn drop(&mut self) {
        let _ = self.hive(&["delete", &self.team, "--delete-workspace"], None);
        let _ = self.tmux(&["kill-server"]);
    }
}

/// `display-message -t @N` exits 0 for a missing window, so existence is
/// read off the listing.
fn window_exists(rig: &Rig, window_id: &str) -> bool {
    let out = rig.tmux(&["list-windows", "-a", "-F", "#{window_id}"]);
    out.status.success()
        && String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|line| line == window_id)
}

fn session_alive(rig: &Rig, session: &str) -> bool {
    rig.tmux(&["has-session", "-t", &format!("={session}")])
        .status
        .success()
}

#[test]
fn test_create_outside_tmux_then_delete_closes_the_team_session() {
    let rig = Rig::new("create");
    rig.create_outside_tmux();
    assert!(session_alive(&rig, &rig.team));

    let stdout = rig.hive_ok(&["team", "-t", &rig.team], None);
    let payload: Value = serde_json::from_str(&stdout).expect("hive team prints JSON");
    assert_eq!(payload["tmuxSession"], Value::String(rig.team.clone()));
    assert_eq!(payload["name"], Value::String(rig.team.clone()));

    rig.delete();
    // The team window was the session's only window: killing it dropped
    // the session hive had built.
    assert!(!session_alive(&rig, &rig.team));
    assert!(rig.team_windows().is_empty());
}

#[test]
fn test_attach_inside_tmux_rebuilds_a_missing_window_in_the_callers_session() {
    let rig = Rig::new("attach-in");
    let first_window = rig.create_outside_tmux();

    // The display goes away underneath the team (window closed by hand);
    // the registry entry is what keeps the team alive.
    rig.tmux_ok(&["kill-window", "-t", &first_window]);
    assert!(!session_alive(&rig, &rig.team));
    assert!(rig.registry_entry().is_some());

    let human = format!("human-{}", std::process::id());
    let pane = rig.tmux_ok(&["new-session", "-d", "-s", &human, "-P", "-F", "#{pane_id}"]);
    let socket = rig.tmux_ok(&["display-message", "-p", "#{socket_path}"]);

    // No client is attached, so the final switch-client has nothing to
    // move; the heal before it is what the test is about.
    assert!(!rig.hived_socket_exists());
    let stdout = rig.hive_ok(&["attach", &rig.team], Some((&socket, &pane)));
    assert!(rig.hived_socket_exists(), "attach starts the team's hived");
    let windows = rig.team_windows();
    assert_eq!(windows.len(), 1, "team windows after heal: {windows:?}");
    let (session, healed_window) = windows.into_iter().next().unwrap();
    assert_eq!(
        session, human,
        "inside tmux the window is rebuilt in the caller's session"
    );
    let healed_target = rig.tmux_ok(&[
        "display-message",
        "-p",
        "-t",
        &healed_window,
        "#{session_name}:#{window_index}",
    ]);
    assert_eq!(stdout.trim_end(), format!("built {healed_target}"));
    let entry = rig.registry_entry().expect("registry entry");
    assert_eq!(entry["display"], Value::String(healed_window.clone()));
    // A fresh team session is not the answer inside tmux.
    assert!(!session_alive(&rig, &rig.team));

    rig.delete();
    // hive built that window itself, so delete closes it — but only the
    // window: the human's session, with its own window, stays.
    assert!(rig.team_windows().is_empty());
    assert!(!window_exists(&rig, &healed_window));
    assert!(session_alive(&rig, &human));
    assert_eq!(
        rig.tmux_ok(&["display-message", "-p", "-t", &pane, "#{pane_id}"]),
        pane,
        "the human's own pane is untouched"
    );
}

#[test]
fn test_attach_outside_tmux_rebuilds_the_team_session_before_attaching() {
    let rig = Rig::new("attach-out");
    let first_window = rig.create_outside_tmux();
    rig.tmux_ok(&["kill-window", "-t", &first_window]);
    assert!(!session_alive(&rig, &rig.team));

    // Without a terminal the final `tmux attach` cannot succeed, so the
    // exit status is tmux's refusal; the heal runs before the exec and is
    // what gets asserted.
    assert!(!rig.hived_socket_exists());
    let out = rig.hive(&["attach", &rig.team], None);
    assert!(
        !out.status.success(),
        "attach without a tty must not report success"
    );
    assert!(rig.hived_socket_exists(), "attach starts the team's hived");
    let windows = rig.team_windows();
    assert_eq!(windows.len(), 1, "team windows after heal: {windows:?}");
    let (session, healed_window) = windows.into_iter().next().unwrap();
    assert_eq!(
        session, rig.team,
        "outside tmux the window is rebuilt in a team session"
    );
    let entry = rig.registry_entry().expect("registry entry");
    assert_eq!(entry["display"], Value::String(healed_window));

    rig.delete();
    assert!(!session_alive(&rig, &rig.team));
}

#[test]
fn test_create_outside_tmux_rolls_the_window_back_when_the_workspace_fails() {
    let rig = Rig::new("rollback");
    // A workspace path under a regular file cannot be initialized.
    let blocker = rig.tmp.path().join("blocker");
    std::fs::write(&blocker, "").unwrap();
    let bad_ws = blocker.join("ws");

    let out = rig.hive(
        &["create", &rig.team, "--workspace", bad_ws.to_str().unwrap()],
        None,
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Error"), "stderr: {stderr}");
    // Nothing half-made survives: no registry entry, no tagged window, no
    // team session left behind for a retry to trip over.
    assert!(rig.registry_entry().is_none());
    assert!(rig.team_windows().is_empty());
    assert!(!session_alive(&rig, &rig.team));

    // The retry with a good workspace is a clean first create: one window,
    // in the team session, and `hive team` resolves that one.
    rig.create_outside_tmux();
    let stdout = rig.hive_ok(&["team", "-t", &rig.team], None);
    let payload: Value = serde_json::from_str(&stdout).expect("hive team prints JSON");
    assert_eq!(payload["tmuxSession"], Value::String(rig.team.clone()));
    rig.delete();
}

#[test]
fn test_attach_names_a_missing_team_without_touching_tmux() {
    let rig = Rig::new("ghost");
    let out = rig.hive(&["attach", &rig.team], None);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&rig.team), "stderr: {stderr}");
    // No server was ever started on the private socket.
    assert!(!rig.socket_dir().exists());
}
