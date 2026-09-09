//! The launcher's own foreground child. A handoff stops a viewer, not its job.

use std::io::{self, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command};

use anyhow::{bail, Result};

pub(super) struct Terminal {
    modes: libc::termios,
    group: libc::pid_t,
}

impl Terminal {
    pub(super) fn capture() -> Result<Self> {
        let mut modes = std::mem::MaybeUninit::uninit();
        if unsafe { libc::tcgetattr(0, modes.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let group = unsafe { libc::getpgrp() };
        if unsafe { libc::tcgetpgrp(0) } != group {
            bail!("hive launcher needs the foreground terminal");
        }
        Ok(Self {
            modes: unsafe { modes.assume_init() },
            group,
        })
    }

    fn foreground(&self, group: libc::pid_t) -> Result<()> {
        // The parent is in the background while its child owns the tty.
        // Ignore SIGTTOU only around the operation that takes it back.
        let old = unsafe { libc::signal(libc::SIGTTOU, libc::SIG_IGN) };
        let result = unsafe { libc::tcsetpgrp(0, group) };
        let error = io::Error::last_os_error();
        unsafe { libc::signal(libc::SIGTTOU, old) };
        if result == 0 {
            Ok(())
        } else {
            Err(error.into())
        }
    }

    pub(super) fn restore(&self, reset_screen: bool) {
        let _ = self.foreground(self.group);
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.modes) };
        if reset_screen {
            // A terminated TUI could not restore its alternate screen/cursor.
            print!("\x1b[?1049l\x1b[?25h\x1b[0m");
            let _ = io::stdout().flush();
        }
    }

    pub(super) fn spawn(&self, command: &mut Command) -> Result<Foreground> {
        let mut child = command.process_group(0).spawn()?;
        if let Err(error) = self.foreground(child.id() as libc::pid_t) {
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            let _ = child.wait();
            return Err(error);
        }
        // If it read before tcsetpgrp, SIGTTIN stopped it; it now owns the tty.
        unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGCONT) };
        Ok(Foreground { child, done: false })
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.restore(false);
    }
}

pub(super) struct Foreground {
    child: Child,
    done: bool,
}

impl Foreground {
    // Observe exit without reaping: the owned leader keeps its process group
    // number reserved while we terminate npm wrappers' native TUI children.
    fn exited(&self, blocking: bool) -> Result<bool> {
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let flags = libc::WEXITED | libc::WNOWAIT | if blocking { 0 } else { libc::WNOHANG };
            let result = unsafe { libc::waitid(libc::P_PID, self.child.id(), &mut info, flags) };
            if result == 0 {
                return Ok(info.si_signo != 0);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
    }

    fn finish(&mut self) -> Result<i32> {
        let result = unsafe { libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL) };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error.into());
            }
        }
        let status = self.child.wait()?;
        self.done = true;
        Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
    }

    pub(super) fn poll(&mut self) -> Result<Option<i32>> {
        if self.exited(false)? {
            Ok(Some(self.finish()?))
        } else {
            Ok(None)
        }
    }

    pub(super) fn stop(&mut self) -> Result<()> {
        if !self.done {
            self.finish()?;
        }
        Ok(())
    }

    pub(super) fn wait(&mut self) -> Result<i32> {
        self.exited(true)?;
        self.finish()
    }
}

impl Drop for Foreground {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wrapper(script: &str) -> Foreground {
        let child = Command::new("/bin/sh")
            .args(["-c", script])
            .process_group(0)
            .spawn()
            .unwrap();
        Foreground { child, done: false }
    }

    fn wait_exit(child: &mut Foreground) -> i32 {
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(code) = child.poll().unwrap() {
                return code;
            }
            assert!(Instant::now() < until, "child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn test_stop_terminates_wrapper_child_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let survivor = dir.path().join("survivor");
        let mut child = wrapper(&format!(
            "(touch {}; sleep 1; touch {}) & wait",
            crate::shell::shlex_quote(&ready.to_string_lossy()),
            crate::shell::shlex_quote(&survivor.to_string_lossy()),
        ));
        let until = Instant::now() + Duration::from_secs(3);
        while !ready.exists() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(child.poll().unwrap(), None);
        child.stop().unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        assert!(!survivor.exists(), "wrapper's child survived stop");
        child.stop().unwrap();
    }

    #[test]
    fn test_wrapper_exit_cleans_remaining_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let survivor = dir.path().join("survivor");
        let mut child = wrapper(&format!(
            "(sleep 1; touch {}) & exit 7",
            crate::shell::shlex_quote(&survivor.to_string_lossy()),
        ));
        assert_eq!(wait_exit(&mut child), 7);
        std::thread::sleep(Duration::from_millis(1100));
        assert!(!survivor.exists(), "orphan survived wrapper exit");
    }
}
