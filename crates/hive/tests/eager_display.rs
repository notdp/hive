//! Real-tmux e2e for eager display: `hive create` outside tmux builds a
//! detached session named after the team, `hive attach` rebuilds a missing
//! window in the team's own session (whether the caller is inside tmux or
//! not), `hive delete` closes what hive built and leaves what a human's
//! session lent. Every test runs the built binary against a private tmux
//! server (its own `TMUX_TMPDIR`) and a temp `HIVE_HOME`, so neither the
//! user's server nor their registry ever sees a session or a team.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

mod common;
use common::{kill_session, private_server, require_tmux, run_tmux, PtyClient};

/// Env markers that would give the binary an engine or tmux identity — or,
/// for a fixture registered as a desktop session, the developer's real
/// desktop record to read.
const IDENTITY_VARS: &[&str] = &[
    "TMUX",
    "TMUX_PANE",
    "CODEX_THREAD_ID",
    "GROK_SESSION_ID",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_HOST_SESSION_ID",
];

struct Rig {
    tmp: tempfile::TempDir,
    /// Where the rig's hive home, workspace and `bin/hive` live: the temp
    /// dir itself, or a directory under it whose name is the point of the
    /// test (a path with shell-special characters).
    root: PathBuf,
    team: String,
    /// The rig's private bin dir first, then the developer's PATH: the
    /// built `hive` under test and every stub CLI live there. tmux gives a
    /// new pane the PATH of the *client* that asked for it (3.7
    /// `spawn.c`, when that client sits in no session — every hive
    /// process here), so it rides on every tmux client and every hive
    /// process the rig runs; `HOME` is the rig's too, or the pane's login
    /// shell would rebuild PATH from the developer's dotfiles and find the
    /// installed hive and the real engines instead.
    path: String,
}

impl Rig {
    fn new(tag: &str) -> Self {
        Rig::new_under(tag, "")
    }

    /// A rig whose home, workspace and `bin/hive` sit under *sub* of the
    /// temp dir ("" for the temp dir itself).
    fn new_under(tag: &str, sub: &str) -> Self {
        require_tmux();
        let tmp = tempfile::tempdir().expect("temp dir");
        let root = if sub.is_empty() {
            tmp.path().to_path_buf()
        } else {
            tmp.path().join(sub)
        };
        std::fs::create_dir_all(root.join("ws")).expect("workspace dir");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        // A pane command is `hive <cli> --resume …`, resolved by the pane
        // shell: this link, not the developer's installed binary, is what
        // it finds.
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_hive"), bin.join("hive"))
            .expect("hive on the rig's PATH");
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Rig {
            path,
            tmp,
            root,
            team: format!("hivetest-{tag}-{}", std::process::id()),
        }
    }

    fn home(&self) -> PathBuf {
        self.root.join(".hive")
    }

    fn ws(&self) -> PathBuf {
        self.root.join("ws")
    }

    /// The `hive` the rig's hooks name: the link under its root when that
    /// is not the temp dir (the binary path is then part of the test), else
    /// the built binary itself.
    fn hive_bin(&self) -> Option<PathBuf> {
        (self.root != self.tmp.path()).then(|| self.root.join("bin").join("hive"))
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
        self.tmux_cmd(args).output().expect("tmux runs")
    }

    /// A tmux client command on the private server, env set, not yet run.
    fn tmux_cmd(&self, args: &[&str]) -> Command {
        // `-S`: by explicit socket, so a client outliving the temp dir can
        // never fall through to the developer's default server (tmux does
        // that silently when TMUX_TMPDIR names a missing directory).
        // tmux creates the socket directory only when resolving it from
        // TMUX_TMPDIR; a `-S` client needs it there before a server binds,
        // and tmux refuses any mode but 0700.
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(self.socket_dir());
        let socket = self.socket_dir().join("default");
        let mut cmd = Command::new("tmux");
        cmd.arg("-u")
            .arg("-S")
            .arg(&socket)
            .args(args)
            .env("PATH", &self.path)
            .env("HOME", self.tmp.path())
            .env("TMUX_TMPDIR", self.tmp.path());
        for key in IDENTITY_VARS {
            cmd.env_remove(key);
        }
        cmd
    }

    /// A terminal arriving at the team session: `tmux attach` on a pty,
    /// the way a human's terminal attaches — what fires the session's
    /// `client-attached` hook. *env* is the human's shell environment on
    /// top of the rig's; the hook runs under the server's, not this.
    fn attach_client(&self, env: &[(&str, &str)]) -> PtyClient {
        self.attach_session_client(&self.team, env)
    }

    /// `attach_client` at *session* (a name), not the team session.
    fn attach_session_client(&self, session: &str, env: &[(&str, &str)]) -> PtyClient {
        let mut cmd = self.tmux_cmd(&["attach", "-t", &format!("={session}")]);
        for (key, value) in env {
            cmd.env(key, value);
        }
        let client = PtyClient::spawn(cmd);
        let pid = client.pid().to_string();
        wait_until("the terminal client to attach", || {
            self.tmux_ok(&["list-clients", "-F", "#{client_pid}"])
                .lines()
                .any(|line| line == pid)
        });
        client
    }

    /// The team session's wake hook entries as tmux lists them, one line
    /// each — asked by session id, the target the hooks were set on.
    fn wake_hooks(&self) -> Vec<String> {
        self.wake_hooks_on(&self.session_id(&self.team))
    }

    /// `wake_hooks` of the session with id *session_id*. An array every
    /// entry has been unset from is listed as its bare name: no entry.
    fn wake_hooks_on(&self, session_id: &str) -> Vec<String> {
        self.tmux_ok(&["show-hooks", "-t", session_id])
            .lines()
            .filter(|line| {
                line.split_once(' ').is_some_and(|(name, _)| {
                    name.starts_with("client-attached[")
                        || name.starts_with("client-session-changed[")
                })
            })
            .map(str::to_string)
            .collect()
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
    /// closed). The parked mirror's hidden window answers the tag through
    /// its pane and is masked, as hive's own scans mask it.
    fn team_windows(&self) -> Vec<(String, String)> {
        let out = self.tmux(&[
            "list-windows",
            "-a",
            "-F",
            "#{session_name}\t#{window_id}\t#{?@hive-hidden,,#{@hive-team}}",
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
    /// way a tmux client's shell would see it; an empty pane id is a
    /// `run-shell` job's view (TMUX set, no TMUX_PANE).
    fn hive_cmd(&self, args: &[&str], inside: Option<(&str, &str)>) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_hive"));
        cmd.args(args)
            .current_dir(self.tmp.path())
            .stdin(Stdio::null())
            .env("PATH", &self.path)
            .env("HOME", self.tmp.path())
            .env("HIVE_HOME", self.home())
            .env("CLAUDE_CONFIG_DIR", self.tmp.path().join("claude"))
            .env("CLAUDE_HOME", self.tmp.path().join("claude-home"))
            .env("CODEX_HOME", self.tmp.path().join("codex"))
            .env("GROK_HOME", self.tmp.path().join("grok"))
            .env("XDG_CACHE_HOME", self.tmp.path().join("cache"))
            .env("TMUX_TMPDIR", self.tmp.path())
            // What the hived reports to every team pane as its colours.
            .env("HIVE_VIEW_THEME", "light");
        for key in IDENTITY_VARS {
            cmd.env_remove(key);
        }
        if let Some(bin) = self.hive_bin() {
            cmd.env("HIVE_BIN", bin);
        }
        if let Some((socket, pane)) = inside {
            cmd.env("TMUX", format!("{socket},{},0", std::process::id()));
            if !pane.is_empty() {
                cmd.env("TMUX_PANE", pane);
            }
        }
        cmd
    }

    fn hive(&self, args: &[&str], inside: Option<(&str, &str)>) -> Output {
        self.hive_cmd(args, inside).output().expect("hive runs")
    }

    /// A stub `claude` on a private bin dir, *script* being its body after
    /// the shebang; returns the PATH that resolves it first.
    fn stub_claude(&self, script: &str) -> String {
        self.stub_cli("claude", script)
    }

    /// A stub *name* on the rig's private bin dir (the one every pane shell
    /// and hive process already searches first), *script* being its body
    /// after the shebang; returns the rig's PATH.
    fn stub_cli(&self, name: &str, script: &str) -> String {
        let stub = self.tmp.path().join("bin").join(name);
        std::fs::write(&stub, format!("#!/bin/sh\n{script}")).expect("stub cli");
        std::fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("stub cli mode");
        self.path.clone()
    }

    /// Roster rows `(name, cli, sessionId)` added to the team's registry
    /// entry under the store lock — members with an engine identity the
    /// heal draws a pane for, without a real engine ever being asked.
    fn add_members(&self, rows: &[(&str, &str, &str)]) {
        use std::os::unix::io::AsRawFd;
        let store = self.home().join("teams");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(store.join(".lock"))
            .expect("store lock");
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
        let path = store.join(&self.team).join("team.json");
        let mut entry: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("entry")).expect("json");
        let members = entry["members"].as_array_mut().expect("members");
        for (name, cli, sid) in rows {
            members.push(serde_json::json!({
                "name": name,
                "cli": cli,
                "sessionId": sid,
                "cwd": self.tmp.path(),
            }));
        }
        std::fs::write(&path, entry.to_string()).expect("entry written");
        let _ = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
    }

    /// `(@hive-agent, width x height, left,top)` of every pane in a window,
    /// in window order: the geometry of the arrangement, pane ids aside.
    fn cells(&self, window_id: &str) -> Vec<String> {
        self.tmux_ok(&[
            "list-panes",
            "-t",
            window_id,
            "-F",
            "#{@hive-agent} #{pane_width}x#{pane_height} #{pane_left},#{pane_top}",
        ])
        .lines()
        .map(str::to_string)
        .collect()
    }

    fn window_layout(&self, window_id: &str) -> String {
        self.tmux_ok(&["display-message", "-p", "-t", window_id, "#{window_layout}"])
    }

    /// The remembered arrangement of the rig's workspace, once its drag
    /// names *layout*; the hook that writes it runs off-loop.
    fn wait_for_remembered_drag(&self, layout: &str) -> Value {
        let path = self
            .ws()
            .join("state")
            .join("hive-arrangement")
            .join("window.json");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(doc) = serde_json::from_str::<Value>(&text) {
                    if doc["drag"]["layout"] == Value::String(layout.to_string()) {
                        return doc;
                    }
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no remembered drag for {layout} at {}",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// `hive` run by a live Claude session `me` (sessionId `s-me`) that
    /// *entrypoint* launched (`claude-desktop` for the desktop app, `cli`
    /// for a terminal): its registration names the inbox socket the
    /// process carries, and this test process is the pid behind it.
    /// `claude` on PATH is a stub whose job ledger is empty, so `s-me`
    /// reads as an interactive session — a mirror, never a resume —
    /// without the real CLI being asked.
    fn hive_as_claude(
        &self,
        args: &[&str],
        inside: Option<(&str, &str)>,
        entrypoint: &str,
    ) -> Output {
        // `CLAUDE_HOME` outranks `CLAUDE_CONFIG_DIR` in hive's config-dir
        // ladder, so the registration goes where the binary will look.
        let sessions = self.tmp.path().join("claude-home").join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");
        let socket = self.tmp.path().join("me.sock");
        std::fs::write(
            sessions.join("me.json"),
            serde_json::json!({
                "name": "me",
                "pid": std::process::id(),
                "messagingSocketPath": socket,
                "sessionId": "s-me",
                "cwd": self.tmp.path(),
                "kind": "interactive",
                "entrypoint": entrypoint,
            })
            .to_string(),
        )
        .expect("session registration");
        let path = self.stub_claude("echo '[]'\n");
        self.hive_cmd(args, inside)
            .env("CLAUDE_CODE_MESSAGING_SOCKET", &socket)
            .env("PATH", path)
            .output()
            .expect("hive runs")
    }

    fn hive_as_claude_ok(&self, args: &[&str], inside: Option<(&str, &str)>) -> String {
        let out = self.hive_as_claude(args, inside, "claude-desktop");
        assert!(
            out.status.success(),
            "hive {args:?} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// `(pane_id, @hive-role, @hive-agent, pane_width)` in window order.
    fn panes(&self, window_id: &str) -> Vec<(String, String, String, i64)> {
        self.tmux_ok(&[
            "list-panes",
            "-t",
            window_id,
            "-F",
            "#{pane_id}\t#{@hive-role}\t#{@hive-agent}\t#{pane_width}",
        ])
        .lines()
        .map(|line| {
            let mut parts = line.split('\t');
            (
                parts.next().unwrap_or_default().to_string(),
                parts.next().unwrap_or_default().to_string(),
                parts.next().unwrap_or_default().to_string(),
                parts.next().unwrap_or_default().parse().unwrap_or(-1),
            )
        })
        .collect()
    }

    fn window_option(&self, window_id: &str, key: &str) -> String {
        self.tmux_ok(&[
            "display-message",
            "-p",
            "-t",
            window_id,
            &format!("#{{@{key}}}"),
        ])
    }

    fn session_id(&self, session: &str) -> String {
        self.tmux_ok(&[
            "display-message",
            "-p",
            "-t",
            &format!("={session}:"),
            "#{session_id}",
        ])
    }

    /// Status line *n* of the window, rendered by tmux itself.
    fn status_line(&self, window_id: &str, n: usize) -> String {
        self.tmux_ok(&[
            "display-message",
            "-p",
            "-t",
            window_id,
            &format!("#{{T:status-format[{n}]}}"),
        ])
    }

    fn pane_pid(&self, pane_id: &str) -> String {
        self.tmux_ok(&["display-message", "-p", "-t", pane_id, "#{pane_pid}"])
    }

    /// `(window_id, pane_id, @hive-role)` of every pane in a window parked
    /// for the team (`@hive-hidden`).
    fn hidden_panes(&self, team: &str) -> Vec<(String, String, String)> {
        let windows: Vec<String> = self
            .tmux_ok(&["list-windows", "-a", "-F", "#{window_id}\t#{@hive-hidden}"])
            .lines()
            .filter_map(|line| {
                let (window, hidden) = line.split_once('\t')?;
                (hidden == team).then(|| window.to_string())
            })
            .collect();
        self.tmux_ok(&[
            "list-panes",
            "-a",
            "-F",
            "#{window_id}\t#{pane_id}\t#{@hive-role}",
        ])
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let window = parts.next()?.to_string();
            let pane = parts.next()?.to_string();
            let role = parts.next().unwrap_or_default().to_string();
            windows.contains(&window).then_some((window, pane, role))
        })
        .collect()
    }

    fn zoomed(&self, window_id: &str) -> bool {
        self.tmux_ok(&[
            "display-message",
            "-p",
            "-t",
            window_id,
            "#{window_zoomed_flag}",
        ]) == "1"
    }

    /// The server's root key table, one line per binding, sorted.
    fn root_keys(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .tmux_ok(&["list-keys", "-T", "root"])
            .lines()
            .map(str::to_string)
            .collect();
        lines.sort();
        lines
    }

    fn socket_path(&self) -> String {
        self.tmux_ok(&["display-message", "-p", "#{socket_path}"])
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
        hive::hived::socket_path(self.ws().to_str().unwrap()).exists()
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

/// `(session_name, control_mode)` of every client on the private server.
fn clients(rig: &Rig) -> Vec<(String, bool)> {
    let out = rig.tmux(&[
        "list-clients",
        "-F",
        "#{session_name}\t#{client_control_mode}",
    ]);
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (session, control) = line.split_once('\t')?;
            Some((session.to_string(), control == "1"))
        })
        .collect()
}

/// The control clients the team's hived has on record, `(pid, session
/// target)`, from the ledger `hive doctor` locates under the run dir.
fn control_client_ledger(path: &std::path::Path) -> Vec<(i64, String)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|doc| doc.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|row| {
            (
                row["pid"].as_i64().unwrap_or_default(),
                row["session"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !ready() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// *hold* must stay true for three of the hived's ticks.
fn hold_for_ticks(what: &str, mut hold: impl FnMut() -> bool) {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < until {
        assert!(hold(), "{what} did not hold");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
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
fn test_attach_inside_tmux_rebuilds_a_missing_window_in_the_team_session() {
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
        session, rig.team,
        "inside tmux the window is rebuilt in the team's session"
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
    assert!(session_alive(&rig, &rig.team));
    assert_eq!(
        rig.tmux_ok(&["show-options", "-t", &rig.team, "-v", "status"]),
        "2"
    );

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

/// A team with no window, queried from a pane of another session: its
/// hived is born there with no display and attaches its monitor to nothing
/// — never to the caller's session. Once `hive attach` rebuilds the
/// window, the same hived follows its display: into the team session, then
/// with the window into another session, and through that session's
/// rename without a restart. The control client on record is always the
/// session the live window's tags sit in.
#[test]
fn test_windowless_team_queried_from_a_foreign_pane_tracks_only_its_own_display() {
    let rig = Rig::new("foreign-query");
    let first_window = rig.create_outside_tmux();
    let other = format!("other-{}", std::process::id());
    let pane = rig.tmux_ok(&["new-session", "-d", "-s", &other, "-P", "-F", "#{pane_id}"]);
    let socket = rig.tmux_ok(&["display-message", "-p", "#{socket_path}"]);
    rig.tmux_ok(&["kill-window", "-t", &first_window]);
    assert!(!session_alive(&rig, &rig.team));
    assert!(!rig.hived_socket_exists(), "no hived before the query");

    // The explicit team query from the foreign pane starts the hived.
    let stdout = rig.hive_ok(&["team", "-t", &rig.team], Some((&socket, &pane)));
    let payload: Value = serde_json::from_str(&stdout).expect("team payload");
    assert_eq!(payload["name"], Value::String(rig.team.clone()));
    assert!(
        rig.hived_socket_exists(),
        "the query started the team's hived"
    );
    let owner_path = rig.ws().join("run").join("hived.owner.json");
    let owner = |path: &PathBuf| -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).expect("owner file"))
            .expect("owner json")
    };
    let born = owner(&owner_path);

    // No display: no control client, on the caller's session or any other.
    hold_for_ticks("no control client for a windowless team", || {
        clients(&rig).iter().all(|(_, control)| !control)
    });

    // The rebuilt window in the team session is where the monitor goes.
    rig.hive_ok(&["attach", &rig.team], Some((&socket, &pane)));
    let windows = rig.team_windows();
    assert_eq!(windows.len(), 1, "{windows:?}");
    let (session, window_id) = windows.into_iter().next().unwrap();
    assert_eq!(session, rig.team);
    let team_session = rig.session_id(&rig.team);
    // The ledger of control clients, where `hive doctor` says the run dir
    // is — run from the rebuilt window's shell pane, bound to the team the
    // way a human's shell pane in a team window is.
    let team_pane = rig.panes(&window_id)[0].0.clone();
    rig.tmux_ok(&[
        "set-option",
        "-p",
        "-t",
        &team_pane,
        "@hive-team",
        &rig.team,
    ]);
    rig.tmux_ok(&["set-option", "-p", "-t", &team_pane, "@hive-role", "shell"]);
    let doctor = rig.hive(&["doctor"], Some((&socket, &team_pane)));
    let report: Value = serde_json::from_str(&String::from_utf8_lossy(&doctor.stdout))
        .unwrap_or_else(|err| {
            panic!(
                "doctor json ({err}): stdout={} stderr={}",
                String::from_utf8_lossy(&doctor.stdout),
                String::from_utf8_lossy(&doctor.stderr)
            )
        });
    let run_dir = PathBuf::from(
        report["runDir"]
            .as_str()
            .unwrap_or_else(|| panic!("runDir in the doctor report: {report}")),
    );
    assert_eq!(run_dir, rig.ws().join("run"));
    let ledger = run_dir.join("control-clients.json");
    wait_until("the monitor on the team session", || {
        control_client_ledger(&ledger)
            .iter()
            .any(|(_, target)| *target == team_session)
            && clients(&rig).contains(&(rig.team.clone(), true))
    });
    assert_eq!(control_client_ledger(&ledger).len(), 1);
    assert!(
        !clients(&rig)
            .iter()
            .any(|(s, control)| *control && *s == other),
        "nothing watches the caller's session: {:?}",
        clients(&rig)
    );
    assert_eq!(owner(&owner_path), born, "the same hived generation");

    // The window moves whole into the other session: the monitor follows.
    rig.tmux_ok(&["move-window", "-s", &window_id, "-t", &format!("={other}:")]);
    assert!(!session_alive(&rig, &rig.team));
    let other_session = rig.session_id(&other);
    assert_ne!(other_session, team_session);
    wait_until("the monitor on the session the window moved to", || {
        control_client_ledger(&ledger)
            .iter()
            .all(|(_, target)| *target == other_session)
            && !control_client_ledger(&ledger).is_empty()
            && clients(&rig).contains(&(other.clone(), true))
    });
    let followed = control_client_ledger(&ledger);
    assert_eq!(followed.len(), 1, "{followed:?}");
    assert_eq!(
        rig.tmux_ok(&["display-message", "-p", "-t", &window_id, "#{session_id}"]),
        followed[0].1,
        "the control client targets the session the live window's tags sit in"
    );
    assert_eq!(owner(&owner_path), born);

    // A rename keeps the session: the same client, no restart.
    let renamed = format!("renamed-{}", std::process::id());
    rig.tmux_ok(&["rename-session", "-t", &format!("={other}"), &renamed]);
    hold_for_ticks("the monitor through a rename", || {
        control_client_ledger(&ledger) == followed
            && clients(&rig).contains(&(renamed.clone(), true))
    });
    assert_eq!(rig.session_id(&renamed), other_session);
    assert_eq!(owner(&owner_path), born);

    rig.delete();
    assert!(session_alive(&rig, &renamed), "the lent session stays");
}

/// The viewer count is asked by exact session: a `fern-dev` on the server
/// lends `fern` nothing, where a bare `-t fern` would have prefix-matched.
#[test]
fn test_viewer_count_never_borrows_a_prefix_matched_session() {
    require_tmux();
    let _server = private_server();
    let lookalike = format!("fern-dev-{}", std::process::id());
    let team = format!("fern-{}", std::process::id());
    let sibling = format!("{team}-dev");
    run_tmux(&["new-session", "-d", "-s", &sibling]);
    run_tmux(&["new-session", "-d", "-s", &lookalike]);
    let sibling_id = run_tmux(&[
        "display-message",
        "-p",
        "-t",
        &format!("={sibling}:"),
        "#{session_id}",
    ]);
    assert_eq!(
        hive::tmux::watching_clients(&team),
        None,
        "a session that does not exist has no count to give"
    );
    assert_eq!(hive::tmux::watching_clients(&sibling), Some(0));
    assert_eq!(hive::tmux::watching_clients(&sibling_id), Some(0));
    assert_eq!(hive::tmux::get_most_recent_client_window(Some(&team)), None);
    kill_session(&sibling);
    kill_session(&lookalike);
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

#[test]
fn test_create_with_a_claude_creator_installs_the_bar_and_mirror_off_on_round_trips() {
    let rig = Rig::new("mirror");
    // The server's stock key tables, read before hive ever touches them.
    let probe = format!("probe-{}", std::process::id());
    rig.tmux_ok(&["new-session", "-d", "-s", &probe]);
    let keys_before = rig.root_keys();
    let prefix_before = rig.tmux_ok(&["list-keys", "-T", "prefix"]);
    assert!(
        !prefix_before.contains("mirror --window"),
        "{prefix_before}"
    );

    let ws = rig.ws();
    rig.hive_as_claude_ok(
        &["create", &rig.team, "--workspace", ws.to_str().unwrap()],
        None,
    );
    let entry = rig.registry_entry().expect("registry entry");
    let roster: Vec<(String, String, String)> = entry["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["name"].as_str().unwrap_or_default().to_string(),
                m["cli"].as_str().unwrap_or_default().to_string(),
                m["sessionId"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert_eq!(
        roster,
        vec![("orch".to_string(), "claude".to_string(), "s-me".to_string())]
    );
    let (_, window_id) = rig.team_windows().into_iter().next().expect("team window");
    // The mirror starts collapsed: the window's one pane is a bare shell,
    // the choice recorded `off`, the chip closed.
    let panes = rig.panes(&window_id);
    assert_eq!(panes.len(), 1, "{panes:?}");
    assert_eq!((panes[0].1.as_str(), panes[0].2.as_str()), ("", ""));
    assert_eq!(rig.window_option(&window_id, "hive-mirror"), "off");
    let line = rig.status_line(&window_id, 0);
    assert!(line.contains(" ▴ orch "), "{line}");
    // `on` from no pane at all (the status click's run-shell) takes the
    // bare pane over: the same pane id is now the mirror.
    let target = rig.tmux_ok(&[
        "display-message",
        "-p",
        "-t",
        &window_id,
        "#{session_name}:#{window_index}",
    ]);
    let stdout = rig.hive_ok(&["mirror", "on", "--window", &target], None);
    assert_eq!(stdout, format!("mirror on ({})\n", rig.team));
    let opened = rig.panes(&window_id);
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert_eq!(
        (
            opened[0].0.as_str(),
            opened[0].1.as_str(),
            opened[0].2.as_str()
        ),
        (panes[0].0.as_str(), "mirror", "orch")
    );
    let mirror = panes[0].0.clone();
    assert_eq!(rig.window_option(&window_id, "hive-mirror"), "on");

    // The bar is the team session's own: two lines there, the probe
    // session and the global default untouched.
    let sid = rig.session_id(&rig.team);
    assert_eq!(
        rig.tmux_ok(&["show-options", "-t", &sid, "-v", "status"]),
        "2"
    );
    let probe_sid = rig.session_id(&probe);
    assert_eq!(
        rig.tmux_ok(&["show-options", "-t", &probe_sid, "-v", "status"]),
        ""
    );
    assert_eq!(rig.tmux_ok(&["show-options", "-g", "-v", "status"]), "on");
    let line = rig.status_line(&window_id, 0);
    assert!(line.contains(&format!(" {} ", rig.team)), "{line}");
    assert!(line.contains(" ▾ orch "), "{line}");
    assert!(!line.contains(" ▴ orch "), "{line}");

    // The window build changed exactly one root binding: the status click,
    // whose else branch is the stock click, plus prefix+m.
    let keys_after = rig.root_keys();
    let gone: Vec<&String> = keys_before
        .iter()
        .filter(|k| !keys_after.contains(k))
        .collect();
    let added: Vec<&String> = keys_after
        .iter()
        .filter(|k| !keys_before.contains(k))
        .collect();
    assert_eq!(gone.len(), 1, "removed: {gone:?}");
    assert_eq!(added.len(), 1, "added: {added:?}");
    assert!(gone[0].contains("MouseDown1Status"), "{}", gone[0]);
    assert!(added[0].contains("MouseDown1Status"), "{}", added[0]);
    assert!(added[0].contains("hive-mirror"), "{}", added[0]);
    assert!(added[0].contains("mirror --window"), "{}", added[0]);
    // The installed binding's else branch is the click the server had
    // (tmux's stock click, whichever this tmux ships), which the build also
    // remembered in the server option.
    let stock = hive::tmux::bound_command_for(gone[0], "root", "MouseDown1Status")
        .expect("the stock status click was bound");
    assert_eq!(
        rig.tmux_ok(&["show-options", "-s", "-v", "@hive-status-click"]),
        stock
    );
    // `list-keys` prints the nested command with its quotes escaped; the
    // else branch is that same stock click.
    assert!(
        added[0].trim_end_matches(['"', '\\']).ends_with(&stock),
        "{}",
        added[0]
    );
    for key in ["MouseDown1Pane", "MouseDrag1Border"] {
        let before: Vec<&String> = keys_before.iter().filter(|k| k.contains(key)).collect();
        let after: Vec<&String> = keys_after.iter().filter(|k| k.contains(key)).collect();
        assert_eq!(before, after, "{key}");
    }
    // prefix+m is gated on a team window; elsewhere it still runs what
    // the key ran before (this server loads the developer's tmux.conf, so
    // that is whatever they bound, else tmux's stock mark-pane), which the
    // build also remembered in the server option.
    let previous = hive::tmux::bound_command(&prefix_before).expect("prefix+m was bound");
    let prefix_after = rig
        .tmux_ok(&["list-keys", "-T", "prefix"])
        .lines()
        .find(|l| l.contains("mirror --window"))
        .map(str::to_string)
        .expect("prefix+m carries hive's binding");
    // `list-keys` prints the else branch double-quoted, inner quotes escaped.
    let printed = format!(
        "\"{}\"",
        previous.replace('\\', "\\\\").replace('"', "\\\"")
    );
    assert!(
        prefix_after.ends_with(&printed),
        "{prefix_after} vs {printed}"
    );
    assert_eq!(
        rig.tmux_ok(&["show-options", "-s", "-v", "@hive-prefix-m"]),
        previous
    );

    // A plain pane stands in for a member: roles are what the layout
    // tiles by, and `hive mirror` runs from it.
    let plain = rig.tmux_ok(&[
        "split-window",
        "-h",
        "-t",
        &window_id,
        "-P",
        "-F",
        "#{pane_id}",
    ]);
    let socket = rig.socket_path();
    let inside = Some((socket.as_str(), plain.as_str()));
    let pid0 = rig.pane_pid(&mirror);

    // `off` parks the pane — same id, same process — in a hidden window of
    // the team session, and the chip reads closed.
    let stdout = rig.hive_ok(&["mirror", "off"], inside);
    assert_eq!(stdout, format!("mirror off ({})\n", rig.team));
    let panes = rig.panes(&window_id);
    assert!(panes.iter().all(|p| p.1 != "mirror"), "{panes:?}");
    assert_eq!(panes.len(), 1, "{panes:?}");
    assert_eq!(panes[0].3, 220, "{panes:?}");
    let hidden = rig.hidden_panes(&rig.team);
    assert_eq!(hidden.len(), 1, "{hidden:?}");
    assert_eq!(
        (hidden[0].1.as_str(), hidden[0].2.as_str()),
        (mirror.as_str(), "mirror")
    );
    assert_eq!(rig.pane_pid(&mirror), pid0);
    assert_eq!(rig.window_option(&window_id, "hive-mirror"), "off");
    let line = rig.status_line(&window_id, 0);
    assert!(line.contains(" ▴ orch "), "{line}");

    // The heal respects the recorded absence — and finds the team window,
    // not the hidden one (whose pane answers `@hive-team` for it).
    let target = rig.tmux_ok(&[
        "display-message",
        "-p",
        "-t",
        &window_id,
        "#{session_name}:#{window_index}",
    ]);
    let stdout = rig.hive_ok(&["attach", &rig.team], inside);
    assert_eq!(stdout.trim_end(), format!("found {target}"));
    assert_eq!(rig.team_windows().len(), 1);
    assert!(rig.panes(&window_id).iter().all(|p| p.1 != "mirror"));
    assert_eq!(rig.hidden_panes(&rig.team).len(), 1);
    assert_eq!(rig.window_option(&window_id, "hive-mirror"), "off");

    // `off` with no mirror records the choice and touches nothing else.
    let stdout = rig.hive_ok(&["mirror", "off"], inside);
    assert_eq!(stdout, format!("mirror off ({}): no mirror\n", rig.team));
    assert_eq!(rig.panes(&window_id).len(), 1);

    // `on` joins the parked pane back as the main pane — no new viewer.
    let stdout = rig.hive_ok(&["mirror", "on"], inside);
    assert_eq!(stdout, format!("mirror on ({})\n", rig.team));
    let panes = rig.panes(&window_id);
    assert_eq!(panes.len(), 2, "{panes:?}");
    assert_eq!(
        (
            panes[0].0.as_str(),
            panes[0].1.as_str(),
            panes[0].2.as_str()
        ),
        (mirror.as_str(), "mirror", "orch"),
        "{panes:?}"
    );
    // The plan's mirror column: half the 220 columns less the separator.
    assert!((109..=110).contains(&panes[0].3), "{panes:?}");
    assert_eq!(panes[1].0, plain);
    assert!(rig.hidden_panes(&rig.team).is_empty());
    assert_eq!(rig.pane_pid(&mirror), pid0);
    assert_eq!(rig.window_option(&window_id, "hive-mirror"), "on");
    let line = rig.status_line(&window_id, 0);
    assert!(line.contains(" ▾ orch "), "{line}");

    // `on` with the mirror already up says so and keeps the human's zoom.
    rig.tmux_ok(&["resize-pane", "-Z", "-t", &plain]);
    assert!(rig.zoomed(&window_id));
    let stdout = rig.hive_ok(&["mirror", "on"], inside);
    assert_eq!(stdout, format!("mirror on ({}): already shown\n", rig.team));
    assert!(rig.zoomed(&window_id));

    // No argument toggles by presence; break-pane unzooms, so the survivor
    // is re-tiled to the whole window.
    let stdout = rig.hive_ok(&["mirror"], inside);
    assert_eq!(stdout, format!("mirror off ({})\n", rig.team));
    let panes = rig.panes(&window_id);
    assert!(panes.iter().all(|p| p.1 != "mirror"), "{panes:?}");
    assert!(!rig.zoomed(&window_id));
    assert_eq!(panes[0].3, 220, "{panes:?}");

    // The bindings' shape: a run-shell job — no caller pane — naming the
    // window.
    let stdout = rig.hive_ok(
        &["mirror", "--window", &target, "on"],
        Some((socket.as_str(), "")),
    );
    assert_eq!(stdout, format!("mirror on ({})\n", rig.team));
    assert_eq!(rig.panes(&window_id)[0].0, mirror);

    rig.hive_ok(&["mirror", "off"], inside);
    assert_eq!(rig.hidden_panes(&rig.team).len(), 1);
    rig.delete();
    // The hidden window went with the team: nothing keeps the session up.
    assert!(rig.hidden_panes(&rig.team).is_empty());
    assert!(!session_alive(&rig, &rig.team));
}

/// What the bar draws is tmux's rendering of options alone: set them the
/// way the hived and notify do, read the lines back.
#[test]
fn test_status_bar_reflects_pane_options_tmux_renders() {
    let rig = Rig::new("bar");
    let window_id = rig.create_outside_tmux();
    let split = |rig: &Rig| -> String {
        rig.tmux_ok(&["split-window", "-t", &window_id, "-P", "-F", "#{pane_id}"])
    };
    let set = |pane: &str, key: &str, value: &str| {
        rig.tmux_ok(&["set-option", "-p", "-t", pane, key, value]);
    };
    let busy = split(&rig);
    set(&busy, "@hive-role", "agent");
    set(&busy, "@hive-agent", "sage");
    set(&busy, "@hive-busy", "1");
    let unread = split(&rig);
    set(&unread, "@hive-role", "agent");
    set(&unread, "@hive-agent", "scout");
    set(&unread, "@hive-unread", "1");
    let attention = split(&rig);
    set(&attention, "@hive-role", "agent");
    set(&attention, "@hive-agent", "bee");
    set(&attention, "@hive-notify-active", "tok");
    rig.tmux_ok(&[
        "set-option",
        "-w",
        "-t",
        &window_id,
        "@hive-ticker",
        "x ##[y]",
    ]);
    rig.tmux_ok(&[
        "set-option",
        "-w",
        "-t",
        &window_id,
        "@hive-notify-text",
        "sage: hi",
    ]);

    let line = rig.status_line(&window_id, 0);
    assert!(line.contains(" ● sage "), "{line}");
    assert!(line.contains(" ✱ scout "), "{line}");
    assert!(line.contains(" ✱ bee "), "{line}");
    assert_eq!(line.matches(" ✱ ").count(), 2, "{line}");
    // No mirror choice recorded: no orch chip (the shell pane tagged as
    // the orch seat is an ordinary chip).
    assert!(
        !line.contains("▾ orch") && !line.contains("▴ orch"),
        "{line}"
    );
    let line = rig.status_line(&window_id, 1);
    assert!(line.contains("✱ sage: hi"), "{line}");
    // The option value is inserted verbatim, never re-expanded.
    assert!(line.ends_with("x ##[y]"), "{line}");

    rig.delete();
}

/// The hived's control client is what tmux answers a pane's OSC 11 query
/// from; on tmux 3.5+ the hived hands it hive's own appearance for every
/// pane of the team session, so an engine asking gets the configured
/// light background, not the control client's uninitialised black.
#[test]
fn test_create_outside_tmux_tells_team_panes_their_background_colour() {
    let rig = Rig::new("osc");
    if hive::tmux::version().is_none_or(|v| v < hive::tmux::PANE_COLOUR_REPORT_SINCE) {
        eprintln!("tmux without refresh-client -r: nothing to report through");
        return;
    }
    let window_id = rig.create_outside_tmux();
    // A create with no orch starts no hived; the first team query does,
    // and the hived attaches its control client to the team session.
    rig.hive_ok(&["team", "-t", &rig.team], None);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let control = rig.tmux_ok(&["list-clients", "-F", "#{client_control_mode}"]);
        if control.lines().any(|l| l == "1") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no control client attached to the team session"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    // A pane's own OSC 11 query, answered by tmux, read back off the tty.
    let pane = rig.panes(&window_id)[0].0.clone();
    let out = rig.tmp.path().join("osc11.txt");
    let probe = format!(
        "python3 -c \"import os,sys,termios,tty,select;fd=os.open('/dev/tty',os.O_RDWR);o=termios.tcgetattr(fd);tty.setraw(fd);os.write(fd,b'\\x1b]11;?\\x1b\\\\\\\\');b=b''\nwhile select.select([fd],[],[],1)[0]: b+=os.read(fd,256)\ntermios.tcsetattr(fd,termios.TCSADRAIN,o);open('{}','wb').write(b)\"",
        out.display()
    );
    rig.tmux_ok(&["send-keys", "-t", &pane, "-l", &probe]);
    rig.tmux_ok(&["send-keys", "-t", &pane, "Enter"]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !out.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the probe never wrote its reply"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    let reply = std::fs::read(&out).unwrap();
    let reply = String::from_utf8_lossy(&reply);
    assert!(
        reply.contains("]11;rgb:ffff/ffff/ffff"),
        "pane was told its background is {reply:?}"
    );
    rig.delete();
}

#[test]
fn test_create_and_join_refuse_a_terminal_claude_session() {
    let rig = Rig::new("terminal");
    let ws = rig.ws();
    let team_json = rig.home().join("teams").join(&rig.team).join("team.json");

    // A terminal's claude: refused before any tmux or registry write. The
    // socket-dir check must come before any `rig.tmux` call, which makes
    // that directory itself.
    let out = rig.hive_as_claude(
        &["create", &rig.team, "--workspace", ws.to_str().unwrap()],
        None,
        "cli",
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("terminal") && stderr.contains("hclaude"),
        "stderr: {stderr}"
    );
    assert!(!team_json.exists());
    assert!(!rig.socket_dir().exists());

    // An entry without an entrypoint: unconfirmed, not called a terminal.
    let out = rig.hive_as_claude(
        &["create", &rig.team, "--workspace", ws.to_str().unwrap()],
        None,
        "",
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unconfirmed") && stderr.contains("hclaude"),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("from a terminal"), "stderr: {stderr}");
    assert!(!team_json.exists());
    assert!(!rig.socket_dir().exists());

    // A shell's create, then the terminal claude asks to join: the
    // registry entry and the window are exactly as they were.
    let window_id = rig.create_outside_tmux();
    let entry_before = std::fs::read(&team_json).expect("team.json");
    let panes_before = rig.panes(&window_id);
    let out = rig.hive_as_claude(&["join", &rig.team], None, "cli");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("hclaude"), "stderr: {stderr}");
    assert_eq!(std::fs::read(&team_json).expect("team.json"), entry_before);
    assert_eq!(rig.panes(&window_id), panes_before);
    rig.tmux(&["kill-server"]);
}

#[test]
fn test_launchers_outside_tmux_without_a_terminal_run_the_raw_cli() {
    let rig = Rig::new("rawcli");
    let marker = rig.tmp.path().join("claude.args");
    let path = rig.stub_claude(&format!("echo \"$*\" >> {}\n", marker.display()));
    for args in [
        vec!["claude", "--help"],
        vec!["claude"],
        vec!["claude", "-p", "hi"],
    ] {
        // `hive_cmd` has stdin at /dev/null and stdout piped: no terminal.
        let out = rig
            .hive_cmd(&args, None)
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let seen = std::fs::read_to_string(&marker).expect("the stub ran");
    assert_eq!(seen, "--help\n\n-p hi\n");
    // The raw path started no server (it only ever asked `tmux -V`).
    assert!(!rig.socket_dir().exists());
}

#[test]
fn test_unmanaged_engine_cannot_create_a_shell_team_outside_tmux() {
    let rig = Rig::new("rawengine");
    for marker in [
        "CODEX_THREAD_ID",
        "GROK_SESSION_ID",
        "CLAUDE_CODE_MESSAGING_SOCKET",
    ] {
        let out = rig
            .hive_cmd(&["create", &rig.team], None)
            .env(marker, "unknown-engine-session")
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(!rig.socket_dir().exists());
    }
}

#[test]
fn test_the_first_member_takes_over_the_placeholder_a_collapsed_mirror_leaves() {
    let rig = Rig::new("placeholder");
    let ws = rig.ws();
    rig.hive_as_claude_ok(
        &["create", &rig.team, "--workspace", ws.to_str().unwrap()],
        None,
    );
    let (_, window) = rig.team_windows().into_iter().next().unwrap();
    // The collapsed mirror leaves one bare pane, marked as hive's own.
    let before = rig.panes(&window);
    assert_eq!(before.len(), 1, "{before:?}");
    assert_eq!((before[0].1.as_str(), before[0].2.as_str()), ("", ""));
    let placeholder = |pane: &str| {
        rig.tmux_ok(&[
            "show-options",
            "-p",
            "-q",
            "-v",
            "-t",
            pane,
            "@hive-placeholder",
        ])
    };
    assert_eq!(placeholder(&before[0].0), rig.team);

    // The heal draws the first roster member into that pane — same id, no
    // split — and the mark comes off with the takeover.
    rig.stub_cli("grok", "exit 0\n");
    rig.add_members(&[("sage", "grok", "sid-sage")]);
    let socket = rig.socket_path();
    rig.hive_ok(&["attach", &rig.team], Some((&socket, &before[0].0)));
    let after = rig.panes(&window);
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(
        (
            after[0].0.as_str(),
            after[0].1.as_str(),
            after[0].2.as_str()
        ),
        (before[0].0.as_str(), "agent", "sage")
    );
    assert_eq!(placeholder(&after[0].0), "");
    assert_eq!(rig.window_option(&window, "hive-mirror"), "off");
    rig.delete();
}

#[test]
fn test_attach_rebuilds_in_the_team_session_and_keeps_the_mirror_parked() {
    let rig = Rig::new("parked-heal");
    let ws = rig.ws();
    rig.hive_as_claude_ok(
        &["create", &rig.team, "--workspace", ws.to_str().unwrap()],
        None,
    );
    let (_, window) = rig.team_windows().into_iter().next().unwrap();
    // The mirror starts collapsed; open it so there is a pane to park.
    let target = rig.tmux_ok(&[
        "display-message",
        "-p",
        "-t",
        &window,
        "#{session_name}:#{window_index}",
    ]);
    rig.hive_ok(&["mirror", "on", "--window", &target], None);
    let mirror = rig.panes(&window)[0].0.clone();
    let pid = rig.pane_pid(&mirror);
    let plain = rig.tmux_ok(&[
        "split-window",
        "-d",
        "-t",
        &window,
        "-P",
        "-F",
        "#{pane_id}",
    ]);
    let socket = rig.socket_path();
    rig.hive_ok(&["mirror", "off"], Some((&socket, &plain)));
    rig.tmux_ok(&["kill-window", "-t", &window]);
    assert!(rig.team_windows().is_empty());
    assert!(session_alive(&rig, &rig.team));
    let human = rig.tmux_ok(&["new-session", "-d", "-s", "human", "-P", "-F", "#{pane_id}"]);

    rig.hive_ok(&["attach", &rig.team], Some((&socket, &human)));

    // The window is rebuilt in the team session — and the `hive mirror
    // off` the dead window recorded outlives it: the rebuilt window
    // withholds the mirror, whose parked pane stays parked.
    let (session, rebuilt) = rig.team_windows().into_iter().next().unwrap();
    assert_eq!(session, rig.team);
    assert_eq!(rig.window_option(&rebuilt, "hive-mirror"), "off");
    let panes = rig.panes(&rebuilt);
    assert!(panes.iter().all(|p| p.1 != "mirror"), "{panes:?}");
    let hidden = rig.hidden_panes(&rig.team);
    assert_eq!(hidden.len(), 1, "{hidden:?}");
    assert_eq!(hidden[0].1, mirror);
    assert_eq!(rig.pane_pid(&mirror), pid);

    // `on` joins that same pane back as the rebuilt window's first pane.
    let shell = panes[0].0.clone();
    rig.hive_ok(&["mirror", "on"], Some((&socket, &shell)));
    assert_eq!(rig.panes(&rebuilt)[0].0, mirror);
    assert_eq!(rig.pane_pid(&mirror), pid);
    assert!(rig.hidden_panes(&rig.team).is_empty());
    rig.delete();
    assert!(session_alive(&rig, "human"));
}

#[test]
fn test_attach_restores_the_dragged_arrangement_of_a_rebuilt_window() {
    let rig = Rig::new("arrange");
    // The rebuilt member panes run their engine's launcher; `grok` on the
    // rig's PATH is a stub that records the call and exits at once, so no
    // real engine is ever started and nothing outlives the pane. The panes
    // stay, dead, tags and all.
    let grok_calls = rig.tmp.path().join("grok-calls");
    rig.stub_cli(
        "grok",
        &format!("echo \"$@\" >> {}\nexit 0\n", grok_calls.display()),
    );
    let ws = rig.ws();
    rig.hive_as_claude_ok(
        &["create", &rig.team, "--workspace", ws.to_str().unwrap()],
        None,
    );
    rig.tmux_ok(&["set-option", "-g", "remain-on-exit", "on"]);
    let (_, window) = rig.team_windows().into_iter().next().unwrap();
    // The orch's mirror starts collapsed; this test wants it on screen.
    let target = rig.tmux_ok(&[
        "display-message",
        "-p",
        "-t",
        &window,
        "#{session_name}:#{window_index}",
    ]);
    rig.hive_ok(&["mirror", "on", "--window", &target], None);
    let socket = rig.socket_path();
    let mirror = rig.panes(&window)[0].0.clone();
    rig.add_members(&[("sage", "grok", "sid-sage"), ("scout", "grok", "sid-scout")]);
    rig.hive_ok(&["attach", &rig.team], Some((&socket, &mirror)));
    let panes = rig.panes(&window);
    let agents: Vec<&str> = panes.iter().map(|p| p.2.as_str()).collect();
    assert_eq!(agents, vec!["orch", "sage", "scout"], "{panes:?}");
    // The panes' launchers reached the stub, not an engine: the built
    // hive's `hive grok` spawned the stub as the leader and gave up on it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::fs::read_to_string(&grok_calls)
        .unwrap_or_default()
        .is_empty()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the grok stub was never run"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let calls = std::fs::read_to_string(&grok_calls).unwrap();
    assert!(calls.contains("agent leader"), "{calls}");
    let plan_key = rig.window_option(&window, "hive-layout");
    assert!(plan_key.contains("/m2/"), "{plan_key}");

    // The human swaps the two members and squeezes one: the hooks see the
    // plan's key unchanged and remember the window as it is now.
    rig.tmux_ok(&["swap-pane", "-d", "-s", &panes[1].0, "-t", &panes[2].0]);
    rig.tmux_ok(&["resize-pane", "-t", &panes[1].0, "-y", "12"]);
    let dragged = rig.window_layout(&window);
    let remembered = rig.wait_for_remembered_drag(&dragged);
    assert_eq!(
        remembered["drag"]["planKey"],
        Value::String(plan_key.clone())
    );
    let leaves: Vec<String> = remembered["drag"]["leaves"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["member"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(leaves, vec!["orch", "scout", "sage"]);
    let cells = rig.cells(&window);
    assert_eq!(rig.window_option(&window, "hive-layout"), plan_key);

    // The display dies; a human attaches from elsewhere.
    let human = rig.tmux_ok(&["new-session", "-d", "-s", "human", "-P", "-F", "#{pane_id}"]);
    rig.tmux_ok(&["kill-window", "-t", &window]);
    assert!(rig.team_windows().is_empty());
    rig.hive_ok(&["attach", &rig.team], Some((&socket, &human)));

    // The rebuilt window has new panes in roster order; the arrangement
    // puts them back where the human had them, cell for cell.
    let (session, rebuilt) = rig.team_windows().into_iter().next().unwrap();
    assert_eq!(session, rig.team);
    assert_ne!(rebuilt, window);
    let store = ws
        .join("state")
        .join("hive-arrangement")
        .join("window.json");
    let after = std::fs::read_to_string(&store).unwrap_or_default();
    assert_eq!(
        rig.cells(&rebuilt),
        cells,
        "key={} store={after} tags={}",
        rig.window_option(&rebuilt, "hive-layout"),
        rig.tmux_ok(&[
            "display-message",
            "-p",
            "-t",
            &rebuilt,
            "#{@hive-team}|#{@hive-workspace}|#{@hive-created}|#{window_layout}"
        ])
    );
    assert_eq!(rig.window_option(&rebuilt, "hive-layout"), plan_key);

    // `hive layout auto` is the way back to the plan: the drag is
    // forgotten with it.
    let shell = rig.panes(&rebuilt)[0].0.clone();
    let stdout = rig.hive_ok(&["layout", "auto"], Some((&socket, &shell)));
    assert!(stdout.contains("\"applied\":true"), "{stdout}");
    assert_ne!(rig.window_layout(&rebuilt), dragged);
    let doc: Value = std::fs::read_to_string(&store)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null);
    assert!(doc.get("drag").is_none(), "{doc}");
    rig.delete();
    assert!(session_alive(&rig, "human"));
}

// --- wake hooks -------------------------------------------------------------

fn owner(ws: &std::path::Path) -> Option<Value> {
    let text = std::fs::read_to_string(ws.join("run").join("hived.owner.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// `pid` and command line of every `--hived <ws>` process on the machine.
fn hived_processes(ws: &std::path::Path) -> Vec<(i64, String)> {
    let out = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .expect("ps runs");
    let needle = format!("--hived {} ", ws.display());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, command) = line.split_once(' ')?;
            if !command.contains(&needle) {
                return None;
            }
            Some((pid.parse().ok()?, command.trim().to_string()))
        })
        .collect()
}

/// The desk on *ws* answers a ping as *team* under *home*: the identity
/// the CLI accepts, not just a socket file.
fn matching_ping(ws: &std::path::Path, team: &str, home: &std::path::Path) -> bool {
    hive::hived::request_ping(ws.to_str().unwrap()).is_some_and(|identity| {
        identity["ok"] == Value::Bool(true)
            && identity["team"] == Value::String(team.to_string())
            && identity["hiveHome"] == Value::String(home.to_string_lossy().into_owned())
            && identity["apiVersion"] == hive::hived::HIVED_API_VERSION
    })
}

/// The `hive wake` jobs still running: both session hooks fire on an
/// attach, and the second one waits on the startup lock until the first
/// has the desk up — a straggler that a delete must not race.
fn wake_jobs() -> usize {
    let out = Command::new("ps")
        .args(["-axo", "command="])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.contains(" wake --session "))
        .count()
}

fn marker_path(ws: &std::path::Path) -> PathBuf {
    hive::hived::asleep_marker_path(ws.to_str().unwrap())
}

fn marker(ws: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(marker_path(ws)).ok()
}

/// The desk retires and leaves the marker a desk that nobody watched
/// leaves — the state a session hook finds after `hived.sleep unwatched`.
fn retire_unwatched(ws: &std::path::Path) {
    hive::hived::stop_hived(ws.to_str().unwrap());
    wait_until("the desk to leave", || {
        owner(ws).is_none() && hived_processes(ws).is_empty()
    });
    std::fs::create_dir_all(marker_path(ws).parent().unwrap()).unwrap();
    std::fs::write(marker_path(ws), "{\"reason\":\"unwatched\",\"at\":1}\n").unwrap();
}

/// The desk of *team* is up on *ws* under *home*: a live `--hived` process
/// whose pid the owner file names, answering a matching ping.
fn wait_for_desk(ws: &std::path::Path, team: &str, home: &std::path::Path) -> i64 {
    wait_until("a matching desk", || {
        owner(ws).is_some() && matching_ping(ws, team, home)
    });
    let pid = owner(ws).unwrap()["pid"].as_i64().expect("owner pid");
    let processes = hived_processes(ws);
    assert!(
        processes.iter().any(|(p, _)| *p == pid),
        "owner pid {pid} is not a live hived of {}: {processes:?}",
        ws.display()
    );
    pid
}

/// Two hive homes on one tmux server, the server's own environment naming
/// the wrong one: the hook home B installed on the shared session wakes
/// B's desk and B's alone, the same-named team of home A — a window in
/// the very same session, an unwatched marker of its own — untouched. A
/// window of B's team from an earlier instance wakes nothing.
#[test]
fn test_wake_hook_from_a_real_attach_wakes_only_the_baked_homes_instance() {
    let rig = Rig::new_under("wake-home", "home-b");
    let home_a = rig.tmp.path().join("home-a");
    let ws_a = rig.tmp.path().join("ws-a");
    std::fs::create_dir_all(&ws_a).unwrap();
    // The server is born from a client whose HIVE_HOME is home A: what
    // the hook would resolve if it carried no home of its own.
    let out = rig
        .tmux_cmd(&["new-session", "-d", "-s", "seed"])
        .env("HIVE_HOME", &home_a)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        rig.tmux_ok(&["show-environment", "-g", "HIVE_HOME"]),
        format!("HIVE_HOME={}", home_a.display())
    );

    // Home B's team, built with its hooks; its desk up, then retired unwatched.
    let window_b = rig.create_outside_tmux();
    let hooks = rig.wake_hooks();
    assert_eq!(hooks.len(), 2, "{hooks:?}");
    assert!(
        hooks
            .iter()
            .all(|h| h.contains(&format!("HIVE_HOME={}", rig.home().display()))),
        "{hooks:?}"
    );
    rig.hive_ok(&["team", "-t", &rig.team], None);
    wait_for_desk(&rig.ws(), &rig.team, &rig.home());
    retire_unwatched(&rig.ws());

    // Home A's same-named team, its window in the same session (a shell
    // pane there ran `hive create`), with an unwatched marker of its own.
    let pane = rig.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        &format!("={}:", rig.team),
        "-P",
        "-F",
        "#{pane_id}",
    ]);
    let socket = rig.socket_path();
    let out = rig
        .hive_cmd(
            &["create", &rig.team, "--workspace", ws_a.to_str().unwrap()],
            Some((&socket, &pane)),
        )
        .env("HIVE_HOME", &home_a)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "create under home A: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let windows = rig.team_windows();
    assert_eq!(windows.len(), 2, "{windows:?}");
    std::fs::create_dir_all(marker_path(&ws_a).parent().unwrap()).unwrap();
    std::fs::write(marker_path(&ws_a), "{\"reason\":\"unwatched\",\"at\":1}\n").unwrap();
    let marker_a = marker(&ws_a);
    assert!(owner(&ws_a).is_none());
    // The create from a shell pane installed nothing: still B's two entries.
    assert_eq!(rig.wake_hooks(), hooks);

    // A terminal arrives: the session hook wakes B's desk.
    let client = rig.attach_client(&[("HIVE_HOME", home_a.to_str().unwrap())]);
    let pid = wait_for_desk(&rig.ws(), &rig.team, &rig.home());
    assert!(
        marker(&rig.ws()).is_none(),
        "the woken desk clears its marker"
    );
    // Home A: no desk, no owner, no socket, the marker byte for byte.
    hold_for_ticks("home A untouched", || {
        owner(&ws_a).is_none()
            && hived_processes(&ws_a).is_empty()
            && !hive::hived::socket_path(ws_a.to_str().unwrap()).exists()
            && marker(&ws_a) == marker_a
    });
    let (_, argv) = hived_processes(&rig.ws())
        .into_iter()
        .find(|(p, _)| *p == pid)
        .unwrap();
    eprintln!("woken hived: pid {pid} argv {argv}");
    assert!(argv.contains(&format!("--hived {} {}", rig.ws().display(), rig.team)));
    // The woken desk reinstalled its hooks in place: still one entry each.
    assert_eq!(rig.wake_hooks(), hooks);
    drop(client);

    // The same window retagged as an earlier instance of B's team: the
    // hook fires again and starts nothing.
    retire_unwatched(&rig.ws());
    let marker_b = marker(&rig.ws());
    rig.tmux_ok(&["set-option", "-w", "-t", &window_b, "@hive-created", "1"]);
    let _client = rig.attach_client(&[]);
    hold_for_ticks("a stale instance stays asleep", || {
        owner(&rig.ws()).is_none()
            && hived_processes(&rig.ws()).is_empty()
            && marker(&rig.ws()) == marker_b
            && owner(&ws_a).is_none()
            && marker(&ws_a) == marker_a
    });
}

/// The rig's binary, hive home and workspace all sit on a path with a
/// space, `$`, a single and a double quote: the hook tmux stores runs
/// that path and wakes the desk on that workspace.
#[test]
fn test_wake_hook_executes_from_special_character_paths() {
    let rig = Rig::new_under("wake-quote", "we ird$'\"x");
    assert!(rig.home().to_str().unwrap().contains("$'\""));
    let bin = rig.hive_bin().unwrap();
    assert!(bin.exists());

    rig.create_outside_tmux();
    let hooks = rig.wake_hooks();
    assert_eq!(hooks.len(), 2, "{hooks:?}");
    eprintln!("stored hook: {}", hooks[0]);
    // tmux prints the stored command with its own escapes on top; undone,
    // the stored line carries the path shell-quoted, quote and dollar
    // intact — the space, `$`, `'` and `"` all went in escaped for tmux.
    let stored = hive::shell::tmux_dquote_unescape(&hooks[0]);
    let quoted_home = hive::shell::shlex_quote(rig.home().to_str().unwrap());
    assert!(
        stored.contains(&format!("HIVE_HOME={quoted_home} ")),
        "{stored}"
    );
    assert!(
        stored.contains(&format!(
            "{} wake --session #{{q:session_id}}",
            hive::shell::shlex_quote(bin.to_str().unwrap())
        )),
        "{stored}"
    );
    rig.hive_ok(&["team", "-t", &rig.team], None);
    wait_for_desk(&rig.ws(), &rig.team, &rig.home());
    retire_unwatched(&rig.ws());

    let _client = rig.attach_client(&[]);
    let pid = wait_for_desk(&rig.ws(), &rig.team, &rig.home());
    let (_, argv) = hived_processes(&rig.ws())
        .into_iter()
        .find(|(p, _)| *p == pid)
        .unwrap();
    eprintln!("woken hived: pid {pid} argv {argv}");
    // The desk was started by the hook's binary on the special-path
    // workspace: its argv names both, unmangled.
    assert!(
        argv.contains(&format!("--hived {} {}", rig.ws().display(), rig.team)),
        "{argv}"
    );
    assert!(argv.starts_with(bin.to_str().unwrap()), "{argv}");
    assert!(marker(&rig.ws()).is_none());
}

/// A wake whose desk cannot bind its socket leaves the unwatched marker
/// as it was, so the next terminal's arrival tries again; once the bind
/// can succeed the desk comes up and the marker goes.
#[test]
fn test_wake_after_a_failed_bind_keeps_the_marker_then_succeeds() {
    let rig = Rig::new("wake-bind");
    let window = rig.create_outside_tmux();
    rig.hive_ok(&["team", "-t", &rig.team], None);
    wait_for_desk(&rig.ws(), &rig.team, &rig.home());
    // Where this workspace's notify log is, from `hive doctor` while the
    // desk is up (a shell pane of the team window, bound to the team).
    let socket = rig.socket_path();
    let team_pane = rig.panes(&window)[0].0.clone();
    rig.tmux_ok(&[
        "set-option",
        "-p",
        "-t",
        &team_pane,
        "@hive-team",
        &rig.team,
    ]);
    rig.tmux_ok(&["set-option", "-p", "-t", &team_pane, "@hive-role", "shell"]);
    let doctor = rig.hive(&["doctor"], Some((&socket, &team_pane)));
    let report: Value = serde_json::from_str(&String::from_utf8_lossy(&doctor.stdout))
        .unwrap_or_else(|err| {
            panic!(
                "doctor json ({err}): stdout={} stderr={}",
                String::from_utf8_lossy(&doctor.stdout),
                String::from_utf8_lossy(&doctor.stderr)
            )
        });
    let notify_log = PathBuf::from(
        report["logs"]["notify"]
            .as_str()
            .unwrap_or_else(|| panic!("logs.notify in the doctor report: {report}")),
    );
    retire_unwatched(&rig.ws());
    let marker_before = marker(&rig.ws()).unwrap();

    // The socket path is taken by a directory: the spawned desk cannot
    // bind, reports it, and leaves the marker byte for byte.
    let socket_path = hive::hived::socket_path(rig.ws().to_str().unwrap());
    std::fs::create_dir_all(&socket_path).unwrap();
    let events = |name: &str| -> usize {
        std::fs::read_to_string(&notify_log)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(name))
            .count()
    };
    let starts_before = events("hived.start");
    let client = rig.attach_client(&[]);
    wait_until("the bind failure to be reported", || {
        events("hived.socket_bind_failed") >= 1
    });
    hold_for_ticks("the marker kept and no desk up", || {
        marker(&rig.ws()).as_deref() == Some(marker_before.as_str())
            && owner(&rig.ws()).is_none()
            && hived_processes(&rig.ws()).is_empty()
    });
    assert!(
        events("hived.start") > starts_before,
        "the hook did start a desk"
    );
    drop(client);

    // The obstacle gone, the next arrival wakes the desk for real.
    std::fs::remove_dir(&socket_path).unwrap();
    let _client = rig.attach_client(&[]);
    wait_for_desk(&rig.ws(), &rig.team, &rig.home());
    assert!(
        marker(&rig.ws()).is_none(),
        "the ready desk cleared the marker"
    );
    wait_until("the wake jobs to finish", || wake_jobs() == 0);
    rig.delete();
}

/// The team window is linked into a second, human session while the desk
/// is awake — the primary stays in the team session, and the human
/// session's terminals count as viewers from then on. The desk arms that
/// session too: after it retires unwatched, a terminal arriving at the
/// human session alone brings it back. Unlinking the window takes this
/// home's entries off that session again and leaves the team session's.
#[test]
fn test_wake_hook_on_a_second_session_of_the_display_wakes_the_desk() {
    let rig = Rig::new("wake-second");
    let window = rig.create_outside_tmux();
    rig.hive_ok(&["team", "-t", &rig.team], None);
    wait_for_desk(&rig.ws(), &rig.team, &rig.home());
    let team_session = rig.session_id(&rig.team);
    let hooks = rig.wake_hooks();
    assert_eq!(hooks.len(), 2, "{hooks:?}");

    // A human's own session, with nothing of hive's on it.
    rig.tmux_ok(&["new-session", "-d", "-s", "human"]);
    let human = rig.session_id("human");
    assert_ne!(human, team_session);
    assert!(rig.wake_hooks_on(&human).is_empty());
    rig.tmux_ok(&["link-window", "-d", "-s", &window, "-t", "human:"]);
    let linked: Vec<(String, String)> = rig
        .tmux_ok(&[
            "list-windows",
            "-t",
            "=human",
            "-F",
            "#{window_index}\t#{window_id}",
        ])
        .lines()
        .filter_map(|line| {
            let (index, id) = line.split_once('\t')?;
            (id == window).then(|| (index.to_string(), id.to_string()))
        })
        .collect();
    assert_eq!(linked.len(), 1, "{linked:?}");

    // The awake desk sees the second session and arms it: one entry per
    // hook there, the team session's untouched.
    wait_until("the wake hooks to reach the human session", || {
        rig.wake_hooks_on(&human).len() == 2
    });
    assert_eq!(rig.wake_hooks_on(&team_session), hooks);
    assert!(
        rig.wake_hooks_on(&human)
            .iter()
            .all(|h| h.contains(&format!("HIVE_HOME={}", rig.home().display()))),
        "{:?}",
        rig.wake_hooks_on(&human)
    );
    retire_unwatched(&rig.ws());

    // A terminal at the human session only: its hook wakes the desk.
    let client = rig.attach_session_client("human", &[]);
    assert!(
        !rig.tmux_ok(&[
            "list-clients",
            "-t",
            &format!("={}", rig.team),
            "-F",
            "#{client_pid}"
        ])
        .lines()
        .any(|line| !line.is_empty()),
        "nothing attached to the team session"
    );
    let pid = wait_for_desk(&rig.ws(), &rig.team, &rig.home());
    assert!(
        marker(&rig.ws()).is_none(),
        "the woken desk clears its marker"
    );
    let (_, argv) = hived_processes(&rig.ws())
        .into_iter()
        .find(|(p, _)| *p == pid)
        .unwrap();
    eprintln!("woken hived: pid {pid} argv {argv}");
    assert!(argv.contains(&format!("--hived {} {}", rig.ws().display(), rig.team)));
    // The woken desk reinstalled in place on both sessions: still one
    // entry each.
    assert_eq!(rig.wake_hooks_on(&team_session), hooks);
    assert_eq!(rig.wake_hooks_on(&human).len(), 2);
    wait_until("the wake jobs to finish", || wake_jobs() == 0);
    drop(client);

    // The window leaves the human session: this home's entries go with
    // it; the team session keeps its own.
    rig.tmux_ok(&["unlink-window", "-t", &format!("=human:{}", linked[0].0)]);
    wait_until("the entries to leave the human session", || {
        rig.wake_hooks_on(&human).is_empty()
    });
    assert_eq!(rig.wake_hooks_on(&team_session), hooks);
    rig.delete();
}
