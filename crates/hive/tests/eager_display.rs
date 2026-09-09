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

/// hive's root variables a launcher mirrors into the session it opens.
const ROOT_VARS: &[&str] = &[
    "HIVE_HOME",
    "CLAUDE_HOME",
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "GROK_HOME",
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
            .env("TMUX_TMPDIR", self.tmp.path());
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
        let bin = self.tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("stub bin dir");
        let stub = bin.join("claude");
        std::fs::write(&stub, format!("#!/bin/sh\n{script}")).expect("stub claude");
        std::fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("stub claude mode");
        format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        )
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
    let panes = rig.panes(&window_id);
    assert_eq!(panes.len(), 1, "{panes:?}");
    assert_eq!(
        (panes[0].1.as_str(), panes[0].2.as_str()),
        ("mirror", "orch")
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

/// The launcher run at a terminal (a pty from `script`) outside tmux, on a
/// private server that already holds stale root globals. One session per
/// caller state of the root variables — unset, a lane, explicitly empty —
/// each held open by the stub `claude` the managed launch falls back to
/// (its `--bg` fails), until the test has read the roots the first pane, a
/// split pane and a window hook's run-shell job see.
#[test]
fn test_launcher_outside_tmux_at_a_terminal_opens_a_session_mirroring_its_roots() {
    let rig = Rig::new("session");
    let home = rig.tmp.path().join("home");
    let old = rig.tmp.path().join("old");
    let lane = rig.tmp.path().join("lane");
    for dir in [&home, &old, &lane] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let old_roots: Vec<(&str, String)> = ROOT_VARS
        .iter()
        .map(|k| {
            (
                *k,
                old.join(k.to_lowercase()).to_string_lossy().into_owned(),
            )
        })
        .collect();
    // The server, with stale globals: HOME is the rig's so nothing here
    // reads the developer's own roots when a variable is unset.
    // The server's global environment is this first client's: the engine
    // markers a developer's shell carries would otherwise reach every pane.
    let mut base = Command::new("tmux");
    base.args(["-S", rig.socket_dir().join("default").to_str().unwrap()])
        .args(["new-session", "-d", "-s", "base"])
        .env("TMUX_TMPDIR", rig.tmp.path())
        .env("HOME", &home);
    for key in IDENTITY_VARS {
        base.env_remove(key);
    }
    for (k, v) in &old_roots {
        base.env(k, v);
    }
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(rig.socket_dir())
        .unwrap();
    assert!(base.output().unwrap().status.success());
    assert_eq!(
        rig.tmux_ok(&["show-environment", "-g", "HIVE_HOME"]),
        format!("HIVE_HOME={}", old_roots[0].1)
    );

    let dump = |tag: &str| {
        format!(
            "printf '%s\\n' \"{tag}\" \"HIVE_HOME=${{HIVE_HOME-UNSET}}\" \"CLAUDE_HOME=${{CLAUDE_HOME-UNSET}}\" \
             \"CLAUDE_CONFIG_DIR=${{CLAUDE_CONFIG_DIR-UNSET}}\" \"CODEX_HOME=${{CODEX_HOME-UNSET}}\" \
             \"GROK_HOME=${{GROK_HOME-UNSET}}\" \"TMUX_PANE=${{TMUX_PANE-UNSET}}\" \"HOME=$HOME\""
        )
    };
    let hive = env!("CARGO_BIN_EXE_hive");
    let planted = |root: &std::path::Path, team: &str| {
        let dir = root.join("teams").join(team);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("team.json"),
            format!(
                r#"{{"team":"{team}","workspace":"{}","members":[]}}"#,
                dir.display()
            ),
        )
        .unwrap();
    };
    planted(&home.join(".hive"), "unset-team");
    planted(&lane, "lane-team");
    planted(&old.join("hive_home"), "old-team");

    for (state, value, expect, root_team) in [
        ("unset", None, "UNSET", "unset-team"),
        (
            "lane",
            Some(lane.to_string_lossy().into_owned()),
            lane.to_str().unwrap(),
            "lane-team",
        ),
        ("empty", Some(String::new()), "", ""),
    ] {
        let out = rig.tmp.path().join(format!("{state}.out"));
        let go = rig.tmp.path().join(format!("{state}.go"));
        let path = rig.stub_claude(&format!(
            "{} >> {out}\ncase \"$1\" in --bg) exit 1;; esac\nwhile [ ! -e {go} ]; do sleep 0.1; done\n",
            dump("claude:$*"),
            out = out.display(),
            go = go.display()
        ));
        // A pty around the launcher: `script` on macOS takes the command
        // after the typescript file, util-linux's wants -c.
        let mut cmd = Command::new("script");
        if cfg!(target_os = "macos") {
            cmd.args(["-q", "/dev/null", hive, "claude"]);
        } else {
            cmd.args(["-qec", &format!("{hive} claude"), "/dev/null"]);
        }
        cmd.current_dir(rig.tmp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env("HOME", &home)
            .env("TMUX_TMPDIR", rig.tmp.path())
            .env("PATH", &path)
            // The tmux client needs a terminal that clears; nextest's is dumb.
            .env("TERM", "xterm-256color");
        for key in IDENTITY_VARS.iter().chain(ROOT_VARS) {
            cmd.env_remove(key);
        }
        if let Some(value) = &value {
            for key in ROOT_VARS {
                cmd.env(key, value);
            }
        }
        let mut child = cmd.spawn().expect("script runs");
        // The stub's second entry is the raw launch holding the pane.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::fs::read_to_string(&out).map_or(true, |s| s.matches("claude:").count() < 2) {
            assert!(
                std::time::Instant::now() < deadline,
                "{state}: launcher never held a pane"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // The raw launch's own record (the second): the `--bg` attempt's
        // fields must not stand in for it.
        let first = std::fs::read_to_string(&out).unwrap();
        let first = first
            .split("claude:")
            .nth(2)
            .map(|record| format!("claude:{record}"))
            .expect("the raw launch's record");
        let session = rig
            .tmux_ok(&["list-sessions", "-F", "#{session_name}"])
            .lines()
            .find(|s| *s != "base")
            .expect("the launcher's session")
            .to_string();
        let window = rig.tmux_ok(&["display-message", "-p", "-t", &session, "#{window_id}"]);

        // A split pane's view, then a window hook's run-shell job's view.
        let split_out = rig.tmp.path().join(format!("{state}.split"));
        rig.tmux_ok(&[
            "split-window",
            "-d",
            "-t",
            &session,
            &format!("{} > {}", dump("split"), split_out.display()),
        ]);
        // The hook's job runs a script file: the dump's quoting stays out
        // of tmux's double-quote parser.
        let hook_out = rig.tmp.path().join(format!("{state}.hook"));
        let hook_sh = rig.tmp.path().join(format!("{state}.hook.sh"));
        std::fs::write(
            &hook_sh,
            format!("{} > {}\n", dump("hook"), hook_out.display()),
        )
        .unwrap();
        let hook = format!(
            "run-shell -b \"sh {} >/dev/null 2>&1 || true\"",
            hook_sh.display()
        );
        rig.tmux_ok(&[
            "set-hook",
            "-w",
            "-t",
            &window,
            "window-layout-changed",
            &hook,
        ]);
        let ls_out = rig.tmp.path().join(format!("{state}.ls"));
        rig.tmux_ok(&[
            "split-window",
            "-d",
            "-t",
            &session,
            &format!(
                "{hive} ls > {out} 2>&1; echo rc=$? >> {out}",
                out = ls_out.display()
            ),
        ]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !(split_out.exists() && hook_out.exists() && ls_out.exists()) {
            assert!(
                std::time::Instant::now() < deadline,
                "{state}: split/hook/ls never reported"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        let split = std::fs::read_to_string(&split_out).unwrap();
        let hooked = std::fs::read_to_string(&hook_out).unwrap();
        for (label, text) in [
            ("first pane", first.as_str()),
            ("split", split.as_str()),
            ("hook", hooked.as_str()),
        ] {
            for key in ROOT_VARS {
                assert!(
                    text.contains(&format!("{key}={expect}\n")),
                    "{state} {label}: {key} should be {expect:?}\n{text}"
                );
            }
            assert!(
                text.contains(&format!("HOME={}\n", home.display())),
                "{state} {label}:\n{text}"
            );
        }
        assert!(
            first.contains("TMUX_PANE=%"),
            "{state} first pane:\n{first}"
        );
        // The registry the pane's hive reads is the caller's root, not the
        // server's old one.
        let ls = std::fs::read_to_string(&ls_out).unwrap();
        assert!(ls.contains("rc=0\n"), "{state} ls:\n{ls}");
        if root_team.is_empty() {
            // An empty root is a root (`paths::hive_home` takes the value as
            // it is): no planting is under it.
            assert!(!ls.contains("-team"), "{state} ls:\n{ls}");
        } else {
            assert!(
                ls.contains(root_team) && !ls.contains("old-team"),
                "{state} ls:\n{ls}"
            );
        }
        // The session's own environment says the same; the globals and the
        // base session are untouched.
        for key in ROOT_VARS {
            let shown = rig.tmux_ok(&["show-environment", "-t", &session, key]);
            let want = match &value {
                None => format!("-{key}"),
                Some(v) => format!("{key}={v}"),
            };
            assert_eq!(shown, want, "{state}");
        }
        for (key, old_value) in &old_roots {
            assert_eq!(
                rig.tmux_ok(&["show-environment", "-g", key]),
                format!("{key}={old_value}"),
                "{state}"
            );
            // `-r` wrote into the new session alone: base still has no
            // variable of its own (it reads the global).
            assert!(
                !rig.tmux(&["show-environment", "-t", "base", key])
                    .status
                    .success(),
                "{state} {key}"
            );
        }

        // A team created in the launcher's session wears the team bar; the
        // session is the human's (their engine holds the first pane), so
        // `hive delete` leaves it, as it leaves any lent window.
        if state == "unset" {
            let team = format!("{}-launched", rig.team);
            let create_out = rig.tmp.path().join("create.out");
            rig.tmux_ok(&[
                "split-window",
                "-d",
                "-t",
                &session,
                &format!(
                    "{hive} create {team} > {out} 2>&1; echo rc=$? >> {out}",
                    out = create_out.display()
                ),
            ]);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !std::fs::read_to_string(&create_out).is_ok_and(|s| s.contains("rc=")) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "create never reported"
                );
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let created = std::fs::read_to_string(&create_out).unwrap();
            assert!(created.contains("rc=0\n"), "create:\n{created}");
            let sid = rig.tmux_ok(&["display-message", "-p", "-t", &session, "#{session_id}"]);
            assert_eq!(
                rig.tmux_ok(&["show-options", "-t", &sid, "-v", "status"]),
                "2",
                "the launcher's session wears the team bar"
            );
            assert!(
                rig.status_line(&window, 0).contains(&format!(" {team} ")),
                "{}",
                rig.status_line(&window, 0)
            );
            assert_eq!(rig.window_option(&window, "hive-built"), "");
            // base, the human's other session, is untouched
            assert_eq!(
                rig.tmux_ok(&["show-options", "-t", "base", "-v", "status"]),
                ""
            );
            rig.tmux_ok(&[
                "split-window",
                "-d",
                "-t",
                &session,
                &format!("{hive} delete {team} > {}.del 2>&1", create_out.display()),
            ]);
            let del_out = rig.tmp.path().join("create.out.del");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !del_out.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "delete never reported"
                );
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            assert!(
                rig.tmux(&["has-session", "-t", &format!("={session}")])
                    .status
                    .success(),
                "delete must leave the launcher's session"
            );
            assert!(std::fs::read_to_string(&out).unwrap().contains("claude:"));
        }

        std::fs::write(&go, "").unwrap();
        let status = child.wait().expect("script exits");
        assert!(status.success(), "{state}: {status:?}");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while rig
            .tmux(&["has-session", "-t", &format!("={session}")])
            .status
            .success()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "{state}: session lingered"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    rig.tmux(&["kill-server"]);
}
