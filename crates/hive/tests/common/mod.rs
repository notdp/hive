//! tmux helpers shared by the real-tmux integration tests. Every test
//! creates its own detached session and never touches a live one.

#![allow(dead_code)]

use std::process::Command;

/// The tmux binary is a hard requirement of these tests: each one starts
/// its own detached session (tmux brings a server up if none is running).
/// A missing binary is a broken dev environment, reported as a failure
/// with the fix in the message — never a silent pass.
pub fn require_tmux() {
    if let Err(err) = Command::new("tmux").arg("-V").output() {
        panic!(
            "tmux is required: this integration test creates its own detached \
             tmux session; install tmux and put it on PATH ({err})"
        );
    }
}

/// A private tmux server for the calling test process: `TMUX_TMPDIR` in
/// the process env, so `run_tmux`, the in-process hive tmux calls and any
/// server-side callback into hive all reach the same throwaway server and
/// never the developer's. The directory outlives the guard's holder; the
/// server exits with its last session.
pub struct PrivateServer {
    // Declaration order is drop order: the env var goes before the
    // directory, so no tmux client can run with TMUX_TMPDIR naming a
    // removed directory (tmux then falls through to the default server).
    _env: EnvVarGuard,
    _dir: tempfile::TempDir,
}

pub fn private_server() -> PrivateServer {
    let dir = tempfile::tempdir().expect("temp dir");
    let env = EnvVarGuard::set("TMUX_TMPDIR", dir.path().to_str().unwrap());
    PrivateServer {
        _env: env,
        _dir: dir,
    }
}

pub fn run_tmux(args: &[&str]) -> String {
    let out = Command::new("tmux").args(args).output().expect("tmux runs");
    assert!(
        out.status.success(),
        "tmux {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .trim_end_matches('\n')
        .to_string()
}

pub fn kill_session(session: &str) {
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", session])
        .output();
}

/// A process env var set for the guard's lifetime and put back (or removed
/// again) on drop, unwinding included.
pub struct EnvVarGuard {
    key: String,
    previous: Option<String>,
}

impl EnvVarGuard {
    pub fn set(key: &str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        std::env::set_var(key, value);
        EnvVarGuard {
            key: key.to_string(),
            previous,
        }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(&self.key, value),
            None => std::env::remove_var(&self.key),
        }
    }
}

/// A tmux client on a pseudo-terminal: what `tmux attach` is to the
/// server — a terminal viewer whose arrival fires the session's
/// `client-attached` hook — without any human terminal. The client's
/// output is drained; dropping the guard hangs the client up and reaps it.
pub struct PtyClient {
    child: std::process::Child,
    master: i32,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl PtyClient {
    /// Run *cmd* (a `tmux … attach …` command, its env already set) with a
    /// 200x50 pty as its controlling terminal.
    pub fn spawn(mut cmd: Command) -> PtyClient {
        use std::os::unix::io::FromRawFd;
        use std::os::unix::process::CommandExt;
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        size.ws_col = 200;
        size.ws_row = 50;
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
        let stdio = |fd: i32| unsafe { std::process::Stdio::from_raw_fd(libc::dup(fd)) };
        cmd.stdin(stdio(slave))
            .stdout(stdio(slave))
            .stderr(stdio(slave))
            .env("TERM", "xterm-256color");
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = cmd.spawn().expect("tmux client on a pty");
        unsafe { libc::close(slave) };
        let drain = master;
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                let n = unsafe { libc::read(drain, buf.as_mut_ptr() as *mut _, buf.len()) };
                if n <= 0 {
                    break;
                }
            }
        });
        PtyClient {
            child,
            master,
            reader: Some(reader),
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Hang the client up (what closing the terminal does) and reap it.
    pub fn hangup(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGHUP) };
        let _ = self.child.wait();
        if self.master >= 0 {
            unsafe { libc::close(self.master) };
            self.master = -1;
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for PtyClient {
    fn drop(&mut self) {
        self.hangup();
    }
}
