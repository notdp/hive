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
    /// way a tmux client's shell would see it. `TERM_PROGRAM` is stripped:
    /// the self-mirror heuristic must never fire from the developer's
    /// shell.
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
            .env_remove("TERM_PROGRAM");
        for key in IDENTITY_VARS {
            cmd.env_remove(key);
        }
        if let Some((socket, pane)) = inside {
            cmd.env("TMUX", format!("{socket},{},0", std::process::id()))
                .env("TMUX_PANE", pane);
        }
        cmd
    }

    fn hive(&self, args: &[&str], inside: Option<(&str, &str)>) -> Output {
        self.hive_cmd(args, inside).output().expect("hive runs")
    }

    /// `hive` run by a live Claude session `me` (sessionId `s-me`): its
    /// registration names the inbox socket the process carries, and this
    /// test process is the pid behind it. `claude` on PATH is a stub whose
    /// job ledger is empty, so `s-me` reads as an interactive session — a
    /// mirror, never a resume — without the real CLI being asked.
    fn hive_as_claude(&self, args: &[&str], inside: Option<(&str, &str)>) -> Output {
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
            })
            .to_string(),
        )
        .expect("session registration");
        let bin = self.tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("stub bin dir");
        let stub = bin.join("claude");
        std::fs::write(&stub, "#!/bin/sh\necho '[]'\n").expect("stub claude");
        std::fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("stub claude mode");
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        self.hive_cmd(args, inside)
            .env("CLAUDE_CODE_MESSAGING_SOCKET", &socket)
            .env("PATH", path)
            .output()
            .expect("hive runs")
    }

    fn hive_as_claude_ok(&self, args: &[&str], inside: Option<(&str, &str)>) -> String {
        let out = self.hive_as_claude(args, inside);
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

#[test]
fn test_create_with_a_claude_creator_builds_the_rail_and_mirror_off_on_round_trips() {
    let rig = Rig::new("rail");
    // The server's stock key table, read before hive ever touches it.
    let probe = format!("probe-{}", std::process::id());
    rig.tmux_ok(&["new-session", "-d", "-s", &probe]);
    let keys_before = rig.root_keys();

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

    // The window build changed exactly one root binding: the click, whose
    // rail branch resizes and whose else branch is the stock click.
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
    assert!(gone[0].contains("MouseDown1Pane"), "{}", gone[0]);
    assert!(added[0].contains("MouseDown1Pane"), "{}", added[0]);
    assert!(added[0].contains("@hive-role"), "{}", added[0]);
    assert!(added[0].contains("resize-pane"), "{}", added[0]);
    // The stock click hive hard-codes is the one tmux shipped (`list-keys`
    // escapes its `;`), and the installed binding ends in that same text as
    // its else branch — a tmux that changes its stock click fails here.
    assert!(
        gone[0]
            .replace(r"\;", ";")
            .ends_with(hive::tmux::_STOCK_CLICK),
        "{}",
        gone[0]
    );
    assert!(
        added[0]
            .trim_end_matches('"')
            .ends_with(hive::tmux::_STOCK_CLICK),
        "{}",
        added[0]
    );
    for key in ["MouseDown1Status", "MouseDrag1Border"] {
        let before: Vec<&String> = keys_before.iter().filter(|k| k.contains(key)).collect();
        let after: Vec<&String> = keys_after.iter().filter(|k| k.contains(key)).collect();
        assert_eq!(before, after, "{key}");
    }

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

    let stdout = rig.hive_ok(&["mirror", "off"], inside);
    assert_eq!(stdout, format!("mirror off ({})\n", rig.team));
    assert!(
        rig.panes(&window_id).iter().all(|p| p.1 != "mirror"),
        "{:?}",
        rig.panes(&window_id)
    );
    assert_eq!(rig.window_option(&window_id, "hive-mirror"), "off");

    // The heal respects the recorded absence.
    let stdout = rig.hive_ok(&["attach", &rig.team], inside);
    assert!(stdout.starts_with("found "), "{stdout}");
    assert!(rig.panes(&window_id).iter().all(|p| p.1 != "mirror"));

    // `off` with no rail records the choice and touches nothing else.
    let stdout = rig.hive_ok(&["mirror", "off"], inside);
    assert_eq!(stdout, format!("mirror off ({}): no rail\n", rig.team));
    assert_eq!(rig.panes(&window_id).len(), 1);

    let stdout = rig.hive_ok(&["mirror", "on"], inside);
    assert_eq!(stdout, format!("mirror on ({})\n", rig.team));
    let panes = rig.panes(&window_id);
    assert_eq!(panes.len(), 2, "{panes:?}");
    assert_eq!(
        (panes[0].1.as_str(), panes[0].2.as_str(), panes[0].3),
        ("mirror", "orch", 14),
        "{panes:?}"
    );
    assert_eq!(panes[1].0, plain);
    assert_eq!(panes[1].3, 220 - 15, "{panes:?}");
    assert_eq!(rig.window_option(&window_id, "hive-mirror"), "on");

    // `on` with the rail already up says so and keeps the human's zoom.
    rig.tmux_ok(&["resize-pane", "-Z", "-t", &plain]);
    assert!(rig.zoomed(&window_id));
    let stdout = rig.hive_ok(&["mirror", "on"], inside);
    assert_eq!(
        stdout,
        format!("mirror on ({}): no session mirror to show\n", rig.team)
    );
    assert!(rig.zoomed(&window_id));
    rig.tmux_ok(&["resize-pane", "-Z", "-t", &plain]);
    assert!(!rig.zoomed(&window_id));

    // The click binding's rail branch, run by tmux's own parser exactly as
    // the binding carries it (the mouse pane `=` being the rail): opens to
    // 45% of the 220 columns, then folds back to the rail.
    let rail = panes[0].0.clone();
    let then = hive::tmux::mirror_click_binding()[9].replace('=', &rail);
    rig.tmux_ok(&["if-shell", "-F", "-t", &rail, "1", &then]);
    assert_eq!(rig.panes(&window_id)[0].3, 99);
    rig.tmux_ok(&["if-shell", "-F", "-t", &rail, "1", &then]);
    assert_eq!(rig.panes(&window_id)[0].3, 14);

    // No argument toggles by presence; the kill unzooms, so the survivor
    // is re-tiled to the whole window.
    rig.tmux_ok(&["resize-pane", "-Z", "-t", &plain]);
    let stdout = rig.hive_ok(&["mirror"], inside);
    assert_eq!(stdout, format!("mirror off ({})\n", rig.team));
    let panes = rig.panes(&window_id);
    assert!(panes.iter().all(|p| p.1 != "mirror"), "{panes:?}");
    assert!(!rig.zoomed(&window_id));
    assert_eq!(panes[0].3, 220, "{panes:?}");

    rig.delete();
}

#[test]
fn test_flow_rig_mirror_pane_is_the_rail() {
    let rig = Rig::new("flowrig");
    // The mirror pane is `hive view s-rig` with nothing else keeping the
    // pane open, and the viewer looks under `$HOME/.claude/projects` — so
    // the run's HOME is the rig and the transcript exists (empty: the
    // viewer follows it live).
    let project = rig.tmp.path().join(".claude").join("projects").join("p");
    std::fs::create_dir_all(&project).expect("projects dir");
    std::fs::write(project.join("s-rig.jsonl"), "").expect("transcript");
    let out = rig
        .hive_cmd(&["flow", "rig", &rig.team, "--orch", "s-rig"], None)
        .env("HOME", rig.tmp.path())
        .output()
        .expect("hive runs");
    assert!(
        out.status.success(),
        "flow rig failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (_, window_id) = rig.team_windows().into_iter().next().expect("rig window");
    let panes = rig.panes(&window_id);
    assert_eq!(panes.len(), 2, "{panes:?}");
    assert_eq!(
        (panes[0].1.as_str(), panes[0].3),
        ("mirror", 14),
        "{panes:?}"
    );
    assert_eq!(
        (panes[1].1.as_str(), panes[1].3),
        ("dock", 205),
        "{panes:?}"
    );
    rig.hive_ok(&["flow", "rig", &rig.team, "--down"], None);
    assert!(rig.registry_entry().is_none());
}
