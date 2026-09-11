//! tmux control-mode output parsing and the pane-activity monitor.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::appearance::{session_colour_snapshot, PaneColourReports};

const CONTROL_MODE_RESTART_DELAY: f64 = 1.0;
/// Restart delays double from `CONTROL_MODE_RESTART_DELAY` up to this cap
/// while attaches keep failing fast (no server to attach to), and reset
/// after a run that stayed attached `CONTROL_MODE_HEALTHY_RUN_SECONDS`.
const CONTROL_MODE_MAX_RESTART_DELAY: f64 = 30.0;
const CONTROL_MODE_HEALTHY_RUN_SECONDS: f64 = 10.0;
/// `stop` joins the monitor thread, so a backoff sleeps in slices this long.
const STOP_POLL: Duration = Duration::from_millis(200);
const COLOUR_SAMPLE_FAST_INTERVAL: Duration = Duration::from_secs(2);
const COLOUR_SAMPLE_IDLE_INTERVAL: Duration = Duration::from_secs(60);

struct ColourSampling {
    last_sample: Instant,
    last_client_event: Option<Instant>,
}

impl ColourSampling {
    fn sample_if_due(
        &mut self,
        now: Instant,
        has_unknown_client: bool,
        force: bool,
        sample: impl FnOnce() -> bool,
    ) -> Option<bool> {
        let recent_client_event = self
            .last_client_event
            .is_some_and(|event| now.duration_since(event) < Duration::from_secs(30));
        let interval = if has_unknown_client || recent_client_event {
            COLOUR_SAMPLE_FAST_INTERVAL
        } else {
            COLOUR_SAMPLE_IDLE_INTERVAL
        };
        if !force && now.duration_since(self.last_sample) < interval {
            return None;
        }
        self.last_sample = now;
        Some(sample())
    }
}

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

/// The delay before the next attach: `CONTROL_MODE_RESTART_DELAY` after a
/// run that stayed attached (or for the first retry), otherwise double the
/// previous delay up to `CONTROL_MODE_MAX_RESTART_DELAY`.
fn next_restart_delay(previous: Option<f64>, ran_for_secs: f64) -> f64 {
    match previous {
        Some(previous) if ran_for_secs < CONTROL_MODE_HEALTHY_RUN_SECONDS => {
            (previous * 2.0).min(CONTROL_MODE_MAX_RESTART_DELAY)
        }
        _ => CONTROL_MODE_RESTART_DELAY,
    }
}

/// One control client this workspace's hived spawned: its pid, its birth
/// as `ps lstart` reports it (whitespace-normalized) and the session it
/// attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ControlClientRow {
    pub pid: i32,
    pub started: String,
    pub session: String,
}

/// `<run dir>/control-clients.json`: the control clients this workspace's
/// hived spawned, for the reaper of the next one. A hived killed with
/// SIGKILL leaves its `tmux -C attach` child reparented to pid 1, attached
/// forever; the next hived reaps it — but only a process the ledger names,
/// whose birth time and argv are still the ledger's, and whose parent is
/// pid 1. A pid alone is not identity (pids are reused) and a matching
/// argv alone is not ownership (a human's control client on a same-named
/// session of another server looks the same): what the ledger does not
/// vouch for is reported, never killed.
fn control_client_ledger_path(workspace: &str) -> PathBuf {
    crate::hived::run_dir_impl(workspace).join("control-clients.json")
}

fn read_control_client_ledger(path: &Path) -> Vec<ControlClientRow> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(serde_json::Value::Array(rows)) = serde_json::from_str::<serde_json::Value>(&text)
    else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let row = row.as_object()?;
            Some(ControlClientRow {
                pid: i32::try_from(row.get("pid")?.as_i64()?).ok()?,
                started: row.get("started")?.as_str()?.to_string(),
                session: row.get("session")?.as_str()?.to_string(),
            })
        })
        .collect()
}

/// Write the ledger by atomic rename; an empty ledger removes the file.
fn write_control_client_ledger(path: &Path, rows: &[ControlClientRow]) {
    if rows.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let doc: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            serde_json::json!({"pid": row.pid, "started": row.started, "session": row.session})
        })
        .collect();
    let Ok((mut file, tmp)) = crate::paths::mkstemp_in(parent, ".control-clients.", ".tmp") else {
        return;
    };
    let mut text = serde_json::to_string_pretty(&serde_json::Value::Array(doc)).unwrap_or_default();
    text.push('\n');
    if file
        .write_all(text.as_bytes())
        .and_then(|_| std::fs::rename(&tmp, path))
        .is_err()
    {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// A process's birth as `ps` reports it (`lstart`), whitespace-normalized;
/// None when the process is gone already.
pub(crate) fn process_birth(pid: i32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let started = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!started.is_empty()).then_some(started)
}

fn record_control_client(workspace: &str, session_target: &str, pid: i32) {
    let Some(started) = process_birth(pid) else {
        return;
    };
    let path = control_client_ledger_path(workspace);
    let mut rows = read_control_client_ledger(&path);
    rows.retain(|row| row.pid != pid);
    rows.push(ControlClientRow {
        pid,
        started,
        session: session_target.to_string(),
    });
    write_control_client_ledger(&path, &rows);
}

fn forget_control_client(workspace: &str, pid: i32) {
    let path = control_client_ledger_path(workspace);
    let rows = read_control_client_ledger(&path);
    if rows.iter().any(|row| row.pid == pid) {
        let kept: Vec<ControlClientRow> = rows.into_iter().filter(|row| row.pid != pid).collect();
        write_control_client_ledger(&path, &kept);
    }
}

/// One `ps -axo pid=,ppid=,lstart=,command=` line: pid, ppid, the
/// five-token birth, the argv.
fn parse_ps_line(line: &str) -> Option<(i32, i32, String, String)> {
    let mut parts = line.split_whitespace();
    let pid: i32 = parts.next()?.parse().ok()?;
    let ppid: i32 = parts.next()?.parse().ok()?;
    let started: Vec<&str> = parts.by_ref().take(5).collect();
    if started.len() < 5 {
        return None;
    }
    let command = parts.collect::<Vec<_>>().join(" ");
    Some((pid, ppid, started.join(" "), command))
}

/// What the reaper does with the ledger against a listing.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct OrphanVerdict {
    /// Ledger rows still that very process, reparented to pid 1: reaped.
    pub reap: Vec<i32>,
    /// Ledger rows still that process under a living parent (an older
    /// hived generation's client): kept.
    pub keep: Vec<ControlClientRow>,
    /// Reparented control clients of the session the ledger does not
    /// vouch for: reported only.
    pub unowned: Vec<i32>,
}

pub(crate) fn orphan_control_clients(
    ledger: &[ControlClientRow],
    ps_output: &str,
    session_target: &str,
) -> OrphanVerdict {
    let wanted = format!("tmux -C attach -t {session_target}");
    let rows: Vec<(i32, i32, String, String)> =
        ps_output.lines().filter_map(parse_ps_line).collect();
    let mut verdict = OrphanVerdict::default();
    for entry in ledger {
        let argv = format!("tmux -C attach -t {}", entry.session);
        let Some((_, ppid, _, _)) = rows.iter().find(|(pid, _, started, command)| {
            *pid == entry.pid && *started == entry.started && *command == argv
        }) else {
            continue; // gone, or another process wearing the pid: dropped
        };
        if *ppid == 1 {
            verdict.reap.push(entry.pid);
        } else {
            verdict.keep.push(entry.clone());
        }
    }
    for (pid, ppid, _, command) in &rows {
        if *ppid == 1 && *command == wanted && !verdict.reap.contains(pid) {
            verdict.unowned.push(*pid);
        }
    }
    verdict
}

fn reap_orphan_control_clients(session_target: &str, workspace: &str) {
    let path = control_client_ledger_path(workspace);
    let ledger = read_control_client_ledger(&path);
    let Ok(out) = Command::new("ps")
        .args(["-axo", "pid=,ppid=,lstart=,command="])
        .output()
    else {
        return;
    };
    let verdict = orphan_control_clients(
        &ledger,
        &String::from_utf8_lossy(&out.stdout),
        session_target,
    );
    for pid in &verdict.reap {
        unsafe {
            libc::kill(*pid, libc::SIGTERM);
        }
        crate::notify_debug::emit(
            workspace,
            "monitor.orphan_reaped",
            &[
                ("session", serde_json::json!(session_target)),
                ("pid", serde_json::json!(pid)),
            ],
        );
    }
    for pid in &verdict.unowned {
        crate::notify_debug::emit(
            workspace,
            "monitor.orphan_unowned",
            &[
                ("session", serde_json::json!(session_target)),
                ("pid", serde_json::json!(pid)),
            ],
        );
    }
    if verdict.keep != ledger {
        write_control_client_ledger(&path, &verdict.keep);
    }
}

fn monitor_run_loop(inner: Arc<MonitorInner>, session_target: String) {
    reap_orphan_control_clients(&session_target, &inner.workspace);
    let mut delay: Option<f64> = None;
    while !inner.stop.load(Ordering::SeqCst) {
        let started = Instant::now();
        // Best-effort monitor: fall back to retry rather than crashing hived.
        let _ = monitor_run_once(&inner, &session_target);
        if inner.stop.load(Ordering::SeqCst) {
            break;
        }
        let next = next_restart_delay(delay, started.elapsed().as_secs_f64());
        delay = Some(next);
        let until = Instant::now() + Duration::from_secs_f64(next);
        while !inner.stop.load(Ordering::SeqCst) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            thread::sleep(left.min(STOP_POLL));
        }
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
    reports.clients = snapshot.clients;
    reports.has_unknown_client = snapshot.has_unknown_client;
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

fn colour_client_changed(line: &str, session_target: &str, clients: &[String]) -> bool {
    if let Some(detached) = line.strip_prefix("%client-detached ") {
        return clients.iter().any(|client| client == detached);
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
    // A known client leaving matters even when it was not the selected source.
    clients.iter().any(|client| client == name)
        || target == session_target
        || session_target.strip_prefix('$') == Some(id)
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
    record_control_client(&inner.workspace, session_target, child.id() as i32);
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
    let mut colour_sampling = ColourSampling {
        last_sample: Instant::now(),
        last_client_event: None,
    };

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
            } else if colour_client_changed(decoded, session_target, &colour_reports.clients) {
                refresh_clients = true;
                colour_sampling.last_client_event = Some(Instant::now());
            }
        }
        if supports_colours {
            let sampled = colour_sampling.sample_if_due(
                Instant::now(),
                colour_reports.has_unknown_client,
                refresh_panes || refresh_clients,
                || {
                    report_session_pane_colours(
                        master,
                        session_target,
                        &inner.workspace,
                        &mut colour_reports,
                        panes_dirty,
                    )
                },
            );
            if sampled == Some(true) {
                panes_dirty = false;
            }
        }
    }
    terminate_child(&mut child);
    forget_control_client(&inner.workspace, child.id() as i32);
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
    fn test_colour_events_filter_other_sessions_and_include_known_client_departure() {
        assert!(colour_client_changed(
            "%client-session-changed human $1 team",
            "team",
            &[]
        ));
        assert!(colour_client_changed(
            "%client-session-changed human $1 team",
            "$1",
            &[]
        ));
        assert!(!colour_client_changed(
            "%client-session-changed other $2 elsewhere",
            "team",
            &["human".into()]
        ));
        assert!(colour_client_changed(
            "%client-session-changed human $2 elsewhere",
            "team",
            &["human".into()]
        ));
        assert!(colour_client_changed(
            "%client-detached human",
            "team",
            &["human".into()]
        ));
        assert!(!colour_client_changed(
            "%client-detached other",
            "team",
            &["human".into()]
        ));
        assert!(!colour_client_changed(
            "%output %1 client-session-changed",
            "team",
            &[]
        ));
        assert!(!colour_client_changed(
            "%client-session-changed malformed",
            "team",
            &[]
        ));
    }

    #[test]
    fn test_known_clients_do_not_spawn_queries_between_idle_samples() {
        let mut env =
            crate::testenv::EnvGuard::cleared(&["HIVE_VIEW_THEME", "HIVE_APPEARANCE", "COLORFGBG"]);
        let temp = tempfile::tempdir().unwrap();
        env.set("HOME", temp.path());
        env.set("HIVE_HOME", temp.path().join("home"));
        let calls = Rc::new(RefCell::new(0));
        let seen = Rc::clone(&calls);
        set_run_override(move |_, _, _| {
            *seen.borrow_mut() += 1;
            Ok(ok_run(0, "C\t1\t\tcontrol\nC\t0\tdark\thuman\nP\t%1", ""))
        });
        let output = std::fs::File::create(temp.path().join("commands")).unwrap();
        let mut reports = PaneColourReports::default();
        let sample = |reports: &mut PaneColourReports| {
            report_session_pane_colours(
                output.as_raw_fd(),
                "team",
                temp.path().to_str().unwrap(),
                reports,
                true,
            )
        };
        assert!(sample(&mut reports));
        assert!(!reports.has_unknown_client);
        let start = Instant::now();
        let mut sampling = ColourSampling {
            last_sample: start,
            last_client_event: None,
        };
        for second in [2, 4, 30, 59] {
            assert_eq!(
                sampling.sample_if_due(
                    start + Duration::from_secs(second),
                    reports.has_unknown_client,
                    false,
                    || sample(&mut reports),
                ),
                None
            );
        }
        assert_eq!(*calls.borrow(), 1);
        assert_eq!(
            sampling.sample_if_due(
                start + Duration::from_secs(60),
                reports.has_unknown_client,
                false,
                || sample(&mut reports),
            ),
            Some(true)
        );
        assert_eq!(*calls.borrow(), 2);
        assert_eq!(
            sampling.sample_if_due(
                start + Duration::from_secs(62),
                reports.has_unknown_client,
                false,
                || sample(&mut reports),
            ),
            None
        );
        assert_eq!(*calls.borrow(), 2);
    }

    #[test]
    fn test_unknown_clients_and_recent_events_keep_fast_sampling() {
        let start = Instant::now();
        let mut sampling = ColourSampling {
            last_sample: start,
            last_client_event: None,
        };
        assert_eq!(
            sampling.sample_if_due(start + Duration::from_secs(1), true, false, || true),
            None
        );
        assert_eq!(
            sampling.sample_if_due(start + Duration::from_secs(2), true, false, || true),
            Some(true)
        );
        // The theme is now known, but a fresh client event keeps the fast window.
        sampling.last_client_event = Some(start + Duration::from_secs(5));
        assert_eq!(
            sampling.sample_if_due(start + Duration::from_secs(5), false, true, || true),
            Some(true)
        );
        assert_eq!(
            sampling.sample_if_due(start + Duration::from_secs(7), false, false, || true),
            Some(true)
        );
        // At 30 seconds after the event, the interval becomes 60 seconds.
        assert_eq!(
            sampling.sample_if_due(start + Duration::from_secs(35), false, false, || true),
            None
        );
        assert_eq!(
            sampling.sample_if_due(start + Duration::from_secs(66), false, false, || true),
            None
        );
        assert_eq!(
            sampling.sample_if_due(start + Duration::from_secs(67), false, false, || true),
            Some(true)
        );
    }

    #[test]
    fn test_any_unknown_human_client_keeps_fast_sampling_even_if_not_selected() {
        let mut env =
            crate::testenv::EnvGuard::cleared(&["HIVE_VIEW_THEME", "HIVE_APPEARANCE", "COLORFGBG"]);
        let temp = tempfile::tempdir().unwrap();
        env.set("HOME", temp.path());
        env.set("HIVE_HOME", temp.path().join("home"));
        set_run_override(|_, _, _| {
            Ok(ok_run(
                0,
                "C\t1\t\tcontrol\nC\t0\tdark\tselected\nC\t0\t\tpending",
                "",
            ))
        });
        let snapshot = session_colour_snapshot("team", false).unwrap();
        assert_eq!(snapshot.selected.client.as_deref(), Some("selected"));
        assert!(snapshot.has_unknown_client);
        assert!(colour_client_changed(
            "%client-detached pending",
            "team",
            &snapshot.clients
        ));
        assert!(!colour_client_changed(
            "%client-detached elsewhere",
            "team",
            &snapshot.clients
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

#[cfg(test)]
mod restart_tests {
    use super::*;

    #[test]
    fn test_next_restart_delay_doubles_to_the_cap_and_resets_after_a_healthy_run() {
        assert_eq!(next_restart_delay(None, 0.0), CONTROL_MODE_RESTART_DELAY);
        assert_eq!(next_restart_delay(Some(1.0), 0.2), 2.0);
        assert_eq!(next_restart_delay(Some(2.0), 0.2), 4.0);
        assert_eq!(
            next_restart_delay(Some(16.0), 0.2),
            CONTROL_MODE_MAX_RESTART_DELAY
        );
        assert_eq!(
            next_restart_delay(Some(CONTROL_MODE_MAX_RESTART_DELAY), 0.2),
            CONTROL_MODE_MAX_RESTART_DELAY
        );
        assert_eq!(
            next_restart_delay(
                Some(CONTROL_MODE_MAX_RESTART_DELAY),
                CONTROL_MODE_HEALTHY_RUN_SECONDS
            ),
            CONTROL_MODE_RESTART_DELAY
        );
    }

    #[test]
    fn test_orphan_control_clients_reaps_only_ledger_rows_still_born_then_and_reparented() {
        let ps = "  352     1 Mon Aug  3 12:20:44 2026 tmux -C attach -t osct\n\
                  13506     1 Mon Aug  3 12:21:00 2026 tmux -C attach -t hornet\n\
                   3390 53702 Mon Aug  3 12:22:00 2026 tmux -C attach -t hornet\n\
                   4000     1 Mon Aug  3 12:23:00 2026 tmux -C attach -t hornet -f x\n\
                   4002     1 Mon Aug  3 12:24:00 2026 /opt/homebrew/bin/tmux -C attach -t hornet\n\
                    600     1 Mon Aug  3 12:26:00 2026 tmux -C attach -t hornet\n\
                   7777     1 Mon Aug  3 12:25:00 2026 python3 server.py\n\
                   garbage line\n";
        let row = |pid: i32, started: &str, session: &str| ControlClientRow {
            pid,
            started: started.to_string(),
            session: session.to_string(),
        };
        let ledger = vec![
            // reparented, still the process born then: reaped
            row(13506, "Mon Aug 3 12:21:00 2026", "hornet"),
            // an older hived is still its parent: kept
            row(3390, "Mon Aug 3 12:22:00 2026", "hornet"),
            // the pid now wears another process: dropped
            row(7777, "Mon Aug 3 12:00:00 2026", "hornet"),
            // gone: dropped
            row(9999, "Mon Aug 3 11:00:00 2026", "hornet"),
            // not the argv the monitor spawns: dropped
            row(4002, "Mon Aug 3 12:24:00 2026", "hornet"),
        ];

        let verdict = orphan_control_clients(&ledger, ps, "hornet");

        assert_eq!(verdict.reap, vec![13506]);
        assert_eq!(
            verdict.keep,
            vec![row(3390, "Mon Aug 3 12:22:00 2026", "hornet")]
        );
        // a reparented client the ledger never saw is reported, not killed
        assert_eq!(verdict.unowned, vec![600]);
        // ownership is the ledger's, whatever session this hived attaches;
        // the unowned candidates are that session's
        let other = orphan_control_clients(&ledger, ps, "osct");
        assert_eq!(other.reap, vec![13506]);
        assert_eq!(other.unowned, vec![352]);
        assert!(orphan_control_clients(&[], ps, "lane").reap.is_empty());
        assert!(orphan_control_clients(&[], ps, "lane").unowned.is_empty());
    }

    #[test]
    fn test_process_birth_is_what_the_listing_says_of_this_process() {
        let pid = std::process::id() as i32;
        let birth = process_birth(pid).expect("this process has a birth");
        let out = Command::new("ps")
            .args(["-axo", "pid=,ppid=,lstart=,command="])
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&out.stdout);
        let row = listing
            .lines()
            .filter_map(parse_ps_line)
            .find(|(p, ..)| *p == pid)
            .expect("this process is listed");
        assert_eq!(row.2, birth);
        assert!(process_birth(i32::MAX - 7).is_none());
    }

    #[test]
    fn test_control_client_ledger_round_trips_and_an_empty_one_removes_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run").join("control-clients.json");
        let rows = vec![ControlClientRow {
            pid: 42,
            started: "Mon Aug 3 12:21:00 2026".to_string(),
            session: "hornet".to_string(),
        }];
        write_control_client_ledger(&path, &rows);
        assert_eq!(read_control_client_ledger(&path), rows);
        write_control_client_ledger(&path, &[]);
        assert!(!path.exists());
        assert!(read_control_client_ledger(&path).is_empty());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json").unwrap();
        assert!(read_control_client_ledger(&path).is_empty());
    }
}
