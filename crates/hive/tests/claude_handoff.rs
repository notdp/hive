//! Private tmux + real child processes/ttys; the Claude fixture needs no network.
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
mod common;

const JOB: &str = "abc12345";
const TEAM: &str = "handoff";
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
    cli: &'static str,
    root: tempfile::TempDir,
    path: String,
    launcher: Option<Child>,
    roots: Vec<(String, Option<String>)>,
}

impl Rig {
    fn new() -> Self {
        common::require_tmux();
        let root = tempfile::Builder::new().prefix("hh-").tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let python = Command::new("which").arg("python3").output().unwrap();
        assert!(python.status.success());
        let python = String::from_utf8(python.stdout).unwrap();
        let script = format!(
            "#!{}\n{}",
            python.trim(),
            include_str!("fixtures/handoff_claude.py")
        );
        fs::write(bin.join("claude"), script).unwrap();
        fs::set_permissions(bin.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();
        let real_tmux = Command::new("which").arg("tmux").output().unwrap();
        let real_tmux = String::from_utf8(real_tmux.stdout).unwrap();
        let proxy = format!(
            r#"#!{python}
import os, pathlib, signal, sys, time
r=pathlib.Path(os.environ['HANDOFF_TEST_ROOT'])
a=sys.argv[1:]
if a and a[0]=='attach-session' and (r/'fail-attach').exists(): sys.exit(42)
if '@hive-agent' in a and (r/'block-bind').exists():
    parent=os.getppid()
    (r/'bind-reached').touch()
    while (r/'block-bind').exists():
        if os.getppid()!=parent: sys.exit(1)
        time.sleep(.01)
if '@hive-agent' in a and (r/'kill-launcher').exists():
    import json
    data=json.loads((r/'c/hive-control/launch-{job}.json').read_text())
    os.kill(data['pid'],signal.SIGKILL)
    (r/'kill-launcher').unlink()
os.execv({tmux:?},[{tmux:?}]+a)
"#,
            python = python.trim(),
            job = JOB,
            tmux = real_tmux.trim()
        );
        fs::write(bin.join("tmux"), proxy).unwrap();
        fs::set_permissions(bin.join("tmux"), fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Self {
            cli: "claude",
            root,
            path,
            launcher: None,
            roots: Vec::new(),
        }
    }
    fn codex() -> Self {
        let mut rig = Self::new();
        rig.cli = "codex";
        let python = Command::new("which").arg("python3").output().unwrap();
        let script = format!(
            "#!{}\n{}",
            String::from_utf8(python.stdout).unwrap().trim(),
            include_str!("fixtures/handoff_codex.py")
        );
        fs::write(rig.file("bin/codex"), script).unwrap();
        fs::set_permissions(rig.file("bin/codex"), fs::Permissions::from_mode(0o755)).unwrap();
        rig
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
            .env("TERM", "xterm-256color")
            .env("HIVE_BIN", env!("CARGO_BIN_EXE_hive"));
        for k in CLEAN {
            c.env_remove(k);
        }
        for (key, value) in &self.roots {
            match value {
                Some(value) => {
                    c.env(key, value);
                }
                None => {
                    c.env_remove(key);
                }
            }
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
    fn launch(&mut self, resume: bool) {
        let mut c = self.command("script");
        let hive = env!("CARGO_BIN_EXE_hive");
        if cfg!(target_os = "macos") {
            c.args(["-q", "/dev/null", hive, self.cli]);
            if resume {
                c.args([
                    if self.cli == "claude" {
                        "--resume"
                    } else {
                        "resume"
                    },
                    if self.cli == "claude" {
                        JOB
                    } else {
                        CODEX_THREAD
                    },
                ]);
            }
        } else {
            let command = if resume {
                format!(
                    "{hive} {} {} {}",
                    self.cli,
                    if self.cli == "claude" {
                        "--resume"
                    } else {
                        "resume"
                    },
                    if self.cli == "claude" {
                        JOB
                    } else {
                        CODEX_THREAD
                    }
                )
            } else {
                format!("{hive} {}", self.cli)
            };
            c.args(["-qec", &command, "/dev/null"]);
        }
        self.launcher = Some(
            c.stdin(Stdio::piped())
                .stdout(fs::File::create(self.file("screen")).unwrap())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    fn engine_command(&self, args: &[&str]) -> Command {
        let e: Value =
            serde_json::from_slice(&fs::read(self.file("engine.json")).unwrap()).unwrap();
        let mut c = self.command(env!("CARGO_BIN_EXE_hive"));
        c.args(args);
        if self.cli == "codex" {
            c.env("CODEX_THREAD_ID", CODEX_THREAD);
        } else {
            c.env(
                "CLAUDE_CODE_MESSAGING_SOCKET",
                e["messagingSocketPath"].as_str().unwrap(),
            );
        }
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
                "launcher did not exit: {} clients: {}",
                fs::read_to_string(self.file("screen")).unwrap_or_default(),
                self.clients()
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
        if let Ok(bytes) = fs::read(self.file("engine.json")) {
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                if let Some(pid) = v["pid"].as_i64() {
                    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                }
            }
        }
    }
}

#[test]
fn test_create_transfers_one_job_and_resume_reuses_its_team_viewer() {
    let mut r = Rig::new();
    r.launch(false);
    r.ready();
    assert!(
        !r.tmux(&["list-sessions"]).status.success(),
        "chat alone must not create tmux"
    );
    let before = fs::read(r.file("engine.json")).unwrap();
    let result = r.create();
    let pane = result["orch"]["pane"].as_str().unwrap();
    r.wait("team viewer", || r.events("attach").len() == 2);
    r.wait("terminal attached", || !r.clients().is_empty());
    assert_eq!(before, fs::read(r.file("engine.json")).unwrap());
    assert_eq!(
        r.tmux_ok(&["show-options", "-v", "-t", TEAM, "status"]),
        "2"
    );
    assert_eq!(
        r.tmux_ok(&["display-message", "-p", "-t", pane, "#{@hive-role}"]),
        "agent"
    );
    r.tmux_ok(&["send-keys", "-t", pane, "z"]);
    r.wait("input in team viewer", || {
        r.events("input").iter().any(|v| v["key"] == "z")
    });
    assert!(r.events("overlap").is_empty());
    r.detach();
    r.wait_launcher();
    std::thread::sleep(Duration::from_millis(150));
    assert!(r.clients().is_empty());
    r.launch(true);
    r.wait("resume attaches team", || !r.clients().is_empty());
    assert_eq!(
        r.events("attach").len(),
        2,
        "resume must not steal the Claude viewer"
    );
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
    assert!(!r.events("stop").is_empty());
}

#[test]
fn test_binding_failure_restores_local_viewer_and_create_can_retry() {
    let mut r = Rig::new();
    r.launch(false);
    r.ready();
    let fail = r.file("c/hive-control/hive-pane-0.job");
    fs::create_dir(&fail).unwrap();
    let out = r.engine_command(&["create", TEAM]).output().unwrap();
    assert!(!out.status.success());
    r.wait("restored viewer", || r.events("attach").len() == 2);
    assert!(!r.team_entry().exists());
    assert!(!r.tmux(&["has-session", "-t", TEAM]).status.success());
    assert!(r.events("overlap").is_empty());
    assert!(r.events("stop").is_empty());
    fs::remove_dir(fail).unwrap();
    r.create();
    r.wait("retried viewer", || r.events("attach").len() == 3);
    assert!(r.events("overlap").is_empty());
}

#[test]
fn test_terminal_attach_failure_keeps_committed_team_without_reopening_local_viewer() {
    let mut r = Rig::new();
    r.launch(false);
    r.ready();
    fs::write(r.file("fail-attach"), "").unwrap();
    r.create();
    r.wait("team viewer", || r.events("attach").len() == 2);
    r.wait_launcher();
    assert!(r.team_entry().exists());
    assert!(r.clients().is_empty());
    assert!(r.events("overlap").is_empty());
    assert_eq!(
        r.events("attach")
            .iter()
            .filter(|v| v["pane"] == "")
            .count(),
        1
    );
}

#[test]
fn test_creator_dying_before_commit_rolls_back_and_restores_local_viewer() {
    let mut r = Rig::new();
    r.launch(false);
    r.ready();
    fs::write(r.file("block-bind"), "").unwrap();
    let mut create = r
        .engine_command(&["create", TEAM])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    r.wait("binding barrier", || r.file("bind-reached").exists());
    create.kill().unwrap();
    create.wait().unwrap();
    r.wait("restored viewer", || r.events("attach").len() == 2);
    assert!(!r.team_entry().exists());
    assert!(!r.tmux(&["has-session", "-t", TEAM]).status.success());
    assert!(r.events("stop").is_empty());
    assert!(r.events("overlap").is_empty());
}

#[test]
fn test_launcher_dying_after_release_leaves_a_complete_recoverable_team() {
    let mut r = Rig::new();
    r.launch(false);
    r.ready();
    fs::write(r.file("kill-launcher"), "").unwrap();
    let result = r.create();
    assert!(result["handoff"].as_str().unwrap().contains("recovery"));
    r.wait("recovered team viewer", || r.events("attach").len() == 2);
    assert!(r.team_entry().exists());
    assert!(r.events("overlap").is_empty());
}

#[test]
fn test_join_moves_viewer_into_existing_team_without_replacing_its_roster() {
    check_join(Rig::new());
}

#[test]
fn test_codex_join_preserves_existing_team_roots_and_resumes_its_thread() {
    check_join(Rig::codex());
}

fn check_join(mut r: Rig) {
    let out = r
        .command(env!("CARGO_BIN_EXE_hive"))
        .args(["create", TEAM])
        .output()
        .unwrap();
    assert!(out.status.success());
    r.tmux_ok(&[
        "set-environment",
        "-t",
        TEAM,
        "CODEX_HOME",
        "existing-team-root",
    ]);
    r.launch(false);
    r.ready();
    let out = r
        .engine_command(&["join", TEAM, "--as", "reviewer", "--no-notify"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    r.wait("joined viewer", || r.events("attach").len() == 2);
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["joined"], "reviewer");
    assert_eq!(
        r.tmux_ok(&["show-environment", "-t", TEAM, "CODEX_HOME"]),
        "CODEX_HOME=existing-team-root"
    );
    assert!(r.events("overlap").is_empty());
}

#[test]
fn test_user_exit_from_local_viewer_does_not_create_a_team_or_restart_viewer() {
    let mut r = Rig::new();
    r.launch(false);
    r.ready();
    r.launcher
        .as_mut()
        .unwrap()
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"q")
        .unwrap();
    r.wait_launcher();
    assert_eq!(r.events("attach").len(), 1);
    assert!(!r.team_entry().exists());
    assert!(!r.file("c/hive-control/launch-abc12345.json").exists());
}

#[test]
fn test_new_team_roots_keep_set_empty_and_unset_over_existing_server_globals() {
    for value in [None, Some(""), Some("caller-root")] {
        let mut r = Rig::new();
        let base = r
            .command("tmux")
            .env("CODEX_HOME", "stale-server-root")
            .args(["new-session", "-d", "-s", "base"])
            .output()
            .unwrap();
        assert!(base.status.success());
        r.roots
            .push(("CODEX_HOME".into(), value.map(str::to_string)));
        r.launch(false);
        r.ready();
        r.create();
        r.wait("team viewer", || r.events("attach").len() == 2);
        let expected = value.map(Value::from).unwrap_or(Value::Null);
        for event in r.events("attach") {
            assert_eq!(event["roots"]["CODEX_HOME"], expected);
        }
        assert_eq!(
            r.tmux_ok(&["show-environment", "-g", "CODEX_HOME"]),
            "CODEX_HOME=stale-server-root"
        );
        assert_eq!(
            r.tmux_ok(&["show-environment", "-t", TEAM, "CODEX_HOME"]),
            value
                .map(|v| format!("CODEX_HOME={v}"))
                .unwrap_or_else(|| "-CODEX_HOME".into())
        );
        let output = r.file("split-root");
        r.tmux_ok(&[
            "split-window",
            "-d",
            "-t",
            TEAM,
            &format!(
                "printf '%s' \"${{CODEX_HOME-UNSET}}\" > {}",
                output.display()
            ),
        ]);
        r.wait("split root", || output.exists());
        assert_eq!(
            fs::read_to_string(&output).unwrap(),
            value.unwrap_or("UNSET")
        );
    }
}

#[test]
fn test_dead_launcher_is_refused_before_creating_team_resources() {
    let mut r = Rig::new();
    r.launch(false);
    r.ready();
    let record: Value =
        serde_json::from_slice(&fs::read(r.file("c/hive-control/launch-abc12345.json")).unwrap())
            .unwrap();
    unsafe { libc::kill(record["pid"].as_i64().unwrap() as i32, libc::SIGKILL) };
    r.wait_launcher();
    let out = r.engine_command(&["create", TEAM]).output().unwrap();
    assert!(!out.status.success());
    assert!(!r.team_entry().exists());
    assert!(!r.tmux(&["list-sessions"]).status.success());
}

#[test]
fn test_local_exit_during_unfinished_request_does_not_restart_viewer() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    let mut r = Rig::new();
    r.launch(false);
    r.ready();
    let record: Value =
        serde_json::from_slice(&fs::read(r.file("c/hive-control/launch-abc12345.json")).unwrap())
            .unwrap();
    let mut socket = UnixStream::connect(record["socket"].as_str().unwrap()).unwrap();
    writeln!(
        socket,
        "{}",
        serde_json::json!({"op":"hello", "job":JOB, "nonce":record["nonce"]})
    )
    .unwrap();
    let mut reader = BufReader::new(socket);
    let mut reply = String::new();
    reader.read_line(&mut reply).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&reply).unwrap()["ok"], true);
    r.launcher
        .as_mut()
        .unwrap()
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"q")
        .unwrap();
    r.wait("local viewer exit", || !r.file("viewer.pid").exists());
    drop(reader);
    r.wait_launcher();
    assert_eq!(r.events("attach").len(), 1);
    assert!(!r.team_entry().exists());
}

#[test]
fn test_delete_from_a_shell_refuses_the_transferred_job_mid_turn_and_down_retires_it() {
    let mut r = Rig::new();
    r.launch(false);
    r.ready();
    r.create();
    // While the job is producing output it is mid-turn to the hived, and a
    // plain delete from a shell (nobody's own turn) is refused and points
    // at --down; once the job has gone quiet the plain delete ends the
    // team. Which one this run sees depends on the job's timing — both
    // keep the contract: the team ends only with the job retired.
    let out = r
        .command(env!("CARGO_BIN_EXE_hive"))
        .args(["delete", TEAM])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        assert!(
            stderr.contains("mid-turn") && stderr.contains("--down"),
            "{stderr}"
        );
        assert!(r.team_entry().exists());
        assert!(r.events("stop").is_empty());
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
    }
    assert!(!r.team_entry().exists());
    assert!(!r.events("stop").is_empty());
}

const CODEX_THREAD: &str = "11111111-2222-4333-8444-555555555555";

#[test]
fn test_codex_create_and_resume_keep_daemon_and_thread() {
    let mut r = Rig::codex();
    r.launch(false);
    r.ready();
    let before = fs::read(r.file("engine.json")).unwrap();
    let result = r.create();
    let entry: Value = serde_json::from_slice(&fs::read(r.team_entry()).unwrap()).unwrap();
    assert_eq!(entry["members"][0]["sessionId"], CODEX_THREAD);
    let pane = result["orch"]["pane"].as_str().unwrap();
    r.wait("Codex team viewer", || r.events("attach").len() == 2);
    r.wait("terminal attached", || !r.clients().is_empty());
    r.tmux_ok(&["send-keys", "-t", pane, "z"]);
    r.wait("Codex input", || {
        r.events("input").iter().any(|v| v["key"] == "z")
    });
    assert_eq!(before, fs::read(r.file("engine.json")).unwrap());
    assert!(r.events("overlap").is_empty());
    assert_eq!(r.events("thread/start").len(), 1);
    r.detach();
    r.wait_launcher();
    r.launch(true);
    r.wait("resume attaches Codex team", || !r.clients().is_empty());
    assert_eq!(r.events("attach").len(), 2);
    r.detach();
    r.wait_launcher();
}

#[test]
fn test_codex_bind_failure_restores_same_thread_without_second_start() {
    let mut r = Rig::codex();
    r.launch(false);
    r.ready();
    let fail = r.file("x/app-server-control/hive-pane-0.thread");
    fs::create_dir(&fail).unwrap();
    let out = r.engine_command(&["create", TEAM]).output().unwrap();
    assert!(!out.status.success());
    r.wait("restored Codex viewer", || r.events("attach").len() == 2);
    assert!(!r.team_entry().exists());
    assert!(r.events("overlap").is_empty());
    assert_eq!(r.events("thread/start").len(), 1);
    fs::remove_dir(fail).unwrap();
    r.create();
    r.wait("retried Codex viewer", || r.events("attach").len() == 3);
}
