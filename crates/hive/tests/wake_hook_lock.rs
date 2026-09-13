//! R3: two hive homes installing their wake hooks on one session at once
//! cannot take the same free index. The hook arrays are read-modify-write
//! state shared by every home on the tmux server, and each home's
//! installer and remover holds the server's hook lock for the whole of its
//! read and its writes.
//!
//! `harness = false` on purpose: the binary plays the contending homes
//! itself, so what races is the production installer and remover in two
//! real processes against a real, private tmux server. A `tmux` shim on
//! the children's PATH runs the real tmux and, for the actor the rig
//! holds, parks the child after its `show-hooks` answer — between the read
//! of the arrays and the writes back, the window a second installer's read
//! must not fall into:
//!
//! - `--install <session_id>` → `hive::tmux::install_wake_hooks`.
//! - `--remove <session_id>`  → `hive::tmux::remove_wake_hooks`.
//! - no args                  → the scenario below.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SESSION: &str = "lcw-hooks";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The one libtest surface a runner needs from a `harness = false`
    // binary: the list of tests it holds.
    if args.iter().any(|arg| arg == "--list") {
        if !args.iter().any(|arg| arg == "--ignored") {
            println!("wake_hook_lock: test");
        }
        return;
    }
    match args.first().map(String::as_str) {
        Some("--install") => {
            if let Err(err) = hive::tmux::install_wake_hooks(&args[1]) {
                eprintln!("install: {err:#}");
                std::process::exit(1);
            }
        }
        Some("--remove") => {
            if let Err(err) = hive::tmux::remove_wake_hooks(&args[1]) {
                eprintln!("remove: {err:#}");
                std::process::exit(1);
            }
        }
        _ => {
            require_tmux();
            concurrent_homes_each_keep_one_entry_per_hook();
            println!("test concurrent_homes_each_keep_one_entry_per_hook ... ok");
        }
    }
}

fn require_tmux() {
    if let Err(err) = Command::new("tmux").arg("-V").output() {
        panic!("tmux is required: this rig starts its own private server ({err})");
    }
}

/// A private lane: a tmux server under its own `TMUX_TMPDIR`, two hive
/// homes, a bin dir holding the `tmux` shim, and the gate directory the
/// shim reports through.
struct Lane {
    root: tempfile::TempDir,
    /// The developer's PATH, what the shim runs the real tmux from.
    real_path: String,
    /// Every actor spawned, killed before the lane's directory goes.
    actors: std::cell::RefCell<Vec<u32>>,
}

impl Lane {
    fn new() -> Lane {
        let root = tempfile::tempdir().expect("lane root");
        let real_path = std::env::var("PATH").unwrap_or_default();
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        // The shim: the real tmux on the lane's socket, named explicitly
        // (`-S`) — tmux falls back to the default server, the developer's,
        // when `TMUX_TMPDIR` names a directory that is gone, and a parked
        // actor released while the lane is being torn down would otherwise
        // write its hooks there — then, for `show-hooks` from the actor the
        // rig holds, a `read` marker and a wait for its `go`.
        let shim = format!(
            "#!/bin/sh\nout=$(PATH={} tmux -S \"$HIVE_RIG_SOCKET\" \"$@\")\nrc=$?\nif [ \"$1\" = show-hooks ] && [ -n \"$HIVE_RIG_GATE\" ]; then\n  : > \"$HIVE_RIG_GATE/read.$HIVE_RIG_ACTOR\"\n  while [ ! -e \"$HIVE_RIG_GATE/go.$HIVE_RIG_ACTOR\" ]; do sleep 0.02; done\nfi\n[ -n \"$out\" ] && printf '%s\\n' \"$out\"\nexit $rc\n",
            shell_quote(&real_path)
        );
        let shim_path = bin.join("tmux");
        std::fs::write(&shim_path, shim).expect("tmux shim");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        for dir in ["home", "home-a", "home-b", "gate"] {
            std::fs::create_dir_all(root.path().join(dir)).expect("lane dir");
        }
        // tmux makes the socket directory only when it resolves it from
        // TMUX_TMPDIR; the `-S` clients below need it there, mode 0700.
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(
                root.path()
                    .join(format!("tmux-{}", unsafe { libc::getuid() })),
            )
            .expect("socket dir");
        Lane {
            root,
            real_path,
            actors: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn socket(&self) -> PathBuf {
        self.root
            .path()
            .join(format!("tmux-{}", unsafe { libc::getuid() }))
            .join("default")
    }

    fn gate(&self) -> PathBuf {
        self.root.path().join("gate")
    }

    fn home(&self, actor: &str) -> PathBuf {
        self.root.path().join(format!("home-{actor}"))
    }

    /// A client of the private server, by explicit socket.
    fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .arg("-S")
            .arg(self.socket())
            .args(args)
            .env("PATH", &self.real_path)
            .output()
            .expect("tmux runs");
        assert!(
            out.status.success(),
            "tmux {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .trim_end_matches('\n')
            .to_string()
    }

    /// The session's wake hook entries as tmux lists them. An array every
    /// entry has been unset from is listed as its bare name: no entry.
    fn hooks(&self, session_id: &str) -> Vec<String> {
        self.tmux(&["show-hooks", "-t", session_id])
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

    /// This binary as *actor* (`a` or `b`) running *verb* on *session_id*
    /// under its own hive home, its tmux resolved through the shim to the
    /// private server. *held* parks the actor after its `show-hooks`
    /// answer until `release`.
    fn spawn(&self, actor: &str, verb: &str, session_id: &str, held: bool) -> Child {
        let path = format!(
            "{}:{}",
            self.root.path().join("bin").display(),
            self.real_path
        );
        if !held {
            std::fs::write(self.gate().join(format!("go.{actor}")), "").unwrap();
        }
        let mut cmd = Command::new(std::env::current_exe().expect("this binary"));
        cmd.arg(verb)
            .arg(session_id)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .env("PATH", path)
            .env("HOME", self.root.path().join("home"))
            .env("HIVE_HOME", self.home(actor))
            .env("TMUX_TMPDIR", self.root.path())
            .env("HIVE_RIG_SOCKET", self.socket())
            .env("HIVE_RIG_GATE", self.gate())
            .env("HIVE_RIG_ACTOR", actor);
        for key in [
            "TMUX",
            "TMUX_PANE",
            "CLAUDE_HOME",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
            "GROK_HOME",
            "CODEX_THREAD_ID",
            "GROK_SESSION_ID",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_HOST_SESSION_ID",
        ] {
            cmd.env_remove(key);
        }
        let child = cmd.spawn().expect("the actor runs");
        self.actors.borrow_mut().push(child.id());
        child
    }

    fn read_marker(&self, actor: &str) -> PathBuf {
        self.gate().join(format!("read.{actor}"))
    }

    fn release(&self, actor: &str) {
        std::fs::write(self.gate().join(format!("go.{actor}")), "").unwrap();
    }

    /// A fresh gate: no actor has read, none is released.
    fn reset_gate(&self) {
        for entry in std::fs::read_dir(self.gate()).unwrap() {
            let _ = std::fs::remove_file(entry.unwrap().path());
        }
    }
}

impl Drop for Lane {
    // Panic or not: every actor is killed first — a parked one released
    // now would run its writes against a lane that is going away — then
    // the private server goes with everything on it.
    fn drop(&mut self) {
        for pid in self.actors.borrow().iter() {
            unsafe {
                libc::kill(*pid as libc::pid_t, libc::SIGKILL);
            }
        }
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(self.socket())
            .arg("kill-server")
            .env("PATH", &self.real_path)
            .output();
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn finished_ok(child: &mut Child, what: &str) {
    let status = child.wait().expect("the actor exits");
    assert!(status.success(), "{what} failed: {status}");
}

/// Whether *marker* stays absent for a second: the held actor is between
/// its read and its writes the whole time, so an actor that reads now has
/// read a listing about to change under it.
fn stays_absent(marker: &Path) -> bool {
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        if marker.exists() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

fn entries_of(hooks: &[String], home: &Path) -> Vec<String> {
    let token = format!("HIVE_HOME={} ", home.display());
    hooks
        .iter()
        .filter(|h| h.contains(&token))
        .cloned()
        .collect()
}

fn concurrent_homes_each_keep_one_entry_per_hook() {
    let lane = Lane::new();
    lane.tmux(&["new-session", "-d", "-s", SESSION]);
    let session_id = lane.tmux(&[
        "display-message",
        "-p",
        "-t",
        &format!("={SESSION}:"),
        "#{session_id}",
    ]);
    // The human's own entry at index 0 of one hook: stays byte for byte.
    const USER_ENTRY: &str = "client-attached[0] display-message user";
    lane.tmux(&[
        "set-hook",
        "-t",
        &session_id,
        "client-attached[0]",
        "display-message user",
    ]);
    assert_eq!(lane.hooks(&session_id), vec![USER_ENTRY.to_string()]);
    let (home_a, home_b) = (lane.home("a"), lane.home("b"));

    // Home A installs and is held after its read. Home B then installs:
    // under the lock B cannot read until A has written — a read now
    // would see the same free indexes A is about to take.
    let mut a = lane.spawn("a", "--install", &session_id, true);
    wait_for("A's read", || lane.read_marker("a").exists());
    let mut b = lane.spawn("b", "--install", &session_id, false);
    assert!(
        stays_absent(&lane.read_marker("b")),
        "B read the hook arrays while A was between its read and its writes"
    );
    lane.release("a");
    finished_ok(&mut a, "A's install");
    finished_ok(&mut b, "B's install");

    let hooks = lane.hooks(&session_id);
    assert!(hooks.contains(&USER_ENTRY.to_string()), "{hooks:?}");
    let (of_a, of_b) = (entries_of(&hooks, &home_a), entries_of(&hooks, &home_b));
    assert_eq!(of_a.len(), 2, "one entry of A's per hook: {hooks:?}");
    assert_eq!(of_b.len(), 2, "one entry of B's per hook: {hooks:?}");
    assert_eq!(hooks.len(), 5, "{hooks:?}");
    for hook in ["client-attached", "client-session-changed"] {
        let mut seen = std::collections::HashSet::new();
        for entry in hooks.iter().filter(|h| h.starts_with(&format!("{hook}["))) {
            let index = entry.split_once(']').unwrap().0.to_string();
            assert!(
                seen.insert(index.clone()),
                "index {index} written twice: {hooks:?}"
            );
        }
    }

    // Installed again, in any order, at once: nothing grows, nothing moves.
    lane.reset_gate();
    let actors = ["a", "b", "a", "b"];
    let mut again: Vec<Child> = actors
        .iter()
        .map(|actor| lane.spawn(actor, "--install", &session_id, false))
        .collect();
    for (child, actor) in again.iter_mut().zip(actors) {
        finished_ok(child, &format!("{actor}'s repeated install"));
    }
    assert_eq!(
        lane.hooks(&session_id),
        hooks,
        "a repeated install changed the arrays"
    );

    // Home A removes its entries and is held after its read; home B's
    // install meanwhile waits the same way, so B's write lands on the
    // arrays A's unsets left, not on the listing A read.
    lane.reset_gate();
    let mut a = lane.spawn("a", "--remove", &session_id, true);
    wait_for("A's read", || lane.read_marker("a").exists());
    let mut b = lane.spawn("b", "--install", &session_id, false);
    assert!(
        stays_absent(&lane.read_marker("b")),
        "B read the hook arrays while A's remover was between its read and its unsets"
    );
    lane.release("a");
    finished_ok(&mut a, "A's remove");
    finished_ok(&mut b, "B's install");
    let after = lane.hooks(&session_id);
    assert!(after.contains(&USER_ENTRY.to_string()), "{after:?}");
    assert!(entries_of(&after, &home_a).is_empty(), "{after:?}");
    assert_eq!(
        entries_of(&after, &home_b),
        of_b,
        "B's entries moved: {after:?}"
    );
    assert_eq!(after.len(), 3, "{after:?}");
}
