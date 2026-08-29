//! tmux operations: pane lifecycle, send_keys, capture_pane, layout.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
thread_local! {
    static RUN_OVERRIDE: std::cell::RefCell<
        Option<Box<dyn FnMut(&[String], bool, u64) -> Result<Run, TmuxError>>>,
    > = const { std::cell::RefCell::new(None) };
    static EXEC_OVERRIDE: std::cell::RefCell<
        Option<Box<dyn FnMut(&[String], u64, Option<&str>) -> Result<Run, TmuxError>>>,
    > = const { std::cell::RefCell::new(None) };
}

/// Low-level subprocess execution with capture + timeout (the
/// `subprocess.run(capture_output=True, timeout=...)` seam).
fn exec_capture(argv: &[String], timeout_secs: u64, input: Option<&str>) -> Result<Run, TmuxError> {
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

const _CONTROL_MODE_RESTART_DELAY: f64 = 1.0;

/// Decode tmux control-mode escape: control bytes and '\' are encoded as \NNN (3 octal digits).
fn _decode_output_payload(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    let chars: Vec<char> = raw.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < n {
        let ch = chars[i];
        if ch == '\\' && i + 3 < n && chars[i + 1..i + 4].iter().all(|c| matches!(c, '0'..='7')) {
            let v = chars[i + 1..i + 4]
                .iter()
                .fold(0u32, |acc, c| acc * 8 + c.to_digit(8).unwrap());
            out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
            i += 4;
        } else {
            out.push(ch);
            i += 1;
        }
    }
    out
}

/// Return (pane_id, decoded_payload) for a control mode output line, or ("", "").
///
/// Hand-rolled equivalent of `^%(extended-output|output) (%[0-9]+)\b`.
pub fn parse_control_mode_output(line: &str) -> (String, String) {
    let stripped = line.trim();
    let empty = || (String::new(), String::new());
    let (is_extended, rest) = if let Some(r) = stripped.strip_prefix("%extended-output ") {
        (true, r)
    } else if let Some(r) = stripped.strip_prefix("%output ") {
        (false, r)
    } else {
        return empty();
    };
    if !rest.starts_with('%') {
        return empty();
    }
    let digits_end = 1 + rest[1..]
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len() - 1);
    if digits_end == 1 {
        return empty();
    }
    // \b after the digit run: the next char (if any) must not be a word char.
    if let Some(c) = rest[digits_end..].chars().next() {
        if c.is_alphanumeric() || c == '_' {
            return empty();
        }
    }
    let pane = &rest[..digits_end];
    let mut remainder = &rest[digits_end..];
    if is_extended {
        // format: "<age> ... : <value>"
        if let Some(colon_idx) = remainder.find(':') {
            remainder = &remainder[colon_idx + 1..];
        }
    }
    (
        pane.to_string(),
        _decode_output_payload(remainder.trim_start()),
    )
}

/// Return the pane id for a control mode output line, if any.
pub fn parse_control_mode_output_pane(line: &str) -> Option<String> {
    let (pane_id, _) = parse_control_mode_output(line);
    if pane_id.is_empty() {
        None
    } else {
        Some(pane_id)
    }
}

/// Hand-rolled `_ANSI_ESCAPE_RE.sub("", s)`: CSI, OSC (BEL/ST terminated),
/// DCS (ST terminated), and 2-char escapes are removed; anything else is kept.
fn strip_ansi_escapes(s: &str) -> String {
    let cs: Vec<char> = s.chars().collect();
    let n = cs.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < n {
        if cs[i] != '\u{1b}' {
            out.push(cs[i]);
            i += 1;
            continue;
        }
        let next = if i + 1 < n { Some(cs[i + 1]) } else { None };
        match next {
            Some('[') => {
                // CSI: params [0-?]* intermediates [ -/]* final [@-~]
                let mut j = i + 2;
                while j < n && ('\u{30}'..='\u{3f}').contains(&cs[j]) {
                    j += 1;
                }
                while j < n && ('\u{20}'..='\u{2f}').contains(&cs[j]) {
                    j += 1;
                }
                if j < n && ('\u{40}'..='\u{7e}').contains(&cs[j]) {
                    i = j + 1;
                } else {
                    // unterminated: no regex match, the ESC stays in place
                    out.push(cs[i]);
                    i += 1;
                }
            }
            Some(']') => {
                // OSC: consume lazily up to BEL or ESC-backslash
                let mut j = i + 2;
                let mut end = None;
                while j < n {
                    if cs[j] == '\u{7}' {
                        end = Some(j + 1);
                        break;
                    }
                    if cs[j] == '\u{1b}' && j + 1 < n && cs[j + 1] == '\\' {
                        end = Some(j + 2);
                        break;
                    }
                    j += 1;
                }
                match end {
                    Some(e) => i = e,
                    // unterminated OSC degrades to the 2-char escape ESC-]
                    None => i += 2,
                }
            }
            Some('P') => {
                // DCS: consume lazily up to ESC-backslash
                let mut j = i + 2;
                let mut end = None;
                while j + 1 < n {
                    if cs[j] == '\u{1b}' && cs[j + 1] == '\\' {
                        end = Some(j + 2);
                        break;
                    }
                    j += 1;
                }
                match end {
                    Some(e) => i = e,
                    None => i += 2,
                }
            }
            Some(c) if ('\u{40}'..='\u{5a}').contains(&c) || ('\u{5c}'..='\u{5f}').contains(&c) => {
                i += 2;
            }
            _ => {
                out.push(cs[i]);
                i += 1;
            }
        }
    }
    out
}

/// `_CONTROL_CHARS_RE.sub("", s)`: drop C0 controls (except \t \n \r) and DEL.
fn strip_control_chars(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            !matches!(c, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{7f}')
        })
        .collect()
}

/// Return true when payload contains visible text, not only terminal repaint codes.
fn _control_mode_payload_has_activity(payload: &str) -> bool {
    if payload.is_empty() {
        return false;
    }
    let visible = strip_ansi_escapes(payload);
    let visible = strip_control_chars(&visible);
    !visible.trim().is_empty()
}

struct MonitorInner {
    stop: AtomicBool,
    last_output_at: Mutex<HashMap<String, Instant>>,
    master_fd: Mutex<Option<i32>>,
}

/// Best-effort tmux control-mode monitor for pane output activity.
pub struct ControlModeOutputMonitor {
    pub session_target: String,
    inner: Arc<MonitorInner>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

impl ControlModeOutputMonitor {
    pub fn new(session_target: &str) -> Self {
        ControlModeOutputMonitor {
            session_target: session_target.to_string(),
            inner: Arc::new(MonitorInner {
                stop: AtomicBool::new(false),
                last_output_at: Mutex::new(HashMap::new()),
                master_fd: Mutex::new(None),
            }),
            thread: Mutex::new(None),
        }
    }

    pub fn start(&self) {
        if self.session_target.is_empty() {
            return;
        }
        let mut slot = self.thread.lock().unwrap();
        if let Some(handle) = slot.as_ref() {
            if !handle.is_finished() {
                return;
            }
        }
        self.inner.stop.store(false, Ordering::SeqCst);
        let inner = Arc::clone(&self.inner);
        let target = self.session_target.clone();
        let spawned = thread::Builder::new()
            .name("hive-tmux-control".to_string())
            .spawn(move || monitor_run_loop(inner, target));
        if let Ok(handle) = spawned {
            *slot = Some(handle);
        }
    }

    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        self._request_detach();
        let handle = self.thread.lock().unwrap().take();
        if let Some(h) = handle {
            // ponytail: unbounded join (Python uses a 2s timeout); the loop
            // re-checks the stop flag every <=0.5s select tick plus the 1s
            // restart delay, so this is bounded in practice.
            let _ = h.join();
        }
    }

    pub fn is_busy(&self, pane_id: &str, threshold_seconds: f64) -> bool {
        if pane_id.is_empty() {
            return false;
        }
        let last = self
            .inner
            .last_output_at
            .lock()
            .unwrap()
            .get(pane_id)
            .copied();
        match last {
            None => false,
            Some(t) => t.elapsed().as_secs_f64() <= threshold_seconds,
        }
    }

    pub fn last_output_age(&self, pane_id: &str) -> Option<f64> {
        if pane_id.is_empty() {
            return None;
        }
        let last = self
            .inner
            .last_output_at
            .lock()
            .unwrap()
            .get(pane_id)
            .copied();
        last.map(|t| t.elapsed().as_secs_f64().max(0.0))
    }

    fn _record_control_mode_output(&self, pane_id: &str, payload: &str) {
        record_control_mode_output(&self.inner, pane_id, payload);
    }

    fn _request_detach(&self) {
        let fd = *self.inner.master_fd.lock().unwrap();
        if let Some(fd) = fd {
            let data = b"detach-client\n";
            unsafe {
                libc::write(fd, data.as_ptr() as *const libc::c_void, data.len());
            }
        }
    }
}

fn record_control_mode_output(inner: &MonitorInner, pane_id: &str, payload: &str) {
    if pane_id.is_empty() {
        return;
    }
    if !_control_mode_payload_has_activity(payload) {
        return;
    }
    inner
        .last_output_at
        .lock()
        .unwrap()
        .insert(pane_id.to_string(), Instant::now());
}

fn monitor_run_loop(inner: Arc<MonitorInner>, session_target: String) {
    while !inner.stop.load(Ordering::SeqCst) {
        // Best-effort monitor: fall back to retry rather than crashing hived.
        let _ = monitor_run_once(&inner, &session_target);
        if inner.stop.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_secs_f64(_CONTROL_MODE_RESTART_DELAY));
    }
}

fn monitor_openpty() -> std::io::Result<(i32, i32)> {
    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;
    let rv = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rv != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((master, slave))
}

fn terminate_child(child: &mut std::process::Child) {
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            _ => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn monitor_run_once(inner: &MonitorInner, session_target: &str) -> std::io::Result<()> {
    let (master, slave) = monitor_openpty()?;
    let mut cmd = Command::new("tmux");
    cmd.args(["-C", "attach", "-t", session_target]);
    unsafe {
        use std::os::unix::io::FromRawFd;
        use std::os::unix::process::CommandExt;
        let fds = [libc::dup(slave), libc::dup(slave), libc::dup(slave)];
        if fds.iter().any(|&fd| fd < 0) {
            for &fd in &fds {
                if fd >= 0 {
                    libc::close(fd);
                }
            }
            libc::close(slave);
            libc::close(master);
            return Err(std::io::Error::last_os_error());
        }
        cmd.stdin(Stdio::from_raw_fd(fds[0]));
        cmd.stdout(Stdio::from_raw_fd(fds[1]));
        cmd.stderr(Stdio::from_raw_fd(fds[2]));
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let spawn_res = cmd.spawn();
    unsafe {
        libc::close(slave);
    }
    let mut child = match spawn_res {
        Ok(c) => c,
        Err(e) => {
            unsafe {
                libc::close(master);
            }
            return Err(e);
        }
    };
    *inner.master_fd.lock().unwrap() = Some(master);

    let mut buffer: Vec<u8> = Vec::new();
    while !inner.stop.load(Ordering::SeqCst) {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        let mut pfd = libc::pollfd {
            fd: master,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, 500) };
        if ready <= 0 {
            continue;
        }
        let mut chunk = [0u8; 65536];
        let nread =
            unsafe { libc::read(master, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        if nread < 0 {
            break;
        }
        if nread == 0 {
            continue;
        }
        buffer.extend_from_slice(&chunk[..nread as usize]);
        while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
            let raw_line: Vec<u8> = buffer.drain(..=pos).collect();
            // Python decodes with errors="ignore": drop invalid bytes.
            let decoded =
                String::from_utf8_lossy(&raw_line[..raw_line.len() - 1]).replace('\u{fffd}', "");
            let decoded = decoded.trim_end_matches('\r');
            let (pane_id, payload) = parse_control_mode_output(decoded);
            if !pane_id.is_empty() {
                record_control_mode_output(inner, &pane_id, &payload);
            }
        }
    }
    terminate_child(&mut child);
    *inner.master_fd.lock().unwrap() = None;
    unsafe {
        libc::close(master);
    }
    Ok(())
}

// --- Session ---

pub fn has_session(name: &str) -> bool {
    match _run(&["has-session", "-t", name], false, 5) {
        Ok(r) => r.returncode == 0,
        // Python would raise OSError here (missing tmux); read it as "no".
        Err(_) => false,
    }
}

/// Create a detached tmux session. Returns the initial pane id.
pub fn new_session(name: &str, width: u32, height: u32) -> anyhow::Result<String> {
    let w = width.to_string();
    let h = height.to_string();
    let r = _run(
        &[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            &w,
            "-y",
            &h,
            "-P",
            "-F",
            "#{pane_id}",
        ],
        true,
        5,
    )?;
    Ok(r.stdout.trim().to_string())
}

pub fn kill_session(name: &str) {
    let _ = _run(&["kill-session", "-t", name], false, 5);
}

/// Create a new tmux window in *session*. Returns (window_target, pane_id).
pub fn new_window(
    session: &str,
    name: &str,
    cwd: Option<&str>,
    detach: bool,
) -> anyhow::Result<(String, String)> {
    // Force `-t` to reference a session, not a window index. Bare numeric
    // session names (e.g. "613") are ambiguous and tmux can treat `-t 613`
    // as an index rather than a session, which fails with "index N in use"
    // once any window exists at that index.
    let target = if session.contains(':') || session.starts_with('$') {
        session.to_string()
    } else {
        format!("{session}:")
    };
    let mut args: Vec<&str> = vec!["new-window", "-t", &target];
    if detach {
        args.push("-d");
    }
    if !name.is_empty() {
        args.push("-n");
        args.push(name);
    }
    if let Some(cwd) = cwd {
        args.push("-c");
        args.push(cwd);
    }
    args.extend(["-P", "-F", "#{session_name}:#{window_index}\t#{pane_id}"]);
    let r = _run(&args, true, 5)?;
    let out = r.stdout.trim().to_string();
    match out.split_once('\t') {
        None => Ok((out, String::new())),
        Some((target, pane_id)) => Ok((target.to_string(), pane_id.to_string())),
    }
}

/// Break *pane_id* out into its own new window. Returns (window_target, pane_id).
///
/// The pane's running process (e.g. agent CLI) continues — only its window
/// parent changes.
pub fn break_pane(pane_id: &str, name: &str, detach: bool) -> anyhow::Result<(String, String)> {
    let mut args: Vec<&str> = vec!["break-pane", "-s", pane_id];
    if detach {
        args.push("-d");
    }
    if !name.is_empty() {
        args.push("-n");
        args.push(name);
    }
    args.extend(["-P", "-F", "#{session_name}:#{window_index}\t#{pane_id}"]);
    let r = _run(&args, true, 5)?;
    let out = r.stdout.trim().to_string();
    match out.split_once('\t') {
        None => Ok((out, pane_id.to_string())),
        Some((target, new_pane_id)) => {
            let new_pane_id = if new_pane_id.is_empty() {
                pane_id
            } else {
                new_pane_id
            };
            Ok((target.to_string(), new_pane_id.to_string()))
        }
    }
}

/// Return (width, height) for *window_target*, or (0, 0) on error.
pub fn window_size(window_target: &str) -> (u32, u32) {
    let r = match _run(
        &[
            "display-message",
            "-t",
            window_target,
            "-p",
            "#{window_width}\t#{window_height}",
        ],
        false,
        5,
    ) {
        Ok(r) => r,
        Err(_) => return (0, 0),
    };
    let out = r.stdout.trim();
    match out.split_once('\t') {
        None => (0, 0),
        Some((w, h)) => match (w.parse(), h.parse()) {
            (Ok(w), Ok(h)) => (w, h),
            _ => (0, 0),
        },
    }
}

/// True when a pane in *window_target* is zoomed (unknown reads as False).
pub fn window_zoomed(window_target: &str) -> bool {
    match _run(
        &[
            "display-message",
            "-t",
            window_target,
            "-p",
            "#{window_zoomed_flag}",
        ],
        false,
        5,
    ) {
        Ok(r) => r.stdout.trim() == "1",
        Err(_) => false,
    }
}

/// Replace this process with `tmux attach` focused on *window_target*.
///
/// The outside-tmux tail of `hive attach`: attach to the session and select
/// the team's window in one tmux command chain. Only returns on exec failure.
pub fn exec_attach(session: &str, window_target: &str) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let err = Command::new("tmux")
        .args([
            "attach",
            "-t",
            session,
            ";",
            "select-window",
            "-t",
            window_target,
        ])
        .exec();
    Err(err.into())
}

pub fn select_window(window_target: &str) {
    let _ = _run(&["select-window", "-t", window_target], false, 5);
}

// --- Pane ---

/// Split a window/pane. Returns the new pane id.
///
/// detach=true (default at call sites) keeps focus on the original pane (-d flag).
pub fn split_window(
    target: &str,
    horizontal: bool,
    size: Option<&str>,
    detach: bool,
    cwd: Option<&str>,
) -> anyhow::Result<String> {
    let mut args: Vec<&str> = vec!["split-window", "-t", target];
    if detach {
        args.push("-d");
    }
    args.push(if horizontal { "-h" } else { "-v" });
    if let Some(size) = size {
        if !size.is_empty() {
            args.push("-l");
            args.push(size);
        }
    }
    if let Some(cwd) = cwd {
        args.push("-c");
        args.push(cwd);
    }
    args.extend(["-P", "-F", "#{pane_id}"]);
    match _run(&args, true, 5) {
        Ok(r) => Ok(r.stdout.trim().to_string()),
        Err(TmuxError::CalledProcess { stderr, .. }) => {
            let stderr = stderr.trim();
            let detail = if stderr.is_empty() {
                String::new()
            } else {
                format!(" ({stderr})")
            };
            Err(anyhow::anyhow!(
                "tmux refused to split {target}{detail} — the window is likely \
full; kill a finished member (hive kill <name>) and retry"
            ))
        }
        Err(e) => Err(e.into()),
    }
}

/// Send literal text to a pane, then optionally press Enter.
///
/// Uses two separate tmux invocations to avoid command-chaining (;)
/// interfering with literal text parsing, which caused truncation.
pub fn send_keys(pane_id: &str, text: &str, enter: bool) -> anyhow::Result<()> {
    _run(&["send-keys", "-t", pane_id, "-l", text], true, 5)?;
    if enter {
        _run(&["send-keys", "-t", pane_id, "Enter"], true, 5)?;
    }
    Ok(())
}

/// Send a special key (Escape, C-c, C-n, etc.).
pub fn send_key(pane_id: &str, key: &str) -> anyhow::Result<()> {
    _run(&["send-keys", "-t", pane_id, key], true, 5)?;
    Ok(())
}

/// Send multiple keys in one tmux call (atomic w.r.t. tmux server).
pub fn send_keys_batch(pane_id: &str, keys: &[&str]) -> anyhow::Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let mut args: Vec<&str> = vec!["send-keys", "-t", pane_id];
    args.extend_from_slice(keys);
    _run(&args, true, 5)?;
    Ok(())
}

/// Load data into a named tmux buffer via stdin.
///
/// Errors on failure (nonzero exit or timeout): callers clear the pane's
/// input on the strength of the buffer holding the draft, so a save that did
/// not happen must not read as one.
pub fn load_buffer(name: &str, data: &str) -> anyhow::Result<()> {
    let argv: Vec<String> = ["tmux", "load-buffer", "-b", name, "-"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let r = exec_capture(&argv, 5, Some(data))?;
    if r.returncode != 0 {
        return Err(TmuxError::CalledProcess {
            returncode: r.returncode,
            stderr: r.stderr,
        }
        .into());
    }
    Ok(())
}

/// Paste a named tmux buffer into a pane (optionally with bracketed-paste sequences).
pub fn paste_buffer(name: &str, target: &str, bracketed: bool) {
    let mut args: Vec<&str> = vec!["paste-buffer", "-b", name, "-t", target];
    if bracketed {
        args.insert(1, "-p");
    }
    let _ = _run(&args, false, 5);
}

pub fn delete_buffer(name: &str) {
    let _ = _run(&["delete-buffer", "-b", name], false, 5);
}

pub fn is_pane_in_mode(pane_id: &str) -> bool {
    display_value(pane_id, "#{pane_in_mode}").as_deref() == Some("1")
}

pub fn cancel_pane_mode(pane_id: &str) {
    let _ = _run(&["copy-mode", "-q", "-t", pane_id], false, 5);
}

/// Capture pane content.
pub fn capture_pane(pane_id: &str, lines: u32, preserve_styles: bool) -> anyhow::Result<String> {
    let start = format!("-{lines}");
    let mut args: Vec<&str> = vec!["capture-pane", "-t", pane_id];
    if preserve_styles {
        args.push("-e");
    }
    args.extend(["-p", "-S", &start]);
    _run_output(&args)
}

pub fn is_pane_alive(pane_id: &str) -> bool {
    let r = match _run(
        &["list-panes", "-a", "-F", "#{pane_id} #{pane_dead}"],
        false,
        5,
    ) {
        Ok(r) => r,
        Err(_) => return true,
    };
    if r.returncode != 0 {
        // tmux didn't answer (timeout / transient failure): unknown is not
        // dead. Callers take irreversible action on False (daemon reap, team
        // GC), so only a successful listing may declare a pane dead.
        return true;
    }
    for line in r.stdout.trim().split('\n') {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[0] == pane_id {
            return parts[1] == "0";
        }
    }
    false
}

pub fn kill_pane(pane_id: &str) {
    let _ = _run(&["kill-pane", "-t", pane_id], false, 5);
}

pub fn kill_window(target: &str) {
    let _ = _run(&["kill-window", "-t", target], false, 5);
}

// --- Layout & Appearance ---

pub fn select_layout(target: &str, layout: &str) {
    let _ = _run(&["select-layout", "-t", target, layout], false, 5);
}

pub fn set_pane_title(pane_id: &str, title: &str) {
    let _ = _run(&["select-pane", "-t", pane_id, "-T", title], false, 5);
}

// A claude member pane is an attach *viewer*: the human can switch it to
// another bg session while the pane keeps its member tags. The hived's view
// probe writes what is really on screen into `@hive-view` (empty while the
// pane shows its own member), so the border reads "name -> what you are
// actually looking at" without the format having to guess from the title.
// Both halves carry the team: with several teams on screen, a bare member
// name says nothing about which team a pane belongs to, and
// the view suffix already names its member as `<team>.<member>`.
pub const _HIVE_PANE_BORDER_FORMAT: &str = concat!(
    " #{?@hive-notify-active,#[fg=colour220]#[bold][!] #[default],}",
    "#{?@hive-agent,#{?@hive-team,#{@hive-team}.,}#{@hive-agent}",
    "#{?@hive-view,#[fg=colour220] -> #{@hive-view}#[default],}",
    ",#{pane_title}} "
);

/// Enable pane border labels for a window.
///
/// Hive-tagged panes show their member name; untagged panes fall back to the
/// native tmux pane title.
pub fn enable_pane_border_status(target: &str) {
    let _ = _run(
        &[
            "set-window-option",
            "-t",
            target,
            "pane-border-status",
            "top",
        ],
        false,
        5,
    );
    let _ = _run(
        &[
            "set-window-option",
            "-t",
            target,
            "pane-border-format",
            _HIVE_PANE_BORDER_FORMAT,
        ],
        false,
        5,
    );
}

/// Apply tmux window options expected for Hive-managed panes.
pub fn configure_hive_window(target: &str) {
    enable_pane_border_status(target);
    set_window_option(target, "monitor-activity", "off");
    set_window_option(target, "monitor-bell", "off");
}

pub fn set_window_option(target: &str, option: &str, value: &str) {
    let _ = _run(
        &["set-window-option", "-t", target, option, value],
        false,
        5,
    );
}

pub fn get_window_option(target: &str, key: &str) -> Option<String> {
    let fmt = format!("#{{@{key}}}");
    let r = _run(&["display-message", "-t", target, "-p", &fmt], false, 5).ok()?;
    let val = r.stdout.trim().to_string();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

/// Global (server-wide) window-option value — read-only, no target.
///
/// Values keep their exact spacing (status formats carry meaningful
/// leading/trailing padding); only the trailing newline is removed.
pub fn get_global_window_option(option: &str) -> Option<String> {
    let r = _run(&["show-options", "-w", "-g", "-v", option], false, 5).ok()?;
    let val = r.stdout.trim_end_matches('\n').to_string();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

pub fn clear_window_option(target: &str, option: &str) {
    let _ = _run(&["set-window-option", "-t", target, "-u", option], false, 5);
}

/// List all pane ids in a window/session.
pub fn list_panes(target: &str) -> Vec<String> {
    match _run(&["list-panes", "-t", target, "-F", "#{pane_id}"], false, 5) {
        Ok(r) => r
            .stdout
            .trim()
            .split('\n')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

// --- Context detection ---

fn env_string(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

/// True inside a tmux client — or inside a member engine's tool subprocess.
///
/// A claude bg engine runs on the supervisor's pty, not in any tmux client,
/// so its tools see no reliable $TMUX; but the member's pane identity is
/// resolvable from the engine's own env markers, and the tmux server on the
/// default socket answers targeted commands without $TMUX. Gating on $TMUX
/// alone would lock every member out of hive.
pub fn is_inside_tmux() -> bool {
    if !env_string("TMUX").is_empty() {
        return true;
    }
    _member_env_pane().is_some()
}

/// Pane resolved from a member engine's per-tool env markers, or None.
///
/// - codex injects the thread's `CODEX_THREAD_ID` into tool subprocesses;
///   hive records which pane each thread is bound to.
/// - a claude bg engine's tools carry `CLAUDE_CODE_MESSAGING_SOCKET`
///   (`/tmp/cc-socks/<enginePid>.sock`); the engine's registry entry names
///   its jobId, and hive records which pane each job is bound to. An
///   interactive claude session's tools carry the socket too, but have no
///   bg registry entry (and no job record), so they fall through.
fn _member_env_pane() -> Option<String> {
    let thread_id = env_string("CODEX_THREAD_ID").trim().to_string();
    if !thread_id.is_empty() {
        if let Some(pane) = crate::adapters::codex_app_server::pane_for_thread(&thread_id) {
            if !pane.is_empty() {
                return Some(pane);
            }
        }
    }
    let sock = env_string("CLAUDE_CODE_MESSAGING_SOCKET")
        .trim()
        .to_string();
    if !sock.is_empty() {
        let base = sock.rsplit('/').next().unwrap_or("");
        let stem = match base.rfind('.') {
            Some(i) => &base[..i],
            None => base,
        };
        if !stem.is_empty() && stem.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(pid) = stem.parse::<u32>() {
                if let Some(engine) = crate::adapters::claude_bg::engine_session_for_pid(pid) {
                    if let Some(pane) = crate::adapters::claude_bg::pane_for_job(&engine.job_id) {
                        if !pane.is_empty() {
                            return Some(pane);
                        }
                    }
                }
            }
        }
    }
    // A per-pane daemon (the grok leader) pins its member's TMUX_PANE into
    // the env it spawns tools with, but carries no $TMUX — grok has no
    // per-CLI marker of its own, so the pinned pane is its identity. Trust
    // it only when the pane is real on the default server.
    let pinned = env_string("TMUX_PANE").trim().to_string();
    if !pinned.is_empty() && env_string("TMUX").is_empty() {
        if let Ok(r) = _run(
            &["display-message", "-t", &pinned, "-p", "#{pane_id}"],
            false,
            5,
        ) {
            if r.stdout.trim() == pinned {
                return Some(pinned);
            }
        }
    }
    None
}

/// Get the pane id of the calling process.
///
/// Inside a member engine's tool subprocess the env's TMUX_PANE is
/// unreliable — the codex shared daemon's env is frozen at spawn time (and
/// hive strips TMUX_PANE from it), and a claude bg engine has none at all —
/// so the per-CLI identity markers win over the env var (see
/// `_member_env_pane`); everywhere else the per-pane TMUX_PANE env var
/// is the answer.
pub fn get_current_pane_id() -> Option<String> {
    if let Some(pane) = _member_env_pane() {
        if !pane.is_empty() {
            return Some(pane);
        }
    }
    std::env::var("TMUX_PANE").ok()
}

fn current_pane_display(fmt: &str) -> Option<String> {
    let pane_id = get_current_pane_id()?;
    if pane_id.is_empty() {
        return None;
    }
    let r = _run(&["display-message", "-t", &pane_id, "-p", fmt], false, 5).ok()?;
    let out = r.stdout.trim().to_string();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Get the window target that contains the calling pane.
pub fn get_current_window_target() -> Option<String> {
    current_pane_display("#{session_name}:#{window_index}")
}

/// Get the tmux session name for the calling pane.
pub fn get_current_session_name() -> Option<String> {
    current_pane_display("#{session_name}")
}

/// Get the window index for the calling pane.
pub fn get_current_window_index() -> Option<String> {
    current_pane_display("#{window_index}")
}

/// Get the stable tmux window id for the calling pane.
pub fn get_current_window_id() -> Option<String> {
    let pane_id = get_current_pane_id()?;
    if pane_id.is_empty() {
        return None;
    }
    display_value(&pane_id, "#{window_id}")
}

pub fn display_value(target: &str, fmt: &str) -> Option<String> {
    let r = _run(&["display-message", "-t", target, "-p", fmt], false, 5).ok()?;
    let val = r.stdout.trim().to_string();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

/// True only when tmux resolves `window_id` to itself.
///
/// Never errors: a missing tmux binary, timeout, nonzero exit, or mismatched
/// id all mean "not alive" to callers making reap decisions.
pub fn window_exists(window_id: &str) -> bool {
    if window_id.is_empty() {
        return false;
    }
    match _run(
        &["display-message", "-t", window_id, "-p", "#{window_id}"],
        false,
        5,
    ) {
        Ok(r) => r.returncode == 0 && r.stdout.trim() == window_id,
        Err(_) => false,
    }
}

/// Open a popup over `target` running `shell_command`. Never errors.
#[allow(clippy::too_many_arguments)]
pub fn display_popup(
    target: &str,
    shell_command: &str,
    client: &str,
    x: &str,
    y: &str,
    width: &str,
    height: &str,
    borderless: bool,
    close_on_exit: bool,
    timeout: u64,
) {
    let mut args: Vec<&str> = vec!["display-popup"];
    if !client.is_empty() {
        args.push("-c");
        args.push(client);
    }
    args.push("-t");
    args.push(target);
    if borderless {
        args.push("-B");
    }
    if !x.is_empty() {
        args.push("-x");
        args.push(x);
    }
    if !y.is_empty() {
        args.push("-y");
        args.push(y);
    }
    if !width.is_empty() {
        args.push("-w");
        args.push(width);
    }
    if !height.is_empty() {
        args.push("-h");
        args.push(height);
    }
    if close_on_exit {
        args.push("-E");
    }
    args.push(shell_command);
    let _ = _run(&args, false, timeout);
}

/// `run-shell -b <command>`: the shell string is passed byte-for-byte.
pub fn run_shell_detached(command: &str) {
    let _ = _run(&["run-shell", "-b", command], false, 5);
}

/// Source a tmux conf; false on missing tmux, timeout, or nonzero exit.
pub fn source_file(path: &str) -> bool {
    matches!(_run(&["source-file", path], false, 5), Ok(r) if r.returncode == 0)
}

pub fn get_most_recent_client_tty(session_name: Option<&str>) -> Option<String> {
    let rows = _list_terminal_clients(session_name);
    rows.into_iter().next().map(|row| row.2)
}

pub fn get_most_recent_terminal_client_pane(session_name: Option<&str>) -> Option<String> {
    let rows = _list_terminal_clients(session_name);
    rows.into_iter().next().map(|row| row.1)
}

fn _list_terminal_clients(session_name: Option<&str>) -> Vec<(i64, String, String)> {
    let mut args: Vec<&str> = vec!["list-clients"];
    if let Some(session) = session_name {
        if !session.is_empty() {
            args.push("-t");
            args.push(session);
        }
    }
    args.extend([
        "-F",
        "#{client_activity}\t#{client_control_mode}\t#{pane_id}\t#{client_tty}",
    ]);
    let r = match _run(&args, false, 5) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut rows: Vec<(i64, String, String)> = Vec::new();
    for line in r.stdout.trim().split('\n') {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() != 4 || parts[1] != "0" || parts[2].is_empty() || parts[3].is_empty() {
            continue;
        }
        let raw = if parts[0].is_empty() { "0" } else { parts[0] };
        let activity: i64 = raw.parse().unwrap_or(0);
        rows.push((activity, parts[2].to_string(), parts[3].to_string()));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows
}

pub fn get_client_window_target(client_tty: &str) -> Option<String> {
    if client_tty.is_empty() {
        return None;
    }
    let r = _run(
        &[
            "display-message",
            "-c",
            client_tty,
            "-p",
            "#{session_name}:#{window_index}",
        ],
        false,
        5,
    )
    .ok()?;
    let out = r.stdout.trim().to_string();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

pub fn get_most_recent_client_window(session_name: Option<&str>) -> Option<String> {
    let client_tty = get_most_recent_client_tty(session_name)?;
    if client_tty.is_empty() {
        return None;
    }
    get_client_window_target(&client_tty)
}

pub fn get_client_mode(target: Option<&str>) -> String {
    let resolved_target = match target {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => match get_current_pane_id() {
            Some(p) if !p.is_empty() => p,
            _ => return "unknown".to_string(),
        },
    };
    match display_value(&resolved_target, "#{client_control_mode}").as_deref() {
        Some("1") => "control".to_string(),
        Some("0") => "terminal".to_string(),
        _ => "unknown".to_string(),
    }
}

pub fn is_control_mode_client(target: Option<&str>) -> bool {
    get_client_mode(target) == "control"
}

pub fn get_pane_window_name(pane_id: &str) -> Option<String> {
    display_value(pane_id, "#{window_name}")
}

pub fn rename_window(window_target: &str, name: &str) {
    let _ = _run(&["rename-window", "-t", window_target, name], false, 5);
}

pub fn get_pane_tty(pane_id: &str) -> Option<String> {
    display_value(pane_id, "#{pane_tty}")
}

pub fn get_pane_title(pane_id: &str) -> Option<String> {
    display_value(pane_id, "#{pane_title}")
}

pub fn get_pane_current_command(pane_id: &str) -> Option<String> {
    display_value(pane_id, "#{pane_current_command}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TTYProcessInfo {
    pub pid: String,
    pub command: String,
    pub argv: String,
}

/// Python `row.split(None, 2)`: split on whitespace runs, at most 3 parts.
fn split_whitespace_max3(row: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = Vec::new();
    let mut rest = row.trim_start();
    for _ in 0..2 {
        match rest.find(|c: char| c.is_whitespace()) {
            Some(idx) => {
                parts.push(&rest[..idx]);
                rest = rest[idx..].trim_start();
            }
            None => break,
        }
    }
    if !rest.is_empty() {
        parts.push(rest);
    }
    parts
}

pub fn list_tty_processes(tty: &str) -> Vec<TTYProcessInfo> {
    let mut tty_name = tty.trim().to_string();
    if tty_name.is_empty() {
        return Vec::new();
    }
    if let Some(stripped) = tty_name.strip_prefix("/dev/") {
        tty_name = stripped.to_string();
    }
    let argv: Vec<String> = ["ps", "-t", &tty_name, "-o", "pid=,comm=,command="]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let result = match exec_capture(&argv, 5, None) {
        Ok(r) => r,
        // TimeoutExpired -> []; a missing ps binary degrades the same way.
        Err(_) => return Vec::new(),
    };
    let mut processes: Vec<TTYProcessInfo> = Vec::new();
    for line in result.stdout.lines() {
        let row = line.trim();
        if row.is_empty() {
            continue;
        }
        let parts = split_whitespace_max3(row);
        if parts.len() < 2 {
            continue;
        }
        processes.push(TTYProcessInfo {
            pid: parts[0].to_string(),
            command: parts[1].to_string(),
            argv: if parts.len() > 2 { parts[2] } else { parts[1] }.to_string(),
        });
    }
    processes
}

pub fn list_tty_commands(tty: &str) -> Vec<String> {
    list_tty_processes(tty)
        .into_iter()
        .map(|process| process.command)
        .collect()
}

pub fn get_pane_window_target(pane_id: &str) -> Option<String> {
    display_value(pane_id, "#{session_name}:#{window_index}")
}

pub fn get_window_id(target: &str) -> Option<String> {
    display_value(target, "#{window_id}")
}

pub fn get_pane_session_name(pane_id: &str) -> Option<String> {
    display_value(pane_id, "#{session_name}")
}

pub fn get_pane_count(pane_id: &str) -> u32 {
    let value = display_value(pane_id, "#{window_panes}").unwrap_or_else(|| "1".to_string());
    value.parse().unwrap_or(1)
}

/// Minimal `shlex.quote`.
fn shlex_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let safe = s.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '-')
    });
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

pub fn flash_window_status(window_target: &str, style: &str, seconds: i64) {
    let duration = std::cmp::max(1, seconds);
    let quoted_target = shlex_quote(window_target);
    let quoted_style = shlex_quote(style);
    let set_cmd = format!(
        "tmux set-window-option -t {quoted_target} window-status-style {quoted_style} >/dev/null 2>&1 || true"
    );
    let clear_cmd = format!(
        "tmux set-window-option -t {quoted_target} -u window-status-style >/dev/null 2>&1 || true"
    );
    let mut parts: Vec<String> = Vec::new();
    for _ in 0..duration {
        parts.push(set_cmd.clone());
        parts.push("sleep 0.5".to_string());
        parts.push(clear_cmd.clone());
        parts.push("sleep 0.5".to_string());
    }
    parts.push(clear_cmd.clone());
    let joined = parts.join("; ");
    let _ = _run(&["run-shell", "-b", &joined], false, 5);
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaneInfo {
    pub pane_id: String,
    pub title: String,
    pub command: String,
    pub role: String,
    pub agent: String,
    pub team: String,
    pub cli: String,
    pub group: String,
}

/// List all panes in a window with their IDs and titles.
pub fn list_panes_with_titles(target: &str) -> Vec<PaneInfo> {
    let r = match _run(
        &[
            "list-panes",
            "-t",
            target,
            "-F",
            "#{pane_id}\t#{pane_title}",
        ],
        false,
        5,
    ) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut result = Vec::new();
    for line in r.stdout.trim().split('\n') {
        if line.is_empty() {
            continue;
        }
        let (pane_id, title) = match line.split_once('\t') {
            Some((p, t)) => (p, t),
            None => (line, ""),
        };
        result.push(PaneInfo {
            pane_id: pane_id.to_string(),
            title: title.to_string(),
            ..Default::default()
        });
    }
    result
}

// Field tables drive both the tmux format string and the parser: adding a
// column means adding one entry to the format and one field to the parse —
// no count literals to keep in sync beyond the field count consts.
pub const _PANE_BASE_FMT: &str = concat!(
    "#{pane_id}\t#{pane_title}\t#{pane_current_command}\t#{@hive-role}\t",
    "#{@hive-agent}\t#{@hive-team}\t#{@hive-cli}\t#{@hive-group}"
);
const _PANE_FIELD_COUNT: usize = 8;

pub const _TEAM_WINDOW_FMT: &str = concat!(
    "#{session_name}:#{window_index}\t#{window_name}\t#{window_id}\t",
    "#{@hive-team}\t#{@hive-workspace}\t#{@hive-created}\t#{@hive-pr}"
);
const _TEAM_WINDOW_FIELD_COUNT: usize = 7;

/// Entry of `list_team_windows_status` (the Python dict keys were
/// window/windowName/windowId/team/workspace/created/pr).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TeamWindow {
    pub window: String,
    pub window_name: String,
    pub window_id: String,
    pub team: String,
    pub workspace: String,
    pub created: String,
    pub pr: String,
}

fn _split_fields(line: &str, count: usize) -> Vec<String> {
    let mut parts: Vec<String> = line.split('\t').map(str::to_string).collect();
    while parts.len() < count {
        parts.push(String::new());
    }
    parts.truncate(count);
    parts
}

/// List all panes with command and hive identity (@hive-*).
pub fn list_panes_full(target: &str) -> Vec<PaneInfo> {
    list_panes_full_or_none(target).unwrap_or_default()
}

/// Status-aware `list_panes_full`: None when tmux did not answer.
///
/// A successful-but-empty listing is a real empty window; None means the
/// caller cannot tell missing panes from a transient tmux failure and must
/// not take irreversible action on it (same contract as `is_pane_alive`).
pub fn list_panes_full_or_none(target: &str) -> Option<Vec<PaneInfo>> {
    let r = _run(
        &["list-panes", "-t", target, "-F", _PANE_BASE_FMT],
        false,
        5,
    )
    .ok()?;
    if r.returncode != 0 {
        return None;
    }
    Some(_parse_panes_full(&r.stdout))
}

/// List every pane across all sessions/windows with hive identity tags.
pub fn list_panes_all() -> Vec<PaneInfo> {
    match _run(&["list-panes", "-a", "-F", _PANE_BASE_FMT], false, 5) {
        Ok(r) => _parse_panes_full(&r.stdout),
        Err(_) => Vec::new(),
    }
}

/// True only when tmux stderr proves there is no server to talk to.
///
/// Proven messages: "no server running on <path>" (clean shutdown) and
/// "error connecting to <path> (No such file or directory)" (socket gone).
/// Anything else — permission denied, connection refused, unexpected text —
/// stays unknown: a server may well be alive behind the failure.
fn _stderr_means_no_server(stderr: &str) -> bool {
    let low = stderr.to_lowercase();
    if low.contains("no server running") {
        return true;
    }
    low.contains("error connecting") && low.contains("no such file or directory")
}

/// Status-aware `list_panes_all`: `(panes, "ok")` on success.
///
/// `(None, "no-server")` when no tmux server is running (nothing can be
/// live), `(None, "unknown")` on any other failure — callers must not
/// read unknown as "dead" (same contract as `is_pane_alive`).
pub fn list_panes_all_status() -> (Option<Vec<PaneInfo>>, &'static str) {
    let r = match _run(&["list-panes", "-a", "-F", _PANE_BASE_FMT], false, 5) {
        Ok(r) => r,
        Err(_) => return (None, "unknown"),
    };
    if r.returncode == 0 {
        return (Some(_parse_panes_full(&r.stdout)), "ok");
    }
    if _stderr_means_no_server(&r.stderr) {
        return (None, "no-server");
    }
    (None, "unknown")
}

/// Status-aware scan of windows carrying `@hive-team`.
///
/// Same (value, status) contract as `list_panes_all_status`. Each
/// entry: window target/name/id plus the team, workspace, and created
/// options — everything `hive ls` needs to match a live
/// team instance against a snapshot.
pub fn list_team_windows_status() -> (Option<Vec<TeamWindow>>, &'static str) {
    let r = match _run(&["list-windows", "-a", "-F", _TEAM_WINDOW_FMT], false, 5) {
        Ok(r) => r,
        Err(_) => return (None, "unknown"),
    };
    if r.returncode != 0 {
        if _stderr_means_no_server(&r.stderr) {
            return (None, "no-server");
        }
        return (None, "unknown");
    }
    let mut out: Vec<TeamWindow> = Vec::new();
    for line in r.stdout.trim().split('\n') {
        if line.is_empty() {
            continue;
        }
        let p = _split_fields(line, _TEAM_WINDOW_FIELD_COUNT);
        if p[3].is_empty() {
            continue;
        }
        out.push(TeamWindow {
            window: p[0].clone(),
            window_name: p[1].clone(),
            window_id: p[2].clone(),
            team: p[3].clone(),
            workspace: p[4].clone(),
            created: p[5].clone(),
            pr: p[6].clone(),
        });
    }
    (Some(out), "ok")
}

/// Return tmux window indices in *session*, ignoring non-numeric output.
pub fn list_window_indices(session: &str) -> Vec<u32> {
    let r = match _run(
        &["list-windows", "-t", session, "-F", "#{window_index}"],
        false,
        5,
    ) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<u32> = Vec::new();
    for line in r.stdout.trim().split('\n') {
        let line = line.trim();
        if line.is_empty() || !line.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Ok(idx) = line.parse() {
            out.push(idx);
        }
    }
    out
}

/// Return `(window_target, window_name)` for every window across sessions.
pub fn list_window_names() -> Vec<(String, String)> {
    let r = match _run(
        &[
            "list-windows",
            "-a",
            "-F",
            "#{session_name}:#{window_index}\t#{window_name}",
        ],
        false,
        5,
    ) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for line in r.stdout.trim().split('\n') {
        if let Some((target, name)) = line.split_once('\t') {
            out.push((target.to_string(), name.to_string()));
        }
    }
    out
}

fn _parse_panes_full(stdout: &str) -> Vec<PaneInfo> {
    let mut result: Vec<PaneInfo> = Vec::new();
    for line in stdout.trim().split('\n') {
        if line.is_empty() {
            continue;
        }
        let p = _split_fields(line, _PANE_FIELD_COUNT);
        result.push(PaneInfo {
            pane_id: p[0].clone(),
            title: p[1].clone(),
            command: p[2].clone(),
            role: p[3].clone(),
            agent: p[4].clone(),
            team: p[5].clone(),
            cli: p[6].clone(),
            group: p[7].clone(),
        });
    }
    result
}

// --- Per-pane user options (@hive-*) ---

pub fn set_pane_option(pane_id: &str, key: &str, value: &str) {
    let opt = format!("@{key}");
    let _ = _run(&["set-option", "-p", "-t", pane_id, &opt, value], false, 5);
}

pub fn get_pane_option(pane_id: &str, key: &str) -> Option<String> {
    let opt = format!("@{key}");
    let r = _run(&["show-options", "-p", "-v", "-t", pane_id, &opt], false, 5).ok()?;
    if r.returncode != 0 {
        return None;
    }
    let val = r.stdout.trim().to_string();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

pub fn clear_pane_option(pane_id: &str, key: &str) {
    let opt = format!("@{key}");
    let _ = _run(&["set-option", "-p", "-t", pane_id, "-u", &opt], false, 5);
}

// `hive-view` is derived state (the claude view probe writes it), not
// identity — but release must clear it with the rest, or a reused pane keeps
// rendering a border suffix nobody owns any more.
const _PANE_TAG_KEYS: [&str; 7] = [
    "hive-role",
    "hive-agent",
    "hive-team",
    "hive-cli",
    "hive-group",
    "hive-owner",
    "hive-view",
];

/// Set all hive identity options on a pane.
pub fn tag_pane(pane_id: &str, role: &str, agent: &str, team: &str, cli: &str, group: &str) {
    set_pane_option(pane_id, "hive-role", role);
    set_pane_option(pane_id, "hive-agent", agent);
    set_pane_option(pane_id, "hive-team", team);
    if !cli.is_empty() {
        set_pane_option(pane_id, "hive-cli", cli);
        if cli != "claude" {
            // Only the claude view tick maintains `hive-view`, and it skips
            // non-claude panes — so a pane retagged onto another CLI in place
            // would keep its last ' -> <session>' suffix forever.
            clear_pane_option(pane_id, "hive-view");
        }
    }
    if !group.is_empty() {
        set_pane_option(pane_id, "hive-group", group);
    }
}

/// Remove all hive identity options from a pane.
pub fn clear_pane_tags(pane_id: &str) {
    for key in _PANE_TAG_KEYS {
        clear_pane_option(pane_id, key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    // env vars are process-global: every test touching them takes this lock.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    type Calls = Rc<RefCell<Vec<(Vec<String>, bool, u64)>>>;

    fn set_run_override(f: impl FnMut(&[String], bool, u64) -> Result<Run, TmuxError> + 'static) {
        RUN_OVERRIDE.with(|o| *o.borrow_mut() = Some(Box::new(f)));
    }

    fn set_exec_override(
        f: impl FnMut(&[String], u64, Option<&str>) -> Result<Run, TmuxError> + 'static,
    ) {
        EXEC_OVERRIDE.with(|o| *o.borrow_mut() = Some(Box::new(f)));
    }

    fn ok_run(returncode: i32, stdout: &str, stderr: &str) -> Run {
        Run {
            returncode,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn _timeout_run() {
        set_exec_override(|_argv, _timeout, _input| Err(TmuxError::Timeout));
    }

    fn _capture_run(rc: i32, out: &'static str) -> Calls {
        let calls: Calls = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&calls);
        set_run_override(move |args, check, timeout| {
            recorded.borrow_mut().push((args.to_vec(), check, timeout));
            Ok(ok_run(rc, out, ""))
        });
        calls
    }

    fn _raising_run() {
        set_run_override(|_args, _check, _timeout| Err(TmuxError::Os("no tmux".to_string())));
    }

    fn is_timeout(err: &anyhow::Error) -> bool {
        matches!(err.downcast_ref::<TmuxError>(), Some(TmuxError::Timeout))
    }

    #[test]
    fn test_run_probe_reads_timeout_as_unknown() {
        _timeout_run();

        let result = _run(&["list-panes"], false, 5).unwrap();

        assert_eq!(result.returncode, 1);
        assert_eq!(result.stderr, "timeout");
    }

    #[test]
    fn test_run_timeout_raises_when_the_command_had_to_happen() {
        // check=true means the caller needs the command to have run: a busy tmux
        // server must not be able to fake a successful send-keys.
        _timeout_run();

        assert!(matches!(
            _run(&["list-panes"], true, 5),
            Err(TmuxError::Timeout)
        ));
        assert!(is_timeout(&send_keys("%1", "hello", true).unwrap_err()));
        assert!(is_timeout(&send_key("%1", "Escape").unwrap_err()));
    }

    #[test]
    fn test_load_buffer_timeout_raises() {
        // A draft save that did not happen must not read as one — the caller
        // clears the pane's composer on the strength of this call.
        _timeout_run();

        assert!(is_timeout(
            &load_buffer("hive_draft_1", "unsent thought").unwrap_err()
        ));
    }

    #[test]
    fn test_session_helpers_delegate_to_tmux() {
        let calls: Calls = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&calls);
        set_run_override(move |args, check, timeout| {
            recorded.borrow_mut().push((args.to_vec(), check, timeout));
            if args[0] == "has-session" {
                return Ok(ok_run(0, "", ""));
            }
            if args[0] == "new-session" {
                return Ok(ok_run(0, "%9\n", ""));
            }
            Ok(ok_run(0, "", ""))
        });

        assert!(has_session("dev"));
        assert_eq!(new_session("dev", 200, 50).unwrap(), "%9");
        kill_session("dev");

        let calls = calls.borrow();
        assert_eq!(calls[0].0[..3], v(&["has-session", "-t", "dev"]));
        assert_eq!(calls[1].0[0], "new-session");
        assert_eq!(calls[2].0, v(&["kill-session", "-t", "dev"]));
    }

    #[test]
    fn test_send_keys_and_send_key_issue_expected_tmux_commands() {
        let calls = _capture_run(0, "");

        send_keys("%1", "hello", true).unwrap();
        send_keys("%2", "raw", false).unwrap();
        send_key("%3", "Escape").unwrap();

        let calls = calls.borrow();
        let got: Vec<(Vec<String>, bool)> = calls.iter().map(|c| (c.0.clone(), c.1)).collect();
        assert_eq!(
            got,
            vec![
                (v(&["send-keys", "-t", "%1", "-l", "hello"]), true),
                (v(&["send-keys", "-t", "%1", "Enter"]), true),
                (v(&["send-keys", "-t", "%2", "-l", "raw"]), true),
                (v(&["send-keys", "-t", "%3", "Escape"]), true),
            ]
        );
    }

    #[test]
    fn test_pane_mode_helpers_use_tmux_display_and_copy_mode() {
        let calls: Calls = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&calls);
        set_run_override(move |args, check, timeout| {
            recorded.borrow_mut().push((args.to_vec(), check, timeout));
            let stdout = if args.len() >= 3 && args[..3] == v(&["display-message", "-t", "%1"]) {
                "1\n"
            } else {
                ""
            };
            Ok(ok_run(0, stdout, ""))
        });

        assert!(is_pane_in_mode("%1"));
        cancel_pane_mode("%1");

        let calls = calls.borrow();
        let got: Vec<(Vec<String>, bool)> = calls.iter().map(|c| (c.0.clone(), c.1)).collect();
        assert_eq!(
            got,
            vec![
                (
                    v(&["display-message", "-t", "%1", "-p", "#{pane_in_mode}"]),
                    false
                ),
                (v(&["copy-mode", "-q", "-t", "%1"]), false),
            ]
        );
    }

    #[test]
    fn test_capture_and_list_parsers() {
        set_run_override(|args, _check, _timeout| {
            if args[0] == "capture-pane" {
                return Ok(ok_run(0, "line1\nline2\n", ""));
            }
            if args.iter().any(|a| a == "#{pane_id}") {
                return Ok(ok_run(0, "%1\n%2\n", ""));
            }
            Ok(ok_run(0, "", ""))
        });

        assert_eq!(capture_pane("%1", 5, false).unwrap(), "line1\nline2");
        assert_eq!(list_panes("dev:0"), vec!["%1", "%2"]);
    }

    #[test]
    fn test_is_pane_alive_parses_tmux_output() {
        set_run_override(|_args, _check, _timeout| Ok(ok_run(0, "%1 0\n%2 1\n", "")));

        assert!(is_pane_alive("%1"));
        assert!(!is_pane_alive("%2"));
        assert!(!is_pane_alive("%9"));
    }

    #[test]
    fn test_is_pane_alive_treats_tmux_failure_as_alive() {
        set_run_override(|_args, _check, _timeout| Ok(ok_run(1, "", "timeout")));

        assert!(is_pane_alive("%1"));
    }

    #[test]
    fn test_context_helpers_use_environment_and_display_message() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("TMUX", "/tmp/tmux-1");
        std::env::set_var("TMUX_PANE", "%7");
        std::env::remove_var("CODEX_THREAD_ID");
        std::env::remove_var("CLAUDE_CODE_MESSAGING_SOCKET");
        set_run_override(|args, _check, _timeout| {
            let stdout = if args.iter().any(|a| a == "#{session_name}:#{window_index}") {
                "dev:2\n"
            } else if args.iter().any(|a| a == "#{session_name}") {
                "dev\n"
            } else if args.iter().any(|a| a == "#{window_id}") {
                "@42\n"
            } else {
                "2\n"
            };
            Ok(ok_run(0, stdout, ""))
        });

        assert!(is_inside_tmux());
        assert_eq!(get_current_pane_id().as_deref(), Some("%7"));
        assert_eq!(get_current_window_target().as_deref(), Some("dev:2"));
        assert_eq!(get_current_session_name().as_deref(), Some("dev"));
        assert_eq!(get_current_window_index().as_deref(), Some("2"));
        assert_eq!(get_current_window_id().as_deref(), Some("@42"));
        assert_eq!(get_window_id("dev:2").as_deref(), Some("@42"));

        std::env::remove_var("TMUX");
        std::env::remove_var("TMUX_PANE");
    }

    #[test]
    fn test_client_mode_and_popup_support_helpers() {
        set_run_override(|args, _check, _timeout| {
            let stdout = if args.iter().any(|a| a == "#{client_control_mode}") {
                "1\n"
            } else {
                ""
            };
            Ok(ok_run(0, stdout, ""))
        });

        assert_eq!(get_client_mode(Some("%7")), "control");
        assert!(is_control_mode_client(Some("%7")));
    }

    #[test]
    fn test_client_mode_returns_terminal_or_unknown() {
        set_run_override(|_args, _check, _timeout| Ok(ok_run(0, "0\n", "")));
        assert_eq!(get_client_mode(Some("%8")), "terminal");
        assert!(!is_control_mode_client(Some("%8")));

        set_run_override(|_args, _check, _timeout| Ok(ok_run(0, "", "")));
        assert_eq!(get_client_mode(Some("%8")), "unknown");
    }

    #[test]
    fn test_client_window_helpers_resolve_most_recent_client() {
        set_run_override(|args, _check, _timeout| {
            if args[0] == "list-clients" {
                return Ok(ok_run(
                    0,
                    "10\t0\t%1\t/dev/ttys010\n50\t0\t%5\t/dev/ttys050\n",
                    "",
                ));
            }
            Ok(ok_run(0, "dev:5\n", ""))
        });

        assert_eq!(
            get_most_recent_client_tty(Some("dev")).as_deref(),
            Some("/dev/ttys050")
        );
        assert_eq!(
            get_most_recent_terminal_client_pane(Some("dev")).as_deref(),
            Some("%5")
        );
        assert_eq!(
            get_client_window_target("/dev/ttys050").as_deref(),
            Some("dev:5")
        );
        assert_eq!(
            get_most_recent_client_window(Some("dev")).as_deref(),
            Some("dev:5")
        );
    }

    #[test]
    fn test_client_helpers_ignore_control_mode_clients() {
        set_run_override(|args, _check, _timeout| {
            if args[0] == "list-clients" {
                return Ok(ok_run(
                    0,
                    "99\t1\t%control\t/dev/ttys999\n20\t0\t%term\t/dev/ttys020\n",
                    "",
                ));
            }
            Ok(ok_run(0, "dev:2\n", ""))
        });

        assert_eq!(
            get_most_recent_client_tty(Some("dev")).as_deref(),
            Some("/dev/ttys020")
        );
        assert_eq!(
            get_most_recent_terminal_client_pane(Some("dev")).as_deref(),
            Some("%term")
        );
        assert_eq!(
            get_most_recent_client_window(Some("dev")).as_deref(),
            Some("dev:2")
        );
    }

    #[test]
    fn test_list_tty_processes_and_commands_strip_dev_prefix_and_parse_output() {
        let calls: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&calls);
        set_exec_override(move |argv, _timeout, _input| {
            recorded.borrow_mut().push(argv.to_vec());
            Ok(ok_run(
                0,
                "35214 -zsh -zsh\n35988 claude claude --verbose\n",
                "",
            ))
        });

        let processes = list_tty_processes("/dev/ttys012");
        assert_eq!(
            processes,
            vec![
                TTYProcessInfo {
                    pid: "35214".to_string(),
                    command: "-zsh".to_string(),
                    argv: "-zsh".to_string(),
                },
                TTYProcessInfo {
                    pid: "35988".to_string(),
                    command: "claude".to_string(),
                    argv: "claude --verbose".to_string(),
                },
            ]
        );
        assert_eq!(list_tty_commands("/dev/ttys012"), vec!["-zsh", "claude"]);
        let expected = v(&["ps", "-t", "ttys012", "-o", "pid=,comm=,command="]);
        assert_eq!(*calls.borrow(), vec![expected.clone(), expected]);
    }

    #[test]
    fn test_current_window_helpers_return_none_without_tmux_pane() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var("TMUX_PANE");
        std::env::remove_var("TMUX");
        std::env::remove_var("CODEX_THREAD_ID");
        std::env::remove_var("CLAUDE_CODE_MESSAGING_SOCKET");

        assert_eq!(get_current_window_target(), None);
        assert_eq!(get_current_session_name(), None);
        assert_eq!(get_current_window_index(), None);
        assert_eq!(get_current_window_id(), None);
    }

    #[test]
    fn test_list_panes_with_titles_and_full_parse_rows() {
        set_run_override(|args, _check, _timeout| {
            let fmt = args.last().map(String::as_str).unwrap_or("");
            let stdout = if fmt == "#{pane_id}\t#{pane_title}" {
                "%1\tmain\n%2\tworker\n"
            } else if fmt == _PANE_BASE_FMT {
                "%1\tmain\tcodex\tagent\tclaude\tteam-a\t\n%2\tshell\tzsh\tterminal\tterm-1\tteam-a\t\n"
            } else {
                ""
            };
            Ok(ok_run(0, stdout, ""))
        });

        let titled = list_panes_with_titles("dev:0");
        let full = list_panes_full("dev:0");

        assert_eq!(
            titled,
            vec![
                PaneInfo {
                    pane_id: "%1".to_string(),
                    title: "main".to_string(),
                    ..Default::default()
                },
                PaneInfo {
                    pane_id: "%2".to_string(),
                    title: "worker".to_string(),
                    ..Default::default()
                },
            ]
        );
        assert_eq!(
            full[0],
            PaneInfo {
                pane_id: "%1".to_string(),
                title: "main".to_string(),
                command: "codex".to_string(),
                role: "agent".to_string(),
                agent: "claude".to_string(),
                team: "team-a".to_string(),
                ..Default::default()
            }
        );
        assert_eq!(full[1].role, "terminal");
    }

    #[test]
    fn test_pane_option_helpers_and_tagging() {
        let calls: Calls = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&calls);
        set_run_override(move |args, check, timeout| {
            recorded.borrow_mut().push((args.to_vec(), check, timeout));
            let stdout = if args[0] == "show-options" {
                "value\n"
            } else {
                ""
            };
            Ok(ok_run(0, stdout, ""))
        });

        set_pane_option("%1", "hive-role", "agent");
        assert_eq!(get_pane_option("%1", "hive-role").as_deref(), Some("value"));
        clear_pane_option("%1", "hive-role");
        tag_pane("%1", "agent", "claude", "team-a", "", "");
        clear_pane_tags("%1");

        let calls = calls.borrow();
        let argvs: Vec<Vec<String>> = calls.iter().map(|c| c.0.clone()).collect();
        assert_eq!(
            argvs[0],
            v(&["set-option", "-p", "-t", "%1", "@hive-role", "agent"])
        );
        assert_eq!(
            argvs[1],
            v(&["show-options", "-p", "-v", "-t", "%1", "@hive-role"])
        );
        assert_eq!(
            argvs[2],
            v(&["set-option", "-p", "-t", "%1", "-u", "@hive-role"])
        );
        assert!(argvs.contains(&v(&[
            "set-option",
            "-p",
            "-t",
            "%1",
            "@hive-agent",
            "claude"
        ])));
        assert!(argvs.contains(&v(&["set-option", "-p", "-t", "%1", "-u", "@hive-team"])));
        // `@hive-view` is derived from the claude view probe: release clears it
        // with the identity tags, or a reused pane keeps a dead border suffix.
        assert!(argvs.contains(&v(&["set-option", "-p", "-t", "%1", "-u", "@hive-view"])));
    }

    #[test]
    fn test_tagging_a_pane_onto_another_cli_drops_the_claude_view() {
        // Only the claude view tick maintains @hive-view, and it skips non-claude
        // panes — an in-place retag must clear it or the suffix is stale forever.
        let calls: Calls = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&calls);
        set_run_override(move |args, check, timeout| {
            recorded.borrow_mut().push((args.to_vec(), check, timeout));
            Ok(ok_run(0, "", ""))
        });

        tag_pane("%1", "agent", "blue", "team-a", "codex", "");
        let unset_view = v(&["set-option", "-p", "-t", "%1", "-u", "@hive-view"]);
        assert!(calls.borrow().iter().any(|c| c.0 == unset_view));

        calls.borrow_mut().clear();
        tag_pane("%1", "agent", "red", "team-a", "claude", "");
        assert!(!calls.borrow().iter().any(|c| c.0 == unset_view));
    }

    #[test]
    fn test_enable_pane_border_status_uses_hive_member_format() {
        let calls = _capture_run(0, "");

        enable_pane_border_status("dev:1");

        let calls = calls.borrow();
        assert_eq!(
            calls[0].0,
            v(&[
                "set-window-option",
                "-t",
                "dev:1",
                "pane-border-status",
                "top"
            ])
        );
        assert_eq!(
            calls[1].0,
            v(&[
                "set-window-option",
                "-t",
                "dev:1",
                "pane-border-format",
                _HIVE_PANE_BORDER_FORMAT,
            ])
        );
        assert!(!_HIVE_PANE_BORDER_FORMAT.contains("#[fg=colour220,bold]"));
        assert!(_HIVE_PANE_BORDER_FORMAT.contains("#[fg=colour220]#[bold][!]"));
    }

    #[test]
    fn test_configure_hive_window_disables_native_tmux_alerts() {
        let calls = _capture_run(0, "");

        configure_hive_window("dev:1");

        let argvs: Vec<Vec<String>> = calls.borrow().iter().map(|c| c.0.clone()).collect();
        assert_eq!(
            argvs,
            vec![
                v(&[
                    "set-window-option",
                    "-t",
                    "dev:1",
                    "pane-border-status",
                    "top"
                ]),
                v(&[
                    "set-window-option",
                    "-t",
                    "dev:1",
                    "pane-border-format",
                    _HIVE_PANE_BORDER_FORMAT,
                ]),
                v(&[
                    "set-window-option",
                    "-t",
                    "dev:1",
                    "monitor-activity",
                    "off"
                ]),
                v(&["set-window-option", "-t", "dev:1", "monitor-bell", "off"]),
            ]
        );
    }

    #[test]
    fn test_parse_control_mode_output_pane_matches_output_notifications() {
        assert_eq!(
            parse_control_mode_output_pane("%output %2772 hello").as_deref(),
            Some("%2772")
        );
        assert_eq!(
            parse_control_mode_output_pane("%extended-output %2773 12 : world").as_deref(),
            Some("%2773")
        );
        assert_eq!(
            parse_control_mode_output_pane("%session-changed $1 dev"),
            None
        );
    }

    #[test]
    fn test_control_mode_monitor_is_busy_uses_threshold() {
        let monitor = ControlModeOutputMonitor::new("613");
        monitor.inner.last_output_at.lock().unwrap().insert(
            "%9".to_string(),
            Instant::now() - Duration::from_secs_f64(1.0),
        );
        assert!(monitor.is_busy("%9", 3.0));
        monitor.inner.last_output_at.lock().unwrap().insert(
            "%9".to_string(),
            Instant::now() - Duration::from_secs_f64(4.0),
        );
        assert!(!monitor.is_busy("%9", 3.0));
    }

    #[test]
    fn test_control_mode_payload_activity_ignores_pure_repaint_sequence() {
        let repaint = concat!(
            "\x1b[?2026h",
            "\x1b[49;2H\x1b[0m\x1b[49m\x1b[K",
            "\x1b[50;2H\x1b[0m\x1b[48;2;244;244;244m\x1b[K",
            "\x1b[51;28H\x1b[0m\x1b[48;2;244;244;244m\x1b[K",
            "\x1b[52;2H\x1b[0m\x1b[48;2;244;244;244m\x1b[K",
            "\x1b[53;52H\x1b[0m\x1b[49m\x1b[K",
            "\x1b[39m\x1b[49m\x1b[0m\x1b[?25h\x1b[51;3H\x1b[?2026l"
        );

        assert!(!_control_mode_payload_has_activity(repaint));
    }

    #[test]
    fn test_control_mode_payload_activity_accepts_visible_text_inside_styles() {
        assert!(_control_mode_payload_has_activity("\x1b[2mhello\x1b[0m"));
    }

    #[test]
    fn test_control_mode_payload_activity_keeps_text_between_st_terminated_osc_sequences() {
        let payload = "\x1b]0;a\x1b\\hello\x1b]0;b\x1b\\";

        assert!(_control_mode_payload_has_activity(payload));
    }

    #[test]
    fn test_control_mode_payload_activity_ignores_pure_dcs_sequence() {
        assert!(!_control_mode_payload_has_activity(
            "\x1bP1;2;3payload\x1b\\"
        ));
    }

    #[test]
    fn test_control_mode_payload_activity_accepts_visible_text_between_dcs_and_osc() {
        let payload = "\x1bPignored\x1b\\hello\x1b]0;title\x1b\\";

        assert!(_control_mode_payload_has_activity(payload));
    }

    #[test]
    fn test_control_mode_monitor_ignores_repaint_only_output() {
        // Repaint-only control sequences never mark a pane busy; the monitor
        // keeps no payload buffer (the pane-content msgId oracle is gone — delivery
        // confirmation is transcript-only).
        let monitor = ControlModeOutputMonitor::new("613");
        let payload = "\x1b[?2026h\x1b[49;2H\x1b[K\x1b[?2026l";

        monitor._record_control_mode_output("%9", payload);

        assert!(!monitor.is_busy("%9", 3.0));
    }

    #[test]
    fn test_control_mode_monitor_marks_visible_text_busy() {
        let monitor = ControlModeOutputMonitor::new("613");

        monitor._record_control_mode_output("%9", "\x1b[2mhello\x1b[0m");

        assert!(monitor.is_busy("%9", 3.0));
    }

    #[test]
    fn test_window_option_helpers_and_flash() {
        let calls = _capture_run(0, "");

        set_window_option("dev:1", "window-status-style", "fg=red");
        clear_window_option("dev:1", "window-status-style");
        flash_window_status("dev:1", "fg=colour235,bg=colour220,bold", 3);

        let calls = calls.borrow();
        assert_eq!(
            calls[0].0,
            v(&[
                "set-window-option",
                "-t",
                "dev:1",
                "window-status-style",
                "fg=red"
            ])
        );
        assert_eq!(
            calls[1].0,
            v(&[
                "set-window-option",
                "-t",
                "dev:1",
                "-u",
                "window-status-style"
            ])
        );
        assert_eq!(calls[2].0[0..2], v(&["run-shell", "-b"]));
        assert!(calls[2].0[2].contains("window-status-style"));
        assert!(calls[2].0[2].contains("dev:1"));
        assert_eq!(calls[2].0[2].matches("sleep 0.5").count(), 6);
    }

    #[test]
    fn test_get_global_window_option_is_read_only_global_scope() {
        let calls = _capture_run(0, "  #I #W  \n");

        let value = get_global_window_option("window-status-format");

        // Read-only `show-options -w -g -v`, no `-t` target — global scope only.
        let calls = calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            (calls[0].0.clone(), calls[0].1),
            (
                v(&["show-options", "-w", "-g", "-v", "window-status-format"]),
                false
            )
        );
        // Meaningful leading/trailing padding survives; only the newline is stripped.
        assert_eq!(value.as_deref(), Some("  #I #W  "));
    }

    #[test]
    fn test_get_global_window_option_returns_none_when_unset() {
        set_run_override(|_args, _check, _timeout| Ok(ok_run(0, "\n", "")));
        assert_eq!(get_global_window_option("window-status-format"), None);
    }

    #[test]
    fn test_list_panes_full_or_none_is_status_aware() {
        set_run_override(|_args, _check, _timeout| {
            Ok(ok_run(
                0,
                "%1\t[w]\tzsh\tagent\tworker\tt1\tclaude\tduo\t\n",
                "",
            ))
        });
        let panes = list_panes_full_or_none("dev:0");
        assert!(panes.is_some());
        assert_eq!(panes.unwrap()[0].pane_id, "%1");
        assert_eq!(list_panes_full("dev:0")[0].pane_id, "%1");

        set_run_override(|_args, _check, _timeout| Ok(ok_run(1, "", "timeout")));
        assert_eq!(list_panes_full_or_none("dev:0"), None);
        assert_eq!(list_panes_full("dev:0"), Vec::new());

        set_run_override(|_args, _check, _timeout| Ok(ok_run(0, "", "")));
        assert_eq!(list_panes_full_or_none("dev:0"), Some(Vec::new()));
    }

    #[test]
    fn test_pane_scan_status_maps_no_server_variants() {
        for stderr in [
            "no server running on /tmp/tmux-501/default",
            "error connecting to /x/tmux-501/default (No such file or directory)",
        ] {
            set_run_override(move |_args, _check, _timeout| Ok(ok_run(1, "", stderr)));
            assert_eq!(list_panes_all_status(), (None, "no-server"));
            assert_eq!(list_team_windows_status(), (None, "no-server"));
        }

        set_run_override(|_args, _check, _timeout| Ok(ok_run(1, "", "timeout")));
        assert_eq!(list_panes_all_status(), (None, "unknown"));
        assert_eq!(list_team_windows_status(), (None, "unknown"));
    }

    #[test]
    fn test_pane_scan_status_keeps_permission_denied_unknown() {
        set_run_override(|_args, _check, _timeout| {
            Ok(ok_run(
                1,
                "",
                "error connecting to /private/tmp/tmux-501/default (Permission denied)",
            ))
        });
        assert_eq!(list_panes_all_status(), (None, "unknown"));
        assert_eq!(list_team_windows_status(), (None, "unknown"));
    }

    #[test]
    fn test_team_window_scan_parses_pr_and_tolerates_short_lines() {
        set_run_override(|_args, _check, _timeout| {
            Ok(ok_run(
                0,
                // second line is an old 6-field line: pr backfills ""
                "dev:1\thive\t@1\t0-w2\t/tmp/ws\t100.0\t52\ndev:2\tother\t@2\t0-w9\t/tmp/w9\t50.0\n",
                "",
            ))
        });

        let (windows, status) = list_team_windows_status();
        assert_eq!(status, "ok");
        let windows = windows.unwrap();
        assert_eq!(windows[0].pr, "52");
        assert_eq!(windows[1].pr, "");
        assert_eq!(windows[1].team, "0-w9");
    }

    // --- facade-hygiene helpers (exact command contracts) ---------------------

    #[test]
    fn test_window_exists_requires_exact_id_echo() {
        let calls = _capture_run(0, "@7\n");
        assert!(window_exists("@7"));
        let calls = calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            (
                v(&["display-message", "-t", "@7", "-p", "#{window_id}"]),
                false,
                5
            )
        );
    }

    #[test]
    fn test_window_exists_false_paths() {
        let calls = _capture_run(0, "@8\n");
        assert!(!window_exists("")); // no subprocess for empty id
        assert!(calls.borrow().is_empty());
        assert!(!window_exists("@7")); // mismatched id
        _capture_run(1, "@7\n");
        assert!(!window_exists("@7")); // nonzero exit
        _raising_run();
        assert!(!window_exists("@7")); // missing binary never raises
    }

    #[test]
    fn test_display_popup_preserves_argv_order_and_never_raises() {
        let calls = _capture_run(0, "");
        display_popup(
            "%5",
            "run-me",
            "/dev/ttys001",
            "#{popup_pane_left}",
            "#{popup_pane_top}",
            "40",
            "20",
            true,
            true,
            5,
        );
        assert_eq!(
            *calls.borrow(),
            vec![(
                v(&[
                    "display-popup",
                    "-c",
                    "/dev/ttys001",
                    "-t",
                    "%5",
                    "-B",
                    "-x",
                    "#{popup_pane_left}",
                    "-y",
                    "#{popup_pane_top}",
                    "-w",
                    "40",
                    "-h",
                    "20",
                    "-E",
                    "run-me",
                ]),
                false,
                5
            )]
        );
        _raising_run();
        display_popup("%5", "run-me", "", "", "", "", "", false, false, 5); // non-raising
    }

    #[test]
    fn test_display_popup_omits_optional_flags() {
        let calls = _capture_run(0, "");
        display_popup("%5", "run-me", "", "", "", "", "", false, false, 5);
        assert_eq!(
            *calls.borrow(),
            vec![(v(&["display-popup", "-t", "%5", "run-me"]), false, 5)]
        );
    }

    #[test]
    fn test_run_shell_detached_passes_command_byte_for_byte() {
        let calls = _capture_run(0, "");
        let cmd = "sleep 0.2 && tmux send-keys -t '%9' Escape";
        run_shell_detached(cmd);
        assert_eq!(
            *calls.borrow(),
            vec![(v(&["run-shell", "-b", cmd]), false, 5)]
        );
    }

    #[test]
    fn test_source_file_bool_contract() {
        let calls = _capture_run(0, "");
        assert!(source_file("/x/enable.conf"));
        assert_eq!(
            *calls.borrow(),
            vec![(v(&["source-file", "/x/enable.conf"]), false, 5)]
        );
        _capture_run(1, "");
        assert!(!source_file("/x/enable.conf"));
        _raising_run();
        assert!(!source_file("/x/enable.conf"));
    }

    #[test]
    fn test_display_value_none_on_failure() {
        _capture_run(1, "");
        assert_eq!(display_value("%5", "#{pane_left}"), None);
    }
}
