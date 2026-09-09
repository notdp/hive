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
            bail!("hclaude needs the foreground terminal");
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
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        // If it read before tcsetpgrp, SIGTTIN stopped it; it now owns the tty.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGCONT) };
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
    pub(super) fn poll(&mut self) -> Result<Option<i32>> {
        match self.child.try_wait()? {
            Some(status) => {
                self.done = true;
                Ok(Some(
                    status
                        .code()
                        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
                ))
            }
            None => Ok(None),
        }
    }

    pub(super) fn stop(&mut self) -> Result<()> {
        if !self.done {
            // Child owns this PID until wait reaps it, even after attach execs
            // into the agents page. Never signal a PID read from a record.
            if self.child.try_wait()?.is_none() {
                self.child.kill()?;
            }
            self.child.wait()?;
            self.done = true;
        }
        Ok(())
    }

    pub(super) fn wait(&mut self) -> Result<i32> {
        let status = self.child.wait()?;
        self.done = true;
        Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
    }
}

impl Drop for Foreground {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
