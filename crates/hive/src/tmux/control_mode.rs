//! tmux control-mode output parsing and the pane-activity monitor.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const _CONTROL_MODE_RESTART_DELAY: f64 = 1.0;

/// Decode tmux control-mode escape: control bytes and '\' are encoded as \NNN (3 octal digits).
fn decode_output_payload(raw: &str) -> String {
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
        decode_output_payload(remainder.trim_start()),
    )
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
pub(crate) fn control_mode_payload_has_activity(payload: &str) -> bool {
    if payload.is_empty() {
        return false;
    }
    let visible = strip_ansi_escapes(payload);
    let visible = strip_control_chars(&visible);
    !visible.trim().is_empty()
}

pub(super) struct MonitorInner {
    stop: AtomicBool,
    pub(super) last_output_at: Mutex<HashMap<String, Instant>>,
    master_fd: Mutex<Option<i32>>,
}

/// Best-effort tmux control-mode monitor for pane output activity.
pub struct ControlModeOutputMonitor {
    pub session_target: String,
    pub(super) inner: Arc<MonitorInner>,
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
        self.request_detach();
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

    #[cfg(test)]
    pub(crate) fn record_control_mode_output(&self, pane_id: &str, payload: &str) {
        record_control_mode_output(&self.inner, pane_id, payload);
    }

    fn request_detach(&self) {
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
    if !control_mode_payload_has_activity(payload) {
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
