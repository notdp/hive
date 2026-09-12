//! AC-2: the startup lock must not survive into the hived a CLI spawns.
//!
//! This rig is `harness = false` on purpose. It plays three roles out of one
//! binary, so the launcher runs hive's real `StartupLock` and `start_hived`
//! and the process they spawn is the real `--hived` entry, not a stand-in:
//!
//! - `--hived …`   → `hive::hived::run_spawned_hived`, the production hived.
//! - `--launcher …` → takes the real startup lock, spawns a real hived, then
//!   parks without ever unlocking — what a Ctrl-C'd `hive send` leaves behind
//!   in the middle of `ensure_hived`'s readiness poll.
//! - no args        → the scenarios below.
//!
//! Everything lives in a private HOME/HIVE_HOME/TMUX_TMPDIR lane.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const TEAM: &str = "lcw1";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The one libtest surface a runner needs from a `harness = false`
    // binary: the list of tests it holds.
    if args.iter().any(|arg| arg == "--list") {
        // `--list --ignored` asks for the ignored subset; nothing here is.
        if !args.iter().any(|arg| arg == "--ignored") {
            println!("startup_lock: test");
        }
        return;
    }
    match args.first().map(String::as_str) {
        Some("--hived") => std::process::exit(hive::hived::run_spawned_hived(&args)),
        Some("--launcher") => launcher(&args[1..]),
        _ => {
            killed_ensure_parent_does_not_leave_lock_in_child();
            println!("test killed_ensure_parent_does_not_leave_lock_in_child ... ok");
            ensure_hived_reaches_a_real_hived_and_the_lane_winds_down();
            println!("test ensure_hived_reaches_a_real_hived_and_the_lane_winds_down ... ok");
        }
    }
}

/// `<workspace> <team> <tmux_window> <tmux_window_id> <pid file>`
fn launcher(args: &[String]) {
    let _lock = hive::hived::StartupLock::acquire(&args[0]).expect("launcher takes the lock");
    let pid = hive::hived::start_hived(&args[0], &args[1], &args[2], &args[3])
        .expect("launcher spawns a hived");
    std::fs::write(&args[4], pid.to_string()).expect("pid file");
    // Park holding the lock. The test kills this process here.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// A private lane: every hive and engine root under one temp tree.
struct Lane {
    _root: tempfile::TempDir,
    workspace: String,
}

fn lane() -> Lane {
    let root = tempfile::tempdir().expect("lane root");
    let set = |key: &str, path: PathBuf| {
        std::fs::create_dir_all(&path).expect("lane dir");
        std::env::set_var(key, &path);
    };
    set("HOME", root.path().join("home"));
    set("HIVE_HOME", root.path().join("hive"));
    set("CLAUDE_HOME", root.path().join("claude"));
    set("CODEX_HOME", root.path().join("codex"));
    set("GROK_HOME", root.path().join("grok"));
    set("XDG_CACHE_HOME", root.path().join("cache"));
    set("TMUX_TMPDIR", root.path().join("tmux"));
    let workspace = root.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let workspace = workspace.to_string_lossy().into_owned();
    hive::registry::record_team(TEAM, &workspace, "1730000000", &[], "").expect("registry entry");
    Lane {
        _root: root,
        workspace,
    }
}

fn lock_path(workspace: &str) -> PathBuf {
    hive::devlog::run_dir(Path::new(workspace)).join("hived.lock")
}

/// An independent contender: a fresh open file description, one
/// `LOCK_EX | LOCK_NB` attempt, released right away when it wins.
fn contender_takes_lock(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_CLOEXEC,
            0o644,
        )
    };
    assert!(fd >= 0, "contender cannot open {}", path.display());
    let won = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if won {
        unsafe { libc::flock(fd, libc::LOCK_UN) };
    }
    unsafe { libc::close(fd) };
    won
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A hived this process is the parent of: `kill(pid, 0)` still answers for
/// a zombie, so the exit has to be reaped to be seen.
fn reap_child(pid: i32) -> Option<i32> {
    let mut status = 0;
    let reaped = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if reaped != pid {
        return None;
    }
    Some(libc::WEXITSTATUS(status))
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

/// The hived's own ping, answered with this team's identity.
fn pings_as_team(workspace: &str) -> bool {
    hive::hived::request_ping(workspace)
        .and_then(|r| r.get("team").and_then(|t| t.as_str()).map(str::to_owned))
        .as_deref()
        == Some(TEAM)
}

/// The open files of a process, as the OS reports them.
fn open_files(pid: i32) -> String {
    let out = Command::new("lsof")
        .args(["-p", &pid.to_string()])
        .output()
        .expect("lsof runs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn killed_ensure_parent_does_not_leave_lock_in_child() {
    let lane = lane();
    let workspace = lane.workspace.clone();
    let lock = lock_path(&workspace);
    std::fs::create_dir_all(lock.parent().unwrap()).expect("run dir");
    assert!(
        contender_takes_lock(&lock),
        "the lane starts with a free lock"
    );

    let pid_file = Path::new(&workspace).join("hived.pid");
    let mut launcher = Command::new(std::env::current_exe().expect("current exe"))
        .args([
            "--launcher",
            &workspace,
            TEAM,
            "",
            "",
            pid_file.to_str().unwrap(),
        ])
        .spawn()
        .expect("launcher spawns");
    let launcher_pid = launcher.id() as i32;

    wait_until("the launcher to report the hived pid", || {
        pid_file.is_file()
    });
    let hived_pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid");
    // The test channel: the helper answers as this team, so it is ready.
    wait_until("the hived to answer a matching ping", || {
        pings_as_team(&workspace)
    });

    // Causal coupling: the launcher really does hold the lock right now.
    assert!(alive(launcher_pid), "the launcher is still up");
    assert!(
        !contender_takes_lock(&lock),
        "the launcher must hold the startup lock while the hived runs"
    );

    // Kill the launcher, never the helper — the Ctrl-C case.
    assert_eq!(unsafe { libc::kill(launcher_pid, libc::SIGKILL) }, 0);
    launcher.wait().expect("launcher reaped");
    assert!(alive(hived_pid), "the hived outlives its launcher");

    let files = open_files(hived_pid);
    assert!(
        !files.contains("hived.lock"),
        "the hived inherited the startup lock:\n{files}"
    );
    assert!(
        contender_takes_lock(&lock),
        "a killed launcher must not leave the lock held by its child"
    );

    // The lane winds down: the real `--hived` entry answers shutdown.
    hive::hived::stop_hived(&workspace);
    wait_until("the hived to exit", || !alive(hived_pid));
    assert!(
        !hive::hived::socket_path(&workspace).exists(),
        "the socket is cleaned up by its owner"
    );
}

fn ensure_hived_reaches_a_real_hived_and_the_lane_winds_down() {
    let lane = lane();
    let workspace = lane.workspace.clone();
    let lock = lock_path(&workspace);

    // The real launcher path, in this process: it takes the startup lock,
    // spawns this binary as `--hived`, and returns only on a matching ping.
    let hived_pid = hive::hived::ensure_hived(&workspace, TEAM, "", "")
        .expect("ensure_hived")
        .expect("a hived was spawned");
    assert!(pings_as_team(&workspace));
    assert!(alive(hived_pid));
    assert!(
        contender_takes_lock(&lock),
        "ensure_hived releases the startup lock when it returns"
    );
    let files = open_files(hived_pid);
    assert!(
        !files.contains("hived.lock"),
        "the hived inherited the startup lock:\n{files}"
    );

    // A second ensure finds the same generation and starts nothing.
    assert_eq!(
        hive::hived::ensure_hived(&workspace, TEAM, "", "").expect("second ensure_hived"),
        None
    );

    hive::hived::stop_hived(&workspace);
    let mut code = None;
    wait_until("the hived to exit", || {
        code = reap_child(hived_pid);
        code.is_some()
    });
    assert_eq!(code, Some(0), "the hived retires cleanly");
    assert!(!hive::hived::socket_path(&workspace).exists());
}
