//! tmux control-mode output parsing and the pane-activity monitor.

use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::appearance::{session_colour_snapshot, PaneColourReports};

const CONTROL_MODE_RESTART_DELAY: f64 = 1.0;
const COLOUR_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

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
    workspace: String,
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
    pub fn new(session_target: &str, workspace: &str) -> Self {
        ControlModeOutputMonitor {
            session_target: session_target.to_string(),
            inner: Arc::new(MonitorInner {
                workspace: workspace.to_string(),
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
            // ponytail: unbounded join; the loop polls every 0.5s. A colour
            // snapshot may add its 1s query timeout; startup also checks the
            // tmux version before entering the loop.
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
        thread::sleep(Duration::from_secs_f64(CONTROL_MODE_RESTART_DELAY));
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

/// Return whether the snapshot was read; failed pane enumeration is retried
/// at the next sample. Reports travel over the existing control connection.
fn report_session_pane_colours(
    master: RawFd,
    session_target: &str,
    workspace: &str,
    reports: &mut PaneColourReports,
    refresh_panes: bool,
) -> bool {
    let Some(snapshot) = session_colour_snapshot(session_target, refresh_panes) else {
        return false;
    };
    if let Some(panes) = &snapshot.panes {
        reports.set_panes(panes);
    }
    let selected = snapshot.selected;
    if reports.selected.as_ref() != Some(&selected) {
        crate::notify_debug::emit(
            workspace,
            "pane-colours.selected",
            &[
                ("session", serde_json::json!(session_target)),
                (
                    "appearance",
                    serde_json::json!(match selected.appearance {
                        crate::view_theme::Appearance::Light => "light",
                        crate::view_theme::Appearance::Dark => "dark",
                    }),
                ),
                ("source", serde_json::json!(selected.source)),
                ("client", serde_json::json!(selected.client)),
            ],
        );
        reports.selected = Some(selected);
    }
    // Failed writes stay pending for the next sample.
    let _ = reports.write_pending(|lines| write_control_command(master, lines.as_bytes()));
    true
}

fn write_control_command(master: RawFd, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let n = unsafe { libc::write(master, bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        bytes = &bytes[n as usize..];
    }
    Ok(())
}

fn colour_client_changed(line: &str, session_target: &str, client: Option<&str>) -> bool {
    if let Some(detached) = line.strip_prefix("%client-detached ") {
        return client == Some(detached);
    }
    let Some(changed) = line.strip_prefix("%client-session-changed ") else {
        return false;
    };
    let Some((name, session)) = changed.rsplit_once(" $") else {
        return false;
    };
    let Some((id, target)) = session.split_once(' ') else {
        return false;
    };
    // Switching the selected client away matters as much as attaching one here.
    client == Some(name) || target == session_target || session_target.strip_prefix('$') == Some(id)
}

/// The `TERM` the control client attaches with. Under tmux, codex asks
/// `tmux display-message` for the client's terminal type (`client_termname`
/// when `client_termtype` is empty), and tmux picks the client by the
/// session's most recent activity, control clients included. A control
/// client that inherits the hived's own `TERM` — `dumb` when the team was
/// created from an agent's tool shell — is then reported as the terminal,
/// and codex refuses to draw. The client presents the terminal the panes
/// are given: the server's `default-terminal`.
fn monitor_run_once(inner: &MonitorInner, session_target: &str) -> std::io::Result<()> {
    let (master, slave) = monitor_openpty()?;
    let mut cmd = Command::new("tmux");
    cmd.args(["-C", "attach", "-t", session_target]);
    cmd.env("TERM", super::default_terminal());
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
    let supports_colours = super::version().is_some_and(|v| v >= super::PANE_COLOUR_REPORT_SINCE);
    let mut colour_reports = PaneColourReports::default();
    let mut panes_dirty = true;
    if supports_colours {
        panes_dirty = !report_session_pane_colours(
            master,
            session_target,
            &inner.workspace,
            &mut colour_reports,
            true,
        );
    }
    let mut last_colour_sample = Instant::now();

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
        if ready > 0 {
            let mut chunk = [0u8; 65536];
            let nread = unsafe { libc::read(master, chunk.as_mut_ptr().cast(), chunk.len()) };
            if nread < 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..nread as usize]);
        }
        let mut refresh_panes = false;
        let mut refresh_clients = false;
        while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
            let raw_line: Vec<u8> = buffer.drain(..=pos).collect();
            // Drop invalid UTF-8 bytes.
            let decoded =
                String::from_utf8_lossy(&raw_line[..raw_line.len() - 1]).replace('\u{fffd}', "");
            let decoded = decoded.trim_end_matches('\r');
            let (pane_id, payload) = parse_control_mode_output(decoded);
            if !pane_id.is_empty() {
                record_control_mode_output(inner, &pane_id, &payload);
            } else if decoded.starts_with("%layout-change ") || decoded.starts_with("%window-add ")
            {
                refresh_panes = true;
                panes_dirty = true;
            } else if colour_client_changed(
                decoded,
                session_target,
                colour_reports
                    .selected
                    .as_ref()
                    .and_then(|s| s.client.as_deref()),
            ) {
                refresh_clients = true;
            }
        }
        if supports_colours
            && (refresh_panes
                || refresh_clients
                || last_colour_sample.elapsed() >= COLOUR_SAMPLE_INTERVAL)
        {
            let sampled = report_session_pane_colours(
                master,
                session_target,
                &inner.workspace,
                &mut colour_reports,
                panes_dirty,
            );
            if sampled {
                panes_dirty = false;
            }
            last_colour_sample = Instant::now();
        }
    }
    terminate_child(&mut child);
    *inner.master_fd.lock().unwrap() = None;
    unsafe {
        libc::close(master);
    }
    Ok(())
}

#[cfg(test)]
mod colour_tests {
    use super::*;
    use crate::tmux::run::{ok_run, set_run_override};
    use std::cell::RefCell;
    use std::os::fd::AsRawFd;
    use std::rc::Rc;

    #[test]
    fn test_colour_events_filter_other_sessions_and_include_selected_client_departure() {
        assert!(colour_client_changed(
            "%client-session-changed human $1 team",
            "team",
            None
        ));
        assert!(colour_client_changed(
            "%client-session-changed human $1 team",
            "$1",
            None
        ));
        assert!(!colour_client_changed(
            "%client-session-changed other $2 elsewhere",
            "team",
            Some("human")
        ));
        assert!(colour_client_changed(
            "%client-session-changed human $2 elsewhere",
            "team",
            Some("human")
        ));
        assert!(colour_client_changed(
            "%client-detached human",
            "team",
            Some("human")
        ));
        assert!(!colour_client_changed(
            "%client-detached other",
            "team",
            Some("human")
        ));
        assert!(!colour_client_changed(
            "%output %1 client-session-changed",
            "team",
            None
        ));
        assert!(!colour_client_changed(
            "%client-session-changed malformed",
            "team",
            None
        ));
    }

    #[test]
    fn test_colour_sampling_uses_one_process_and_preserves_state_after_query_failure() {
        let mut env =
            crate::testenv::EnvGuard::cleared(&["HIVE_VIEW_THEME", "HIVE_APPEARANCE", "COLORFGBG"]);
        let temp = tempfile::tempdir().unwrap();
        env.set("HOME", temp.path());
        env.set("HIVE_HOME", temp.path().join("home"));
        let workspace = temp.path().to_str().unwrap();
        let output = std::fs::File::create(temp.path().join("commands")).unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::clone(&calls);
        let snapshot = Rc::new(RefCell::new(ok_run(
            0,
            "C\t1\t\tcontrol\nP\t%1\nP\t%2\n",
            "",
        )));
        let response = Rc::clone(&snapshot);
        set_run_override(move |args, _, _| {
            seen.borrow_mut().push(args.to_vec());
            Ok(response.borrow().clone())
        });
        let mut reports = PaneColourReports::default();
        let fd = output.as_raw_fd();
        assert!(report_session_pane_colours(
            fd,
            "team",
            workspace,
            &mut reports,
            true
        ));
        assert_eq!(calls.borrow().len(), 1);
        assert!(calls.borrow()[0].iter().any(|a| a == "list-panes"));
        let initial = output.metadata().unwrap().len();
        assert!(initial > 0);
        *snapshot.borrow_mut() = ok_run(0, "C\t0\tdark\thuman\n", "");
        assert!(report_session_pane_colours(
            fd,
            "team",
            workspace,
            &mut reports,
            false
        ));
        assert_eq!(calls.borrow().len(), 2);
        assert_eq!(
            calls.borrow()[1],
            vec![
                "-u",
                "list-clients",
                "-t",
                "team",
                "-F",
                "C\t#{client_control_mode}\t#{client_theme}\t#{client_name}"
            ]
        );
        let changed = output.metadata().unwrap().len();
        assert!(changed > initial);
        assert!(report_session_pane_colours(
            fd,
            "team",
            workspace,
            &mut reports,
            false
        ));
        assert_eq!(output.metadata().unwrap().len(), changed);
        *snapshot.borrow_mut() = ok_run(1, "", "query failed");
        assert!(!report_session_pane_colours(
            fd,
            "team",
            workspace,
            &mut reports,
            true
        ));
        assert_eq!(
            reports.selected.as_ref().unwrap().appearance,
            crate::view_theme::Appearance::Dark
        );
        assert_eq!(output.metadata().unwrap().len(), changed);
        *snapshot.borrow_mut() = ok_run(0, "C\t0\tdark\thuman\nP\t%1\nP\t%2\nP\t%3", "");
        assert!(report_session_pane_colours(
            fd,
            "team",
            workspace,
            &mut reports,
            true
        ));
        let commands = std::fs::read_to_string(temp.path().join("commands")).unwrap();
        assert_eq!(
            commands[changed as usize..],
            super::super::pane_colour_report_lines("%3", crate::view_theme::Appearance::Dark)
                .concat()
        );
        let log = std::fs::read_to_string(crate::notify_debug::log_path(workspace)).unwrap();
        let events: Vec<serde_json::Value> = log
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["source"], "fallback");
        assert_eq!(events[1]["source"], "client");
        assert_eq!(events[1]["client"], "human");
        assert_eq!(events[1]["appearance"], "dark");
    }
}
