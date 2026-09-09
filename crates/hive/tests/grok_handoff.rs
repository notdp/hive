//! Private tmux + real child processes/ttys; the grok fixture needs no network.
//! The grok side of the terminal handoff: a launch leader keeps serving the
//! session while the TUI moves from the terminal into the team pane.
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
mod common;

const SID: &str = "0f0f0f0f-1111-4222-8333-444444444444";
const TEAM: &str = "grokhand";
const CLEAN: &[&str] = &[
    "TMUX",
    "TMUX_PANE",
    "CODEX_THREAD_ID",
    "GROK_SESSION_ID",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_HOST_SESSION_ID",
    "CLAUDE_HOME",
];

struct Rig {
    root: tempfile::TempDir,
    path: String,
    launcher: Option<Child>,
}

impl Rig {
    fn new() -> Self {
        common::require_tmux();
        let root = tempfile::Builder::new().prefix("gh-").tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let python = Command::new("which").arg("python3").output().unwrap();
        assert!(python.status.success());
        let python = String::from_utf8(python.stdout).unwrap();
        let script = format!(
            "#!{}\n{}",
            python.trim(),
            include_str!("fixtures/handoff_grok.py")
        );
        fs::write(bin.join("grok"), script).unwrap();
        fs::set_permissions(bin.join("grok"), fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Self {
            root,
            path,
            launcher: None,
        }
    }
    fn file(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }
    fn command(&self, binary: &str) -> Command {
        let mut c = Command::new(binary);
        c.current_dir(self.root.path())
            .env("PATH", &self.path)
            .env("HOME", self.file("home"))
            .env("HIVE_HOME", self.file("h"))
            .env("CLAUDE_CONFIG_DIR", self.file("c"))
            .env("CODEX_HOME", self.file("x"))
            .env("GROK_HOME", self.file("g"))
            .env("TMUX_TMPDIR", self.root.path())
            .env("XDG_CACHE_HOME", self.file("cache"))
            .env("HANDOFF_TEST_ROOT", self.root.path())
            .env("TERM", "xterm-256color");
        for k in CLEAN {
            c.env_remove(k);
        }
        c
    }
    fn tmux(&self, args: &[&str]) -> Output {
        self.command("tmux").args(args).output().unwrap()
    }
    fn tmux_ok(&self, args: &[&str]) -> String {
        let out = self.tmux(args);
        assert!(
            out.status.success(),
            "tmux {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    /// `hgrok` at a terminal: a pty from `script`, the session id pinned so
    /// the test can speak as the engine's tools.
    fn launch(&mut self, resume: bool) {
        let mut c = self.command("script");
        let hive = env!("CARGO_BIN_EXE_hive");
        let flag = if resume { "--resume" } else { "--session-id" };
        if cfg!(target_os = "macos") {
            c.args(["-q", "/dev/null", hive, "grok", flag, SID]);
        } else {
            c.args(["-qec", &format!("{hive} grok {flag} {SID}"), "/dev/null"]);
        }
        self.launcher = Some(
            c.stdin(Stdio::piped())
                .stdout(fs::File::create(self.file("screen")).unwrap())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    /// `hive` as a tool the leader runs: it carries the session id the
    /// leader exports.
    fn engine_command(&self, args: &[&str]) -> Command {
        let mut c = self.command(env!("CARGO_BIN_EXE_hive"));
        c.args(args).env("GROK_SESSION_ID", SID);
        c
    }
    fn events(&self, kind: &str) -> Vec<Value> {
        fs::read_to_string(self.file("events"))
            .unwrap_or_default()
            .lines()
            .filter_map(|s| serde_json::from_str::<Value>(s).ok())
            .filter(|v| v["kind"] == kind)
            .collect()
    }
    fn wait(&self, what: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "{what}: {}",
                fs::read_to_string(self.file("screen")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    fn ready(&self) {
        self.wait("local viewer", || self.events("attach").len() == 1);
    }
    fn leader_pid(&self) -> i64 {
        let v: Value =
            serde_json::from_slice(&fs::read(self.file("leader.json")).unwrap()).unwrap();
        v["pid"].as_i64().unwrap()
    }
    fn launch_key(&self) -> String {
        let hive = self.file("g/hive");
        fs::read_dir(hive)
            .unwrap()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".sock")
                    .filter(|k| k.starts_with("l-"))
                    .map(str::to_string)
            })
            .next()
            .expect("a launch leader socket")
    }
    fn create(&self) -> Value {
        let out = self.engine_command(&["create", TEAM]).output().unwrap();
        assert!(
            out.status.success(),
            "create: {} screen: {}",
            String::from_utf8_lossy(&out.stderr),
            fs::read_to_string(self.file("screen")).unwrap_or_default()
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn wait_launcher(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if self
                .launcher
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_some()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "launcher did not exit: {}",
                fs::read_to_string(self.file("screen")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(30));
        }
        self.launcher.take();
    }
    fn clients(&self) -> String {
        String::from_utf8_lossy(
            &self
                .tmux(&[
                    "list-clients",
                    "-F",
                    "#{?client_control_mode,,#{client_name}}",
                ])
                .stdout,
        )
        .trim()
        .into()
    }
    fn detach(&self) {
        for client in self.clients().lines().filter(|s| !s.is_empty()) {
            self.tmux_ok(&["detach-client", "-t", client]);
        }
    }
    fn team_entry(&self) -> PathBuf {
        self.file(&format!("h/teams/{TEAM}/team.json"))
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self
            .command(env!("CARGO_BIN_EXE_hive"))
            .args(["delete", TEAM, "--down"])
            .output();
        let _ = self.tmux(&["kill-server"]);
        if let Some(mut c) = self.launcher.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Ok(bytes) = fs::read(self.file("leader.json")) {
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                if let Some(pid) = v["pid"].as_i64() {
                    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                }
            }
        }
    }
}

#[test]
fn test_create_moves_the_grok_tui_into_the_team_on_the_same_leader() {
    let mut r = Rig::new();
    r.launch(false);
    r.ready();
    assert!(
        !r.tmux(&["list-sessions"]).status.success(),
        "chat alone must not create tmux"
    );
    let key = r.launch_key();
    let leader = r.leader_pid();
    let local = &r.events("attach")[0];
    assert_eq!(local["session"], SID);
    assert!(local["socket"]
        .as_str()
        .unwrap()
        .ends_with(&format!("{key}.sock")));
    assert_eq!(local["pane"], "");

    let result = r.create();
    let pane = result["orch"]["pane"].as_str().unwrap();
    r.wait("team viewer", || r.events("attach").len() == 2);
    r.wait("terminal attached", || !r.clients().is_empty());
    // the same leader serves the team pane through the member's alias
    assert_eq!(r.leader_pid(), leader);
    let team_viewer = &r.events("attach")[1];
    assert_eq!(team_viewer["pane"], pane);
    assert_eq!(team_viewer["session"], SID);
    assert!(team_viewer["socket"]
        .as_str()
        .unwrap()
        .ends_with(&format!("{key}.sock")));
    assert_eq!(
        fs::read_to_string(r.file(&format!("g/hive/m-{TEAM}.orch.alias"))).unwrap(),
        key
    );
    assert!(r.events("overlap").is_empty());
    assert_eq!(
        r.tmux_ok(&["show-options", "-v", "-t", TEAM, "status"]),
        "2"
    );
    assert_eq!(
        r.tmux_ok(&["display-message", "-p", "-t", pane, "#{@hive-role}"]),
        "agent"
    );
    let entry: Value = serde_json::from_slice(&fs::read(r.team_entry()).unwrap()).unwrap();
    let orch = &entry["members"][0];
    assert_eq!(
        (orch["cli"].as_str(), orch["sessionId"].as_str()),
        (Some("grok"), Some(SID))
    );
    r.tmux_ok(&["send-keys", "-t", pane, "z"]);
    r.wait("input in team viewer", || {
        r.events("input").iter().any(|v| v["key"] == "z")
    });
    r.detach();
    r.wait_launcher();
    // the launcher's exit leaves the member's leader alone
    assert!(r.events("stop").is_empty());
    std::thread::sleep(Duration::from_millis(150));
    assert!(r.clients().is_empty());

    // an external resume of an enrolled session opens the team, never a
    // second viewer
    r.launch(true);
    r.wait("resume attaches team", || !r.clients().is_empty());
    assert_eq!(r.events("attach").len(), 2);
    r.detach();
    r.wait_launcher();

    let out = r
        .command(env!("CARGO_BIN_EXE_hive"))
        .args(["delete", TEAM, "--down"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!r.team_entry().exists());
    r.wait("leader stopped with the team", || {
        !r.events("stop").is_empty()
    });
    assert!(!r.file(&format!("g/hive/{key}.sock")).exists());
    assert!(!r.file(&format!("g/hive/m-{TEAM}.orch.alias")).exists());
}

#[test]
fn test_a_member_with_its_own_leader_refuses_the_bind_and_the_local_tui_returns() {
    let mut r = Rig::new();
    r.launch(false);
    r.ready();
    let own = r.file(&format!("g/hive/m-{TEAM}.orch.sock"));
    fs::write(&own, "").unwrap();
    let out = r.engine_command(&["create", TEAM]).output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("leader of its own"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    r.wait("restored viewer", || r.events("attach").len() == 2);
    assert!(!r.team_entry().exists());
    assert!(!r.file(&format!("g/hive/m-{TEAM}.orch.alias")).exists());
    assert!(r.events("overlap").is_empty());
    assert!(r.events("stop").is_empty());
    fs::remove_file(own).unwrap();
    // the retry goes through
    let result = r.create();
    assert!(result["orch"]["pane"].is_string());
    r.wait("team viewer", || r.events("attach").len() == 3);
    r.detach();
    r.wait_launcher();
}

#[test]
fn test_leaving_the_local_tui_stops_an_unbound_launch_leader() {
    let mut r = Rig::new();
    r.launch(false);
    r.ready();
    let key = r.launch_key();
    r.launcher
        .as_mut()
        .unwrap()
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"q")
        .unwrap();
    r.wait_launcher();
    r.wait("leader stopped", || !r.events("stop").is_empty());
    assert!(!r.file(&format!("g/hive/{key}.sock")).exists());
    assert!(!r.file(&format!("g/hive/{key}.session")).exists());
    assert!(!r.tmux(&["list-sessions"]).status.success());
}
