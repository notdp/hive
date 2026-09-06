use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::engine::{pane_for_job, EngineSession};
use super::keyboard::{
    _ATTACH_EXIT_TIMEOUT, _BUF_CAP, _CLEAR_LINE, _CLIENT_READY_TIMEOUT, _CONTROL_KEY_GAP,
    _DEFAULT_PTY_COLS, _DEFAULT_PTY_ROWS, _ENGINE_READY_TIMEOUT, _RESTORE_KILL,
};
use super::lifecycle::{bg_env, wait_engine_entry_until};
use super::sleep_s;

#[cfg(test)]
use super::testhook;

// --------------------------------------------------------------------------
// the attach client
// --------------------------------------------------------------------------

struct DrainBuf {
    data: Vec<u8>,
    seen: usize,
}

/// A `claude attach` client on a pty, wearing the size already on screen.
///
/// The engine's pty follows whatever client is attached, so a client with no
/// tty drags it to a default the moment it connects and back when it leaves —
/// measured on a real engine: 180 columns, 120 while a tty-less pipe was
/// attached, 180 again after. Wearing the viewer's own size makes the
/// connection invisible instead.
pub(crate) struct RealClient {
    child: Child,
    pid: i32,
    // The attached TUI paints continuously; an undrained master fills its
    // buffer and blocks the engine's writes. The drained bytes are the
    // engine's own screen — kept (bounded) so the echo check reads them
    // in-memory instead of shelling out to `claude logs` per poll.
    fd: Arc<AtomicI32>,
    buf: Arc<Mutex<DrainBuf>>,
}

impl RealClient {
    fn new(child: Child, master_fd: i32) -> Self {
        let pid = child.id() as i32;
        let buf = Arc::new(Mutex::new(DrainBuf {
            data: Vec::new(),
            seen: 0,
        }));
        let drain_buf = Arc::clone(&buf);
        thread::spawn(move || {
            let mut chunk = [0u8; 65536];
            loop {
                let n = unsafe {
                    libc::read(
                        master_fd,
                        chunk.as_mut_ptr() as *mut libc::c_void,
                        chunk.len(),
                    )
                };
                if n <= 0 {
                    return;
                }
                let n = n as usize;
                let mut b = drain_buf.lock().unwrap_or_else(|e| e.into_inner());
                b.data.extend_from_slice(&chunk[..n]);
                b.seen += n;
                if b.data.len() > _BUF_CAP {
                    let excess = b.data.len() - _BUF_CAP;
                    b.data.drain(..excess);
                }
            }
        });
        RealClient {
            child,
            pid,
            fd: Arc::new(AtomicI32::new(master_fd)),
            buf,
        }
    }

    fn mark(&self) -> usize {
        self.buf.lock().unwrap_or_else(|e| e.into_inner()).seen
    }

    fn text_since(&self, mark: usize) -> String {
        let b = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        let kept = b.data.len();
        let start = kept.saturating_sub(b.seen.saturating_sub(mark));
        String::from_utf8_lossy(&b.data[start..]).into_owned()
    }

    fn write_str(&self, payload: &str) -> std::io::Result<()> {
        let fd = self.fd.load(Ordering::SeqCst);
        if fd < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "attach client closed",
            ));
        }
        let bytes = payload.as_bytes();
        let mut off = 0;
        while off < bytes.len() {
            let n = unsafe {
                libc::write(
                    fd,
                    bytes[off..].as_ptr() as *const libc::c_void,
                    bytes.len() - off,
                )
            };
            if n < 0 {
                return Err(std::io::Error::last_os_error());
            }
            off += n as usize;
        }
        Ok(())
    }

    fn close_stdin(&self) {
        let fd = self.fd.swap(-1, Ordering::SeqCst);
        if fd >= 0 {
            unsafe {
                libc::close(fd);
            }
        }
    }

    fn poll(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            _ => None,
        }
    }

    fn wait_timeout(&mut self, secs: f64) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs_f64(secs.max(0.0));
        loop {
            if let Some(code) = self.poll() {
                return Some(code);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

/// The keystroke path's view of an attach client; the fake variant is the
/// scripted pipe `claude_bg/tests.rs` types into.
pub(crate) enum Client {
    Real(RealClient),
    #[cfg(test)]
    Fake(testhook::FakePipe),
}

impl Client {
    fn pid(&self) -> i32 {
        match self {
            Client::Real(c) => c.pid,
            #[cfg(test)]
            Client::Fake(f) => f.pid(),
        }
    }

    pub(super) fn mark(&self) -> usize {
        match self {
            Client::Real(c) => c.mark(),
            #[cfg(test)]
            Client::Fake(f) => f.mark(),
        }
    }

    pub(super) fn text_since(&self, mark: usize) -> String {
        match self {
            Client::Real(c) => c.text_since(mark),
            #[cfg(test)]
            Client::Fake(f) => f.text_since(mark),
        }
    }

    fn write_str(&mut self, payload: &str) -> std::io::Result<()> {
        match self {
            Client::Real(c) => c.write_str(payload),
            #[cfg(test)]
            Client::Fake(f) => f.write_str(payload),
        }
    }

    fn close_stdin(&mut self) {
        match self {
            Client::Real(c) => c.close_stdin(),
            #[cfg(test)]
            Client::Fake(f) => f.close(),
        }
    }

    fn poll(&mut self) -> Option<i32> {
        match self {
            Client::Real(c) => c.poll(),
            #[cfg(test)]
            Client::Fake(f) => f.poll(),
        }
    }

    fn wait_timeout(&mut self, secs: f64) -> Option<i32> {
        match self {
            Client::Real(c) => c.wait_timeout(secs),
            #[cfg(test)]
            Client::Fake(f) => {
                let _ = secs;
                f.wait_timeout()
            }
        }
    }

    fn kill(&mut self) {
        match self {
            Client::Real(c) => c.kill(),
            #[cfg(test)]
            Client::Fake(f) => f.kill(),
        }
    }
}

/// (cols, rows) the engine is rendering at — its viewer pane's size.
///
/// The pane hive bound the job to is the client that set the current size, so
/// matching it means the attach changes nothing. With no pane on record (or
/// no tmux answer) the engine is not on anyone's screen and any size is
/// harmless; the fallback is the size claude's own pty host starts at.
pub(crate) fn engine_screen_size(job_id: &str) -> (u16, u16) {
    if let Some(pane) = pane_for_job(job_id) {
        if let Some(raw) = crate::tmux::display_value(&pane, "#{pane_width}\t#{pane_height}") {
            let mut parts = raw.splitn(2, '\t');
            let cols = parts.next().unwrap_or("");
            let rows = parts.next().unwrap_or("");
            if !cols.is_empty()
                && !rows.is_empty()
                && cols.chars().all(|c| c.is_ascii_digit())
                && rows.chars().all(|c| c.is_ascii_digit())
            {
                if let (Ok(cols), Ok(rows)) = (cols.parse::<u16>(), rows.parse::<u16>()) {
                    if cols > 0 && rows > 0 {
                        return (cols, rows);
                    }
                }
            }
        }
    }
    (_DEFAULT_PTY_COLS, _DEFAULT_PTY_ROWS)
}

pub(crate) fn attach_pipe(job_id: &str, claude_bin: &str) -> Option<Client> {
    #[cfg(test)]
    {
        if let Some(Some(pipe)) = testhook::with(|h| h.attach_pipe.clone()) {
            return Some(Client::Fake(pipe));
        }
    }
    let (cols, rows) = engine_screen_size(job_id);
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    };
    if rc != 0 {
        return None;
    }
    let out_fd = unsafe { libc::dup(slave) };
    let err_fd = unsafe { libc::dup(slave) };
    if out_fd < 0 || err_fd < 0 {
        unsafe {
            libc::close(master);
            libc::close(slave);
            if out_fd >= 0 {
                libc::close(out_fd);
            }
            if err_fd >= 0 {
                libc::close(err_fd);
            }
        }
        return None;
    }
    let mut cmd = Command::new(claude_bin);
    cmd.arg("attach").arg(job_id).env_clear().envs(bg_env(None));
    unsafe {
        use std::os::unix::io::FromRawFd;
        use std::os::unix::process::CommandExt;
        cmd.stdin(Stdio::from_raw_fd(slave));
        cmd.stdout(Stdio::from_raw_fd(out_fd));
        cmd.stderr(Stdio::from_raw_fd(err_fd));
        cmd.pre_exec(|| {
            // the pty is the client's tty, not ours
            libc::setsid();
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(child) => Some(Client::Real(RealClient::new(child, master))),
        Err(_) => {
            unsafe {
                libc::close(master);
            }
            None
        }
    }
}

pub(crate) fn feed(proc: &mut Client, payload: &str) -> bool {
    proc.write_str(payload).is_ok()
}

/// The engine the pipe is typing into — the attach itself wakes a parked one,
/// so this is also the wake wait. A client that exits first says the job is
/// gone; there is nothing left to wait for.
pub(crate) fn wait_engine_behind(job_id: &str, proc: &mut Client) -> Option<EngineSession> {
    wait_engine_entry_until(job_id, engine_ready_timeout(), || proc.poll().is_some())
}

fn engine_ready_timeout() -> f64 {
    #[cfg(test)]
    {
        if let Some(Some(v)) = testhook::with(|h| h.engine_ready_timeout) {
            return v;
        }
    }
    _ENGINE_READY_TIMEOUT
}

fn client_ready_timeout() -> f64 {
    #[cfg(test)]
    {
        if let Some(Some(v)) = testhook::with(|h| h.client_ready_timeout) {
            return v;
        }
    }
    _CLIENT_READY_TIMEOUT
}

pub(super) fn hooked_wait_engine_behind(job_id: &str, proc: &mut Client) -> Option<EngineSession> {
    #[cfg(test)]
    {
        if let Some(Some(v)) = testhook::with(|h| h.wait_engine_behind.clone()) {
            return v;
        }
    }
    wait_engine_behind(job_id, proc)
}

/// Wait until the attach client has the session on screen.
///
/// Its own attach-journal entry says so (~0.3s), and that matters for the
/// control bytes: a `\x15` written into a client that is not in raw key mode
/// yet is inserted into the composer as a literal character instead of
/// clearing it — observed once on 2.1.240, and silent when it happens.
pub(crate) fn wait_client_ready(proc: &mut Client) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f64(client_ready_timeout());
    while Instant::now() < deadline {
        if proc.poll().is_some() {
            return false;
        }
        if crate::adapters::claude_view::attach_entry_for_pid(proc.pid()).is_some() {
            return true;
        }
        sleep_s(0.1);
    }
    false
}

pub(super) fn hooked_wait_client_ready(proc: &mut Client) -> bool {
    #[cfg(test)]
    {
        if let Some(Some(v)) = testhook::with(|h| h.client_ready) {
            return v;
        }
    }
    wait_client_ready(proc)
}

/// C-u, in a chunk of its own — see [`wait_client_ready`].
pub(crate) fn clear_composer(proc: &mut Client) -> bool {
    if !feed(proc, _CLEAR_LINE) {
        return false;
    }
    sleep_s(_CONTROL_KEY_GAP);
    true
}

/// C-y: paste the draft the C-u killed back into the (now empty) composer.
///
/// Best-effort — a failed restore leaves what today's behavior always left,
/// the draft on claude's kill ring with the TUI's own Ctrl+Y hint on screen.
pub(crate) fn restore_draft(proc: &mut Client) {
    sleep_s(_CONTROL_KEY_GAP);
    if feed(proc, _RESTORE_KILL) {
        sleep_s(_CONTROL_KEY_GAP); // let the client forward it before EOF
    }
}

/// Let the attach client exit, and make sure it does.
///
/// A wedged client would otherwise outlive the caller holding the pipe open;
/// nothing downstream may block on it — reap off-thread so the caller
/// returns immediately.
pub(crate) fn close_pipe(mut proc: Client) {
    proc.close_stdin();
    thread::spawn(move || {
        if proc.wait_timeout(_ATTACH_EXIT_TIMEOUT).is_none() {
            proc.kill();
            let _ = proc.wait_timeout(_ATTACH_EXIT_TIMEOUT);
        }
    });
}
