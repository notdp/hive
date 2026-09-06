//! Low-level tmux subprocess execution: `Run`, `TmuxError`, `_run`.

use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Mirror of Python's `subprocess.CompletedProcess` as `_run` returns it.
#[derive(Debug, Clone)]
pub struct Run {
    pub returncode: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
pub enum TmuxError {
    /// `subprocess.TimeoutExpired`
    Timeout,
    /// `subprocess.CalledProcessError`
    CalledProcess { returncode: i32, stderr: String },
    /// `OSError` (missing binary, spawn failure)
    Os(String),
}

impl std::fmt::Display for TmuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TmuxError::Timeout => write!(f, "tmux command timed out"),
            TmuxError::CalledProcess { returncode, stderr } => {
                write!(f, "tmux exited with status {returncode}: {stderr}")
            }
            TmuxError::Os(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for TmuxError {}

#[cfg(test)]
type RunOverride = Box<dyn FnMut(&[String], bool, u64) -> Result<Run, TmuxError>>;
#[cfg(test)]
type ExecOverride = Box<dyn FnMut(&[String], u64, Option<&str>) -> Result<Run, TmuxError>>;

#[cfg(test)]
thread_local! {
    pub(super) static RUN_OVERRIDE: std::cell::RefCell<Option<RunOverride>> =
        const { std::cell::RefCell::new(None) };
    pub(super) static EXEC_OVERRIDE: std::cell::RefCell<Option<ExecOverride>> =
        const { std::cell::RefCell::new(None) };
}

/// Test seam: route every `_run` through *f*, which sees `(argv, check,
/// timeout)`. Crate-visible because the command-layer tests drive whole
/// handlers and assert on the tmux argv those handlers issue.
#[cfg(test)]
pub(crate) fn _set_run_override(
    f: impl FnMut(&[String], bool, u64) -> Result<Run, TmuxError> + 'static,
) {
    RUN_OVERRIDE.with(|o| *o.borrow_mut() = Some(Box::new(f)));
}

/// Low-level subprocess execution with capture + timeout (the
/// `subprocess.run(capture_output=True, timeout=...)` seam).
pub(super) fn exec_capture(
    argv: &[String],
    timeout_secs: u64,
    input: Option<&str>,
) -> Result<Run, TmuxError> {
    #[cfg(test)]
    {
        let hit = EXEC_OVERRIDE.with(|o| {
            let mut slot = o.borrow_mut();
            slot.as_mut().map(|f| f(argv, timeout_secs, input))
        });
        if let Some(res) = hit {
            return res;
        }
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.spawn().map_err(|e| TmuxError::Os(e.to_string()))?;
    if let Some(data) = input {
        if let Some(mut stdin) = child.stdin.take() {
            let owned = data.as_bytes().to_vec();
            // Write from a thread: a hung tmux server must hit the timeout,
            // not block this process on a full pipe.
            thread::spawn(move || {
                use std::io::Write;
                let _ = stdin.write_all(&owned);
            });
        }
    }
    let mut stdout_pipe = child.stdout.take();
    let out_h = thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        if let Some(p) = stdout_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let mut stderr_pipe = child.stderr.take();
    let err_h = thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        if let Some(p) = stderr_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    // ponytail: 10ms try_wait polling instead of signalfd/waitpid plumbing;
    // tmux commands finish in single-digit ms, precision does not matter.
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_h.join();
                    let _ = err_h.join();
                    return Err(TmuxError::Timeout);
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(TmuxError::Os(e.to_string())),
        }
    };
    let stdout = String::from_utf8_lossy(&out_h.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_h.join().unwrap_or_default()).into_owned();
    let returncode = status.code().unwrap_or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        -status.signal().unwrap_or(1)
    });
    Ok(Run {
        returncode,
        stdout,
        stderr,
    })
}

/// Run a tmux command.
///
/// `check=true` means the caller needs the command to have happened, so a
/// timeout errors like a nonzero exit does — a busy tmux server must never
/// look like a successful send-keys. `check=false` callers are probes that
/// read "unknown" out of the rc-1 sentinel, so they keep it.
pub fn _run(args: &[&str], check: bool, timeout: u64) -> Result<Run, TmuxError> {
    #[cfg(test)]
    {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let hit = RUN_OVERRIDE.with(|o| {
            let mut slot = o.borrow_mut();
            slot.as_mut().map(|f| f(&owned, check, timeout))
        });
        if let Some(res) = hit {
            return res;
        }
    }
    let mut argv: Vec<String> = Vec::with_capacity(args.len() + 1);
    argv.push("tmux".to_string());
    argv.extend(args.iter().map(|s| s.to_string()));
    match exec_capture(&argv, timeout, None) {
        Ok(r) => {
            if check && r.returncode != 0 {
                Err(TmuxError::CalledProcess {
                    returncode: r.returncode,
                    stderr: r.stderr,
                })
            } else {
                Ok(r)
            }
        }
        Err(TmuxError::Timeout) => {
            if check {
                Err(TmuxError::Timeout)
            } else {
                Ok(Run {
                    returncode: 1,
                    stdout: String::new(),
                    stderr: "timeout".to_string(),
                })
            }
        }
        Err(e) => Err(e),
    }
}

pub fn _run_output(args: &[&str]) -> anyhow::Result<String> {
    let r = _run(args, true, 5)?;
    Ok(r.stdout.trim().to_string())
}
