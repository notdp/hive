//! Real-tmux e2e for the auto layout: the two window hooks re-plan a team
//! window on a resize and a kill-pane with no hive call from the test, a
//! matching key leaves a human's border drag alone through a proportional
//! resize, a window hive builds itself carries the hooks, and a window a
//! human's session lent the team loses them at `hive delete`. Every test
//! runs the built binary against a private tmux server on its own `-S`
//! socket (a short 0700 directory under /tmp — unix socket paths are
//! capped) and a temp `HIVE_HOME`, so the developer's server never sees a
//! window.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

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
    /// The socket directory: short, 0700, removed on drop.
    dir: PathBuf,
    tmp: tempfile::TempDir,
    team: String,
}

impl Rig {
    fn new(tag: &str) -> Self {
        require_tmux();
        use std::os::unix::fs::DirBuilderExt;
        let dir = PathBuf::from(format!("/tmp/hl.{}", std::process::id()));
        // The socket sits where `TMUX_TMPDIR` resolves it, so a `hive`
        // that starts the server itself (test 2) and a `-S` client meet on
        // the same path; tmux refuses any mode but 0700 on that directory.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir.join(format!("tmux-{}", unsafe { libc::getuid() })))
            .expect("socket dir");
        let tmp = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(tmp.path().join("ws")).expect("workspace dir");
        Rig {
            dir,
            tmp,
            team: format!("hivetest-{tag}-{}", std::process::id()),
        }
    }

    fn socket(&self) -> PathBuf {
        self.dir
            .join(format!("tmux-{}", unsafe { libc::getuid() }))
            .join("default")
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join(".hive")
    }

    /// The env every process on the private server gets — the tmux server
    /// inherits it from the client that starts it, and hands it to the
    /// hook's `run-shell` jobs, which is how a hook-run `hive` finds the
    /// same `HIVE_HOME` (its lock directory) and never the developer's.
    fn env(&self, cmd: &mut Command) {
        cmd.env("HIVE_HOME", self.home())
            .env("CLAUDE_CONFIG_DIR", self.tmp.path().join("claude"))
            .env("CLAUDE_HOME", self.tmp.path().join("claude-home"))
            .env("CODEX_HOME", self.tmp.path().join("codex"))
            .env("GROK_HOME", self.tmp.path().join("grok"))
            .env("XDG_CACHE_HOME", self.tmp.path().join("cache"))
            .env("TMUX_TMPDIR", &self.dir);
        for key in IDENTITY_VARS {
            cmd.env_remove(key);
        }
    }

    fn tmux(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new("tmux");
        cmd.arg("-u").arg("-S").arg(self.socket()).args(args);
        self.env(&mut cmd);
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

    /// The built binary the way a `run-shell` job sees it: TMUX naming the
    /// private socket, no TMUX_PANE.
    fn hive(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_hive"));
        cmd.args(args)
            .current_dir(self.tmp.path())
            .stdin(Stdio::null());
        self.env(&mut cmd);
        cmd.env(
            "TMUX",
            format!("{},{},0", self.socket().display(), std::process::id()),
        );
        cmd.output().expect("hive runs")
    }

    /// The built binary run from inside `pane` on the private server, the
    /// way a human's shell there would run it.
    fn hive_in_pane(&self, args: &[&str], pane: &str) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_hive"));
        cmd.args(args)
            .current_dir(self.tmp.path())
            .stdin(Stdio::null());
        self.env(&mut cmd);
        cmd.env(
            "TMUX",
            format!("{},{},0", self.socket().display(), std::process::id()),
        )
        .env("TMUX_PANE", pane);
        cmd.output().expect("hive runs")
    }

    fn hive_in_pane_ok(&self, args: &[&str], pane: &str) -> String {
        let out = self.hive_in_pane(args, pane);
        assert!(
            out.status.success(),
            "hive {args:?} in {pane} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// The lock files the appliers took, by name.
    fn lock_files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.home().join("state").join("locks"))
            .map(|dir| {
                dir.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    // a rerun marker a yielding hook left behind is not a lock
                    .filter(|name| name.ends_with(".lock"))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn hive_ok(&self, args: &[&str]) -> String {
        let out = self.hive(args);
        assert!(
            out.status.success(),
            "hive {args:?} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn display(&self, target: &str, format: &str) -> String {
        self.tmux_ok(&["display-message", "-p", "-t", target, format])
    }

    fn layout(&self, window: &str) -> String {
        self.display(window, "#{window_layout}")
    }

    fn key(&self, window: &str) -> String {
        self.display(window, "#{@hive-layout}")
    }

    /// `(pane_id, role, left, width)` in window order.
    fn panes(&self, window: &str) -> Vec<(String, String, i64, i64)> {
        self.tmux_ok(&[
            "list-panes",
            "-t",
            window,
            "-F",
            "#{pane_id}\t#{@hive-role}\t#{pane_left}\t#{pane_width}",
        ])
        .lines()
        .map(|line| {
            let mut parts = line.split('\t');
            (
                parts.next().unwrap_or_default().to_string(),
                parts.next().unwrap_or_default().to_string(),
                parts.next().unwrap_or_default().parse().unwrap_or(-1),
                parts.next().unwrap_or_default().parse().unwrap_or(-1),
            )
        })
        .collect()
    }

    fn pane_infos(&self, window: &str) -> Vec<hive::tmux::PaneInfo> {
        self.panes(window)
            .into_iter()
            .map(|(pane_id, role, _, _)| hive::tmux::PaneInfo {
                pane_id,
                role,
                ..Default::default()
            })
            .collect()
    }

    /// The planner's answer for the window as tmux has it now.
    fn plan(&self, window: &str, size: (i64, i64)) -> hive::layout::Plan {
        hive::layout::plan(size, &self.pane_infos(window)).expect("a plan")
    }

    /// Poll until the window carries `expected` — its layout string and,
    /// written after it, its key (the hook runs as a background job); the
    /// layout strings seen when it never arrives.
    fn wait_for_plan(
        &self,
        window: &str,
        expected: &hive::layout::Plan,
    ) -> Result<(), Vec<String>> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        loop {
            let now = self.layout(window);
            if now == expected.layout && self.key(window) == expected.key {
                return Ok(());
            }
            if seen.last() != Some(&now) {
                seen.push(now);
            }
            if Instant::now() > deadline {
                return Err(seen);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Give any hook job fired by the last tmux command time to run and
    /// (wrongly) write.
    fn settle(&self) {
        std::thread::sleep(Duration::from_millis(500));
    }
}

impl Drop for Rig {
    // Best effort, panic or not: a team the test made (and its hived)
    // released, then the private server with everything on it.
    fn drop(&mut self) {
        let _ = self.hive(&["delete", &self.team, "--delete-workspace"]);
        let _ = self.tmux(&["kill-server"]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn test_layout_hooks_replan_on_flip_keep_a_drag_through_a_proportional_resize_and_replan_on_kill() {
    let rig = Rig::new("hooks");
    let session = &rig.team;
    let mirror = rig.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        session,
        "-x",
        "220",
        "-y",
        "60",
        "-P",
        "-F",
        "#{pane_id}",
    ]);
    let window = rig.display(&mirror, "#{window_id}");
    let member_a = rig.tmux_ok(&["split-window", "-t", &window, "-P", "-F", "#{pane_id}"]);
    let member_b = rig.tmux_ok(&["split-window", "-t", &window, "-P", "-F", "#{pane_id}"]);
    for (pane, role) in [
        (&mirror, "mirror"),
        (&member_a, "agent"),
        (&member_b, "agent"),
    ] {
        rig.tmux_ok(&["set-option", "-p", "-t", pane, "@hive-role", role]);
    }
    // The hooks by hand, naming the built binary, the rows hive installs.
    for row in hive::layout::hook_argv(&window, env!("CARGO_BIN_EXE_hive")) {
        let args: Vec<&str> = row.iter().map(String::as_str).collect();
        rig.tmux_ok(&args);
    }
    let hooks = rig.tmux_ok(&["show-hooks", "-w", "-t", &window]);
    assert!(hooks.contains("window-resized"), "{hooks}");
    assert!(hooks.contains("window-layout-changed"), "{hooks}");

    // The human's repair: a forced apply of the landscape plan.
    let stdout = rig.hive_ok(&["layout", "auto", "--window", &window]);
    let payload: Value = serde_json::from_str(&stdout).expect("hive layout auto prints JSON");
    let landscape = rig.plan(&window, (220, 60));
    assert_eq!(payload["applied"], Value::Bool(true));
    assert_eq!(payload["layout"], Value::String(landscape.key.clone()));
    assert_eq!(
        payload["orientation"],
        Value::String("landscape".to_string())
    );
    assert_eq!(landscape.key, "landscape/m2/mirror-half/1x2");
    assert_eq!(rig.layout(&window), landscape.layout);
    assert_eq!(rig.key(&window), landscape.key);
    let panes = rig.panes(&window);
    assert_eq!(
        panes.iter().map(|p| p.3).collect::<Vec<_>>(),
        vec![109, 110, 110],
        "{panes:?}"
    );

    // A portrait client attaching is `resize-window` to the test: the hook
    // re-plans without a hive call from here, and every pane sits at x=0.
    rig.tmux_ok(&["resize-window", "-t", &window, "-x", "100", "-y", "90"]);
    let portrait = rig.plan(&window, (100, 90));
    assert_eq!(portrait.key, "portrait/m2/mirror-min/1x2");
    rig.wait_for_plan(&window, &portrait)
        .unwrap_or_else(|seen| panic!("portrait plan never applied; saw {seen:?}"));
    assert_eq!(rig.key(&window), portrait.key);
    let panes = rig.panes(&window);
    assert!(panes.iter().all(|p| p.2 == 0), "{panes:?}");
    assert!(panes.iter().all(|p| p.3 == 100), "{panes:?}");

    // Back to landscape: the flip is a key change, so the plan lands again.
    rig.tmux_ok(&["resize-window", "-t", &window, "-x", "220", "-y", "60"]);
    rig.wait_for_plan(&window, &landscape)
        .unwrap_or_else(|seen| panic!("landscape plan never re-applied; saw {seen:?}"));

    // A human drags the mirror's border 20 columns left: the hook fires,
    // the plan's key is unchanged, so nothing is written back.
    rig.tmux_ok(&["resize-pane", "-t", &member_a, "-L", "20"]);
    rig.settle();
    let dragged = rig.layout(&window);
    assert_ne!(dragged, landscape.layout);
    let panes = rig.panes(&window);
    assert_eq!(
        panes.iter().map(|p| p.3).collect::<Vec<_>>(),
        vec![89, 130, 130],
        "{panes:?}"
    );
    assert_eq!(rig.key(&window), landscape.key);

    // A proportional resize keeps the key: tmux scales the dragged layout
    // and hive leaves it, rather than resetting the mirror to its half.
    rig.tmux_ok(&["resize-window", "-t", &window, "-x", "200", "-y", "55"]);
    rig.settle();
    let scaled = rig.plan(&window, (200, 55));
    assert_eq!(scaled.key, landscape.key);
    let panes = rig.panes(&window);
    assert_ne!(rig.layout(&window), scaled.layout, "{panes:?}");
    assert!(panes[0].3 < 95, "the drag's ratio survived: {panes:?}");
    assert_eq!(rig.key(&window), landscape.key);

    // A member dies: the count is in the key, so the survivors are
    // re-planned by the hook alone.
    rig.tmux_ok(&["kill-pane", "-t", &member_b]);
    let one = rig.plan(&window, (200, 55));
    assert_eq!(one.key, "landscape/m1/mirror-half/1x1");
    rig.wait_for_plan(&window, &one)
        .unwrap_or_else(|seen| panic!("re-plan after kill never applied; saw {seen:?}"));
    assert_eq!(rig.key(&window), one.key);
    let panes = rig.panes(&window);
    assert_eq!(
        panes.iter().map(|p| p.3).collect::<Vec<_>>(),
        vec![99, 100],
        "{panes:?}"
    );

    // The hook form from a run-shell job's view is silent and writes
    // nothing when the key matches.
    let stdout = rig.hive_ok(&["layout", "auto", "--on-change", "--window", &window]);
    assert_eq!(stdout, "");
    assert_eq!(rig.layout(&window), one.layout);

    // The hooks name the window `@N`; a verb names it `session:index`. Both
    // lock on the id, so the window has one lock file, not one per spelling.
    let by_index = format!("{session}:0");
    let stdout = rig.hive_ok(&["layout", "auto", "--on-change", "--window", &by_index]);
    assert_eq!(stdout, "");
    let id_digits = window.trim_start_matches('@');
    assert_eq!(rig.lock_files(), vec![format!("layout-_{id_digits}.lock")]);
}

#[test]
fn test_delete_on_a_lent_window_unsets_the_hooks_and_leaves_the_humans_split_alone() {
    let rig = Rig::new("lent");
    // A human's session with one shell pane…
    let human = rig.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "human",
        "-x",
        "220",
        "-y",
        "60",
        "-P",
        "-F",
        "#{pane_id}",
    ]);
    let window = rig.display(&human, "#{window_id}");
    // …lends its window to a team: an in-tmux create binds it in place.
    let ws = rig.tmp.path().join("ws");
    let stdout = rig.hive_in_pane_ok(
        &["create", &rig.team, "--workspace", ws.to_str().unwrap()],
        &human,
    );
    assert!(stdout.contains(&rig.team), "create stdout: {stdout}");
    // Window options as tmux lists them (`display-message` would answer
    // the orch pane's own `@hive-team` tag for the window).
    let options = rig.tmux_ok(&["show-options", "-w", "-t", &window]);
    assert!(
        options.contains(&format!("@hive-team {}", rig.team)),
        "{options}"
    );
    let hooks = rig.tmux_ok(&["show-hooks", "-w", "-t", &window]);
    for hook in hive::layout::LAYOUT_HOOKS {
        assert!(hooks.contains(hook), "{hook} missing from {hooks}");
    }

    // The team goes; the window stays (it is the human's) with no tag,
    // no key and no hook left on it.
    rig.hive_in_pane_ok(&["delete", &rig.team, "--delete-workspace"], &human);
    let options = rig.tmux_ok(&["show-options", "-w", "-t", &window]);
    assert!(
        !options.lines().any(|line| line.starts_with("@hive-")),
        "{options}"
    );
    assert_eq!(rig.key(&window), "");
    let hooks = rig.tmux_ok(&["show-hooks", "-w", "-t", &window]);
    for hook in hive::layout::LAYOUT_HOOKS {
        assert!(!hooks.contains(hook), "{hook} outlived delete: {hooks}");
    }

    // The human splits their window by hand: 40 columns on the right, and
    // it stays that way — no hook re-tiles it, no key is written.
    rig.tmux_ok(&["split-window", "-h", "-l", "40", "-t", &window]);
    let split = rig.layout(&window);
    rig.settle();
    assert_eq!(rig.layout(&window), split);
    assert_eq!(rig.key(&window), "");
    let panes = rig.panes(&window);
    assert_eq!(
        panes.iter().map(|p| p.3).collect::<Vec<_>>(),
        vec![179, 40],
        "{panes:?}"
    );
}

#[test]
fn test_create_outside_tmux_installs_the_layout_hooks_on_the_team_window() {
    let rig = Rig::new("create");
    let ws = rig.tmp.path().join("ws");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hive"));
    cmd.args(["create", &rig.team, "--workspace", ws.to_str().unwrap()])
        .current_dir(rig.tmp.path())
        .stdin(Stdio::null());
    rig.env(&mut cmd);
    let out = cmd.output().expect("hive runs");
    assert!(
        out.status.success(),
        "create failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let window = rig.display(&format!("={}:", rig.team), "#{window_id}");
    let hooks = rig.tmux_ok(&["show-hooks", "-w", "-t", &window]);
    let bin = env!("CARGO_BIN_EXE_hive");
    for hook in hive::layout::LAYOUT_HOOKS {
        let line = hooks
            .lines()
            .find(|line| line.starts_with(hook))
            .unwrap_or_else(|| panic!("{hook} missing from {hooks}"));
        assert!(line.contains(bin), "{line}");
        assert!(line.contains("layout auto --on-change --window"), "{line}");
        assert!(line.contains("#{window_id}"), "{line}");
    }
    // One pane: no plan, and the human form says so.
    let stdout = rig.hive_ok(&["layout", "auto", "--window", &window]);
    let payload: Value = serde_json::from_str(&stdout).expect("JSON");
    assert_eq!(payload["applied"], Value::Bool(false));
    assert_eq!(payload["reason"], Value::String("no-plan".to_string()));
    assert_eq!(rig.key(&window), "");
    // A second pane through tmux alone: the hook plans the pair.
    rig.tmux_ok(&["split-window", "-t", &window]);
    let pair = rig.plan(&window, (220, 60));
    rig.wait_for_plan(&window, &pair)
        .unwrap_or_else(|seen| panic!("pair never planned by the hook; saw {seen:?}"));
    assert_eq!(rig.key(&window), pair.key);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hive"));
    cmd.args(["delete", &rig.team, "--delete-workspace"])
        .current_dir(rig.tmp.path())
        .stdin(Stdio::null());
    rig.env(&mut cmd);
    let out = cmd.output().expect("hive runs");
    assert!(
        out.status.success(),
        "delete failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
