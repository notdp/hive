//! Codex app-server client over a single shared daemon.
//!
//! One `codex app-server --listen unix://<sock>` daemon per CODEX_HOME hosts
//! every hive codex thread. Each codex TUI attaches with `codex resume
//! <threadId> --remote unix://<sock> --cd <cwd>` and drives its own thread;
//! hive connects as one more client over the same socket for runtime signals
//! and turn delivery.
//!
//! Identity is the threadId (== transcript sessionId), never the process
//! environment: the daemon's env is frozen at spawn time and shared by every
//! thread, so `TMUX_PANE` is stripped from it and codex's own per-thread
//! `CODEX_THREAD_ID` injection into tool subprocesses is the tool-side
//! identity. Which thread belongs to which tmux pane is recorded in a
//! per-pane `.thread` file beside the socket.
//!
//! Transport is WebSocket framing over the unix socket — RFC6455 masked text
//! frames, one background reader thread per connection.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{json, Map, Value};

const _HANDSHAKE_TIMEOUT: f64 = 5.0;
const _CALL_TIMEOUT: f64 = 10.0;

/// Worst-case local submission budget for one send_to_pane call (fresh daemon
/// handshake plus the turn/start RPC). The hived derives its request budgets
/// from this so a valid slow acceptance can never outlive the caller's timeout.
pub const SUBMIT_TIMEOUT: f64 = _HANDSHAKE_TIMEOUT + _CALL_TIMEOUT;
const _DAEMON_START_TIMEOUT: f64 = 8.0;
const _CONNECT_COOLDOWN: f64 = 5.0;
const _RESUME_COOLDOWN: f64 = 5.0;

pub fn codex_home() -> PathBuf {
    match env::var("CODEX_HOME") {
        Ok(value) => PathBuf::from(value),
        Err(_) => PathBuf::from(env::var("HOME").unwrap_or_default()).join(".codex"),
    }
}

/// The shared daemon's socket under the real CODEX_HOME.
///
/// Lives under `app-server-control/` (a real directory codex itself uses, so
/// it is never a symlink — codex rejects a symlinked socket parent, e.g.
/// `/tmp` on macOS). The path carries no per-pane or per-worktree component:
/// unix socket paths cap at ~104 bytes (SUN_LEN) and there is exactly one
/// daemon per CODEX_HOME.
pub fn shared_socket_path() -> PathBuf {
    codex_home()
        .join("app-server-control")
        .join("hive-shared.sock")
}

pub fn shared_pidfile_path() -> PathBuf {
    shared_socket_path().with_extension("pid")
}

/// Per-pane record of the thread hive bound to this pane.
pub fn pane_thread_path(pane: &str) -> PathBuf {
    let slug = pane.replace('%', "");
    let slug = if slug.is_empty() {
        "default"
    } else {
        slug.as_str()
    };
    codex_home()
        .join("app-server-control")
        .join(format!("hive-pane-{slug}.thread"))
}

pub fn write_pane_thread(pane: &str, thread_id: &str, cwd: &str) -> Result<()> {
    let path = pane_thread_path(pane);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &path,
        json!({"threadId": thread_id, "cwd": cwd}).to_string(),
    )?;
    Ok(())
}

pub fn read_pane_thread(pane: &str) -> Option<(String, String)> {
    let text = fs::read_to_string(pane_thread_path(pane)).ok()?;
    let data: Value = serde_json::from_str(&text).ok()?;
    let obj = data.as_object()?;
    let thread_id = obj
        .get("threadId")
        .and_then(Value::as_str)
        .filter(|tid| !tid.is_empty())?;
    let cwd = obj.get("cwd").and_then(Value::as_str).unwrap_or("");
    Some((thread_id.to_string(), cwd.to_string()))
}

pub fn clear_pane_thread(pane: &str) -> Result<()> {
    match fs::remove_file(pane_thread_path(pane)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub fn thread_id_for_pane(pane: &str) -> Option<String> {
    read_pane_thread(pane).map(|(tid, _cwd)| tid)
}

/// Inverse of [`pane_thread_path`]: `hive-pane-19.thread` -> `%19`.
fn _pane_from_record_name(name: &str) -> Option<String> {
    let slug = name.strip_prefix("hive-pane-")?.strip_suffix(".thread")?;
    if slug.is_empty() || slug == "default" {
        return None;
    }
    Some(format!("%{slug}"))
}

/// Pane ids that currently have a thread record on disk.
pub fn list_recorded_panes() -> Vec<String> {
    let root = codex_home().join("app-server-control");
    if !root.is_dir() {
        return Vec::new();
    }
    let mut panes = Vec::new();
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if let Some(pane) = _pane_from_record_name(name) {
                    panes.push(pane);
                }
            }
        }
    }
    panes
}

/// Pane recorded for *thread_id*, or None.
///
/// The reverse lookup behind tool-side identity: a `hive` invocation inside a
/// codex tool carries `CODEX_THREAD_ID` (injected per thread by codex), and
/// this maps it back to the tmux pane hive bound the thread to.
pub fn pane_for_thread(thread_id: &str) -> Option<String> {
    if thread_id.is_empty() {
        return None;
    }
    for pane in list_recorded_panes() {
        if let Some((tid, _cwd)) = read_pane_thread(&pane) {
            if tid == thread_id {
                return Some(pane);
            }
        }
    }
    None
}

// --------------------------------------------------------------------------
// directory trust (config.toml)
// --------------------------------------------------------------------------

/// Python `_TRUST_LEVEL_RE`: `^\s*trust_level\s*=`.
fn _trust_level_line(line: &str) -> bool {
    match line.trim_start().strip_prefix("trust_level") {
        Some(rest) => rest.trim_start().starts_with('='),
        None => false,
    }
}

/// Header spellings that name *directory*'s [projects] entry.
///
/// Codex writes the TOML basic-string form; the literal-string form is also
/// matched (when representable) so a hand-edited entry is not duplicated — a
/// duplicate table would make the whole config.toml unparsable.
fn _trusted_section_headers(directory: &str) -> Vec<String> {
    let escaped = directory.replace('\\', "\\\\").replace('"', "\\\"");
    let mut headers = vec![format!("[projects.\"{escaped}\"]")];
    if !directory.contains('\'') {
        headers.push(format!("[projects.'{directory}']"));
    }
    headers
}

/// Python str.splitlines(keepends=True) over \n, \r\n, and \r.
fn _split_keepends(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                lines.push(text[start..=i].to_string());
                i += 1;
                start = i;
            }
            b'\r' => {
                let mut end = i + 1;
                if end < bytes.len() && bytes[end] == b'\n' {
                    end += 1;
                }
                lines.push(text[start..end].to_string());
                i = end;
                start = end;
            }
            _ => i += 1,
        }
    }
    if start < bytes.len() {
        lines.push(text[start..].to_string());
    }
    lines
}

/// Converge `[projects."<dir>"] trust_level = "trusted"` in config.toml.
///
/// Remote-mode directory trust is judged from the daemon's config.toml on
/// disk (`-c` overrides do not apply), so every new cwd must be trusted
/// before its thread starts. Idempotent line-level edit: read, minimally
/// patch, write only on change; an unreadable config is left alone.
pub fn ensure_dir_trusted(directory: &str) -> Result<()> {
    let config_path = codex_home().join("config.toml");
    let mut content = String::new();
    if config_path.exists() {
        match fs::read_to_string(&config_path) {
            Ok(text) => content = text,
            Err(_) => return Ok(()),
        }
    }
    let original = content.clone();
    let headers = _trusted_section_headers(directory);
    let lines = _split_keepends(&content);
    let mut start = None;
    for (i, line) in lines.iter().enumerate() {
        let stripped = line.trim();
        if headers.iter().any(|h| {
            stripped == h
                || stripped.starts_with(&format!("{h} "))
                || stripped.starts_with(&format!("{h}#"))
        }) {
            start = Some(i + 1);
            break;
        }
    }
    match start {
        None => {
            let section = format!("{}\ntrust_level = \"trusted\"\n", headers[0]);
            if content.is_empty() {
                content = section;
            } else if content.ends_with('\n') {
                content.push('\n');
                content.push_str(&section);
            } else {
                content.push_str("\n\n");
                content.push_str(&section);
            }
        }
        Some(start) => {
            let mut end = start;
            while end < lines.len() && !lines[end].trim().starts_with('[') {
                end += 1;
            }
            let mut body: Vec<String> = lines[start..end].to_vec();
            let mut replaced = false;
            for line in body.iter_mut() {
                if _trust_level_line(line) {
                    if line.trim() == "trust_level = \"trusted\"" {
                        return Ok(());
                    }
                    *line = "trust_level = \"trusted\"\n".to_string();
                    replaced = true;
                    break;
                }
            }
            if !replaced {
                body.insert(0, "trust_level = \"trusted\"\n".to_string());
            }
            let mut rebuilt: Vec<String> = lines[..start].to_vec();
            rebuilt.extend(body);
            rebuilt.extend_from_slice(&lines[end..]);
            content = rebuilt.concat();
        }
    }
    if content != original {
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&config_path, content)?;
    }
    Ok(())
}

// --------------------------------------------------------------------------
// transport: minimal RFC6455 client over a unix socket (text frames, masked)
// --------------------------------------------------------------------------

/// Accepted-transport classification for durable delivery observations: the
/// shared daemon took the turn. Not proof the turn produced output.
pub const TURN_START_ACCEPTED: &str = "turnStartAccepted";

/// Interrupt outcomes: the daemon aborted the running turn, or there was no
/// turn to abort (an idle thread is nothing to interrupt, not a failure).
pub const TURN_INTERRUPT_ACCEPTED: &str = "turnInterruptAccepted";
pub const NO_RUNNING_TURN: &str = "noRunningTurn";

fn _urandom(n: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    let mut file = fs::File::open("/dev/urandom")?;
    file.read_exact(&mut buf)?;
    Ok(buf)
}

fn _b64encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn _find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn _ws_send_frame(stream: &UnixStream, opcode: u8, payload: &[u8]) -> io::Result<()> {
    let n = payload.len();
    let mut frame = Vec::with_capacity(n + 14);
    frame.push(0x80 | opcode);
    if n < 126 {
        frame.push(0x80 | n as u8);
    } else if n < 65536 {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(n as u64).to_be_bytes());
    }
    let mask = _urandom(4)?;
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, c)| c ^ mask[i % 4]));
    let mut writer = stream;
    writer.write_all(&frame)
}

/// Python `_WSConn`.
pub struct WsConn {
    stream: Arc<UnixStream>,
    rx: Vec<u8>,
}

impl WsConn {
    pub fn connect(path: &Path, timeout: Duration) -> io::Result<WsConn> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let mut conn = WsConn {
            stream: Arc::new(stream),
            rx: Vec::new(),
        };
        conn._handshake()?;
        // The timeout guards only the handshake. A live daemon can legally go
        // silent for 5s+ mid-call (its models refresh stalls exactly 5.00s on
        // a stale cache) — leaving it armed lets that silence kill the reader
        // thread right before the response.
        conn.stream.set_read_timeout(None)?;
        conn.stream.set_write_timeout(None)?;
        Ok(conn)
    }

    fn _handshake(&mut self) -> io::Result<()> {
        let key = _b64encode(&_urandom(16)?);
        let req = format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\n\
             Upgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        {
            let mut writer = &*self.stream;
            writer.write_all(req.as_bytes())?;
        }
        let mut data: Vec<u8> = Vec::new();
        while _find(&data, b"\r\n\r\n").is_none() {
            let mut chunk = [0u8; 4096];
            let n = {
                let mut reader = &*self.stream;
                reader.read(&mut chunk)?
            };
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "app-server handshake closed early",
                ));
            }
            data.extend_from_slice(&chunk[..n]);
        }
        let head_end = _find(&data, b"\r\n").unwrap_or(data.len());
        if _find(&data[..head_end], b"101").is_none() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!(
                    "app-server handshake rejected: {}",
                    String::from_utf8_lossy(&data[..data.len().min(64)])
                ),
            ));
        }
        let body_start = _find(&data, b"\r\n\r\n").unwrap_or(data.len() - 4) + 4;
        self.rx = data[body_start..].to_vec();
        Ok(())
    }

    fn _recv_exact(&mut self, n: usize) -> io::Result<Vec<u8>> {
        while self.rx.len() < n {
            let mut chunk = [0u8; 65536];
            let read = {
                let mut reader = &*self.stream;
                reader.read(&mut chunk)?
            };
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "app-server connection closed",
                ));
            }
            self.rx.extend_from_slice(&chunk[..read]);
        }
        let rest = self.rx.split_off(n);
        Ok(std::mem::replace(&mut self.rx, rest))
    }

    fn _recv_frame(&mut self) -> io::Result<(bool, u8, Vec<u8>)> {
        let header = self._recv_exact(2)?;
        let (b0, b1) = (header[0], header[1]);
        let fin = b0 & 0x80 != 0;
        let opcode = b0 & 0x0F;
        let masked = b1 & 0x80 != 0;
        let mut length = (b1 & 0x7F) as u64;
        if length == 126 {
            let bytes = self._recv_exact(2)?;
            length = u16::from_be_bytes([bytes[0], bytes[1]]) as u64;
        } else if length == 127 {
            let bytes = self._recv_exact(8)?;
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&bytes);
            length = u64::from_be_bytes(raw);
        }
        let mask = if masked {
            self._recv_exact(4)?
        } else {
            Vec::new()
        };
        let mut payload = self._recv_exact(length as usize)?;
        if masked {
            for (i, c) in payload.iter_mut().enumerate() {
                *c ^= mask[i % 4];
            }
        }
        Ok((fin, opcode, payload))
    }

    pub fn recv_text(&mut self) -> io::Result<String> {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let (fin, opcode, payload) = self._recv_frame()?;
            if opcode == 0x8 {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "app-server sent close",
                ));
            }
            if opcode == 0x9 {
                _ws_send_frame(&self.stream, 0xA, &payload)?;
                continue;
            }
            if opcode == 0xA {
                continue;
            }
            buf.extend_from_slice(&payload);
            if fin {
                return Ok(String::from_utf8_lossy(&buf).into_owned());
            }
        }
    }

    pub fn send_text(&self, text: &str) -> io::Result<()> {
        _ws_send_frame(&self.stream, 0x1, text.as_bytes())
    }

    pub fn close(&self) {
        let _ = _ws_send_frame(&self.stream, 0x8, b"");
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

// --------------------------------------------------------------------------
// per-thread runtime state, kept current by the reader thread
// --------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ThreadRuntime {
    pub busy: bool,
    pub turn_phase: String,
    pub input_state: String,
    pub observed_at: f64,
}

impl Default for ThreadRuntime {
    fn default() -> Self {
        ThreadRuntime {
            busy: false,
            turn_phase: "unknown_evidence".to_string(),
            input_state: String::new(),
            observed_at: 0.0,
        }
    }
}

fn _now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn _apply_status(rt: &mut ThreadRuntime, status: &Value) {
    match status.get("type").and_then(Value::as_str) {
        Some("active") => {
            rt.busy = true;
            rt.turn_phase = "tool_open".to_string();
            let waiting =
                status
                    .get("activeFlags")
                    .and_then(Value::as_array)
                    .map_or(false, |flags| {
                        flags.iter().any(|flag| {
                            matches!(
                                flag.as_str(),
                                Some("waitingOnApproval") | Some("waitingOnUserInput")
                            )
                        })
                    });
            rt.input_state = if waiting { "waiting_user" } else { "ready" }.to_string();
        }
        Some("idle") => {
            rt.busy = false;
            rt.turn_phase = "turn_closed".to_string();
            rt.input_state = "ready".to_string();
        }
        // notLoaded / systemError: leave prior fields, only observed_at advanced
        _ => {}
    }
}

// --------------------------------------------------------------------------
// one connection to the shared daemon
// --------------------------------------------------------------------------

type CallOverride = Box<dyn Fn(&str, &Value) -> Value + Send>;

struct Slot {
    msg: Mutex<Option<Value>>,
    cv: Condvar,
}

#[derive(Default)]
struct ClientState {
    threads: HashMap<String, ThreadRuntime>,
    resume_cooldown: HashMap<String, Instant>,
}

struct Inner {
    state: Mutex<ClientState>,
    pending: Mutex<HashMap<u64, Arc<Slot>>>,
    /// Doubles as the Python `_send_lock`: id mint + frame write are atomic.
    next_id: Mutex<u64>,
    stream: Option<Arc<UnixStream>>,
    closed: AtomicBool,
    reader: Mutex<Option<thread::JoinHandle<()>>>,
    /// Test seam standing in for Python's per-instance `c.call = ...`
    /// monkeypatch; always None in production.
    call_override: Mutex<Option<CallOverride>>,
}

#[derive(Clone)]
pub struct CodexDaemonClient {
    inner: Arc<Inner>,
}

fn _on_notification_state(inner: &Inner, method: &str, params: &Value) {
    // thread/status/changed is the only busy-relevant notification a
    // non-turn-owning client receives on the shared daemon (turn/* and item/*
    // go to the turn's own client only).
    if method != "thread/status/changed" {
        return;
    }
    let tid = match params.get("threadId").and_then(Value::as_str) {
        Some(tid) if !tid.is_empty() => tid,
        _ => return,
    };
    let mut state = inner.state.lock().unwrap();
    let rt = state.threads.entry(tid.to_string()).or_default();
    rt.observed_at = _now_epoch();
    _apply_status(rt, params.get("status").unwrap_or(&Value::Null));
}

fn _reader_loop(inner: Arc<Inner>, mut conn: WsConn) {
    while !inner.closed.load(Ordering::SeqCst) {
        let txt = match conn.recv_text() {
            Ok(txt) => txt,
            Err(_) => break,
        };
        let msg: Value = match serde_json::from_str(&txt) {
            Ok(msg) => msg,
            Err(_) => continue,
        };
        // Pop atomically: a `call()` that timed out concurrently may have
        // already removed this rid. A missing slot just means the waiter is
        // gone (timed out) — drop the late response silently.
        let slot = msg
            .get("id")
            .and_then(Value::as_u64)
            .and_then(|rid| inner.pending.lock().unwrap().remove(&rid));
        if let Some(slot) = slot {
            *slot.msg.lock().unwrap() = Some(msg);
            slot.cv.notify_all();
        } else if let Some(method) = msg.get("method").and_then(Value::as_str) {
            if !method.is_empty() {
                let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
                _on_notification_state(&inner, method, &params);
            }
        }
    }
    inner.closed.store(true, Ordering::SeqCst);
}

impl CodexDaemonClient {
    pub fn new(socket_path: &Path) -> io::Result<CodexDaemonClient> {
        let conn = WsConn::connect(socket_path, Duration::from_secs_f64(_HANDSHAKE_TIMEOUT))?;
        let inner = Arc::new(Inner {
            state: Mutex::new(ClientState::default()),
            pending: Mutex::new(HashMap::new()),
            next_id: Mutex::new(0),
            stream: Some(conn.stream.clone()),
            closed: AtomicBool::new(false),
            reader: Mutex::new(None),
            call_override: Mutex::new(None),
        });
        let reader_inner = inner.clone();
        let handle = thread::spawn(move || _reader_loop(reader_inner, conn));
        *inner.reader.lock().unwrap() = Some(handle);
        Ok(CodexDaemonClient { inner })
    }

    // ---- request/response ----
    pub fn call(&self, method: &str, params: Value) -> Value {
        {
            let overridden = self.inner.call_override.lock().unwrap();
            if let Some(call) = overridden.as_ref() {
                return call(method, &params);
            }
        }
        let stream = match self.inner.stream.as_ref() {
            Some(stream) => stream,
            None => return json!({"__error__": "closed"}),
        };
        let slot = Arc::new(Slot {
            msg: Mutex::new(None),
            cv: Condvar::new(),
        });
        let rid;
        {
            let mut next_id = self.inner.next_id.lock().unwrap();
            if self.inner.closed.load(Ordering::SeqCst) {
                return json!({"__error__": "closed"});
            }
            *next_id += 1;
            rid = *next_id;
            self.inner.pending.lock().unwrap().insert(rid, slot.clone());
            let payload = json!({"id": rid, "method": method, "params": params});
            if let Err(err) = _ws_send_frame(stream, 0x1, payload.to_string().as_bytes()) {
                self.inner.pending.lock().unwrap().remove(&rid);
                return json!({"__error__": err.to_string()});
            }
        }
        let guard = slot.msg.lock().unwrap();
        let (mut guard, _timeout) = slot
            .cv
            .wait_timeout_while(guard, Duration::from_secs_f64(_CALL_TIMEOUT), |msg| {
                msg.is_none()
            })
            .unwrap();
        let msg = match guard.take() {
            Some(msg) => msg,
            None => {
                self.inner.pending.lock().unwrap().remove(&rid);
                return json!({"__timeout__": true});
            }
        };
        if let Some(err) = msg.get("error") {
            return json!({"__error__": err.clone()});
        }
        json!({"result": msg.get("result").cloned().unwrap_or(Value::Null)})
    }

    // ---- notification -> state ----
    pub fn _on_notification(&self, method: &str, params: &Value) {
        _on_notification_state(&self.inner, method, params);
    }

    fn _seed_status(&self, thread_id: &str, status: &Value) {
        if !status.is_object() {
            return;
        }
        let mut state = self.inner.state.lock().unwrap();
        let rt = state.threads.entry(thread_id.to_string()).or_default();
        rt.observed_at = _now_epoch();
        _apply_status(rt, status);
    }

    pub fn runtime_for(&self, thread_id: &str) -> Option<ThreadRuntime> {
        self.inner
            .state
            .lock()
            .unwrap()
            .threads
            .get(thread_id)
            .cloned()
    }

    /// Runtime for *thread_id*, resuming once to recover missing state.
    ///
    /// A client connected before the thread existed has no state for it until
    /// the first status broadcast; `thread/resume` returns the thread's
    /// current status and backfills it. Rate-limited per thread so a
    /// never-resolving id does not storm resumes.
    pub fn runtime_or_backfill(&self, thread_id: &str) -> Option<ThreadRuntime> {
        if let Some(rt) = self.runtime_for(thread_id) {
            return Some(rt);
        }
        {
            let mut state = self.inner.state.lock().unwrap();
            let now = Instant::now();
            if let Some(until) = state.resume_cooldown.get(thread_id) {
                if now < *until {
                    return None;
                }
            }
            state.resume_cooldown.insert(
                thread_id.to_string(),
                now + Duration::from_secs_f64(_RESUME_COOLDOWN),
            );
        }
        self.resume(thread_id);
        self.runtime_for(thread_id)
    }

    // ---- protocol helpers ----
    pub fn initialize(&self) -> bool {
        let res = self.call(
            "initialize",
            json!({
                "clientInfo": {"name": "hive", "title": "hive", "version": "1"},
                "capabilities": {"experimentalApi": true},
            }),
        );
        res.get("result").is_some()
    }

    /// Recover state for already-active threads (busy late-join).
    ///
    /// A client online when a status edge fires gets the broadcast; this
    /// covers the late-join case by resuming each loaded thread once — the
    /// resume response carries the thread's current status.
    pub fn attach(&self) {
        for tid in self.loaded_list() {
            self.resume(&tid);
        }
    }

    pub fn loaded_list(&self) -> Vec<String> {
        let res = self.call("thread/loaded/list", json!({}));
        if res.get("result").is_none() {
            return Vec::new();
        }
        res.get("result")
            .and_then(|result| result.get("data"))
            .and_then(Value::as_array)
            .map(|data| {
                data.iter()
                    .filter_map(|item| item.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Backfill a thread's current status from `thread/resume`.
    pub fn resume(&self, thread_id: &str) -> bool {
        let res = self.call(
            "thread/resume",
            json!({"threadId": thread_id, "excludeTurns": true}),
        );
        let result = match res.get("result").and_then(Value::as_object) {
            Some(result) => result,
            None => return false,
        };
        if let Some(thread) = result.get("thread").filter(|t| t.is_object()) {
            self._seed_status(thread_id, thread.get("status").unwrap_or(&Value::Null));
        }
        true
    }

    /// Mint a new thread for *cwd*; return its threadId (== sessionId).
    ///
    /// `thread/start` alone leaves the thread unpersisted — `thread/resume`
    /// (and therefore the TUI's `codex resume <tid>`) fails with `no rollout
    /// found`. The follow-up `thread/name/set` flushes the rollout to disk
    /// (0.149.0 verified), so a minted thread is immediately resumable.
    /// *name* must be non-empty (the daemon rejects empty names).
    pub fn start_thread(&self, cwd: &str, name: &str, model: &str) -> Option<String> {
        let mut params = Map::new();
        params.insert("cwd".to_string(), json!(cwd));
        if !model.is_empty() {
            params.insert("model".to_string(), json!(model));
        }
        let res = self.call("thread/start", Value::Object(params));
        let thread = res
            .get("result")
            .and_then(Value::as_object)?
            .get("thread")
            .and_then(Value::as_object)?;
        let tid = _thread_id_from(thread)?;
        self._seed_status(&tid, thread.get("status").unwrap_or(&Value::Null));
        let flush = self.call("thread/name/set", json!({"threadId": tid, "name": name}));
        if flush.get("result").is_none() {
            return None; // unflushed thread is not attachable; treat as failure
        }
        Some(tid)
    }

    /// Fork a rolled-out thread server-side; return the fork's threadId.
    pub fn fork_thread(&self, thread_id: &str, name: &str) -> Option<String> {
        let res = self.call("thread/fork", json!({"threadId": thread_id}));
        let thread = res
            .get("result")
            .and_then(Value::as_object)?
            .get("thread")
            .and_then(Value::as_object)?;
        let tid = _thread_id_from(thread)?;
        self._seed_status(&tid, thread.get("status").unwrap_or(&Value::Null));
        let flush = self.call("thread/name/set", json!({"threadId": tid, "name": name}));
        if flush.get("result").is_none() {
            return None;
        }
        Some(tid)
    }

    pub fn turn_start(&self, thread_id: &str, text: &str) -> Value {
        self.call(
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{"type": "text", "text": text}],
            }),
        )
    }

    /// Id of the thread's in-progress turn, read from the daemon.
    ///
    /// `turn/interrupt` requires the turnId and `ThreadStatus::Active`
    /// carries none, so the id has to be read back — hive never owns the turn
    /// (the pane's TUI started it) and only the starting client gets `turn/*`
    /// notifications. `thread/read` with `includeTurns` is the one route.
    pub fn active_turn_id(&self, thread_id: &str) -> Option<String> {
        let res = self.call(
            "thread/read",
            json!({"threadId": thread_id, "includeTurns": true}),
        );
        let result = res.get("result").and_then(Value::as_object)?;
        let turns = result
            .get("thread")
            .and_then(|thread| thread.get("turns"))
            .and_then(Value::as_array);
        for turn in turns.map(Vec::as_slice).unwrap_or(&[]).iter().rev() {
            if turn.get("status").and_then(Value::as_str) == Some("inProgress") {
                let id = turn.get("id").and_then(Value::as_str).unwrap_or("");
                return if id.is_empty() {
                    None
                } else {
                    Some(id.to_string())
                };
            }
        }
        None
    }

    /// Abort *turn_id* on *thread_id*.
    ///
    /// 0.149.1 verified: the turnId is mandatory (omitting it answers
    /// `-32600 Invalid request: missing field turnId`) and is checked against
    /// the live turn, so a stale id can never abort a turn that started since.
    pub fn turn_interrupt(&self, thread_id: &str, turn_id: &str) -> Value {
        self.call(
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
        )
    }

    /// Start a context-compaction turn (the `/compact` slash equivalent).
    ///
    /// This is the dedicated RPC the codex TUI fires for `/compact`; sending
    /// `/compact` as `turn/start` text only feeds the model a literal prompt
    /// and never compacts.
    pub fn compact_start(&self, thread_id: &str) -> Value {
        self.call("thread/compact/start", json!({"threadId": thread_id}))
    }

    pub fn is_alive(&self) -> bool {
        if self.inner.closed.load(Ordering::SeqCst) {
            return false;
        }
        self.inner
            .reader
            .lock()
            .unwrap()
            .as_ref()
            .map_or(false, |handle| !handle.is_finished())
    }

    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        if let Some(stream) = self.inner.stream.as_ref() {
            let _ = _ws_send_frame(stream, 0x8, b"");
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

fn _thread_id_from(thread: &Map<String, Value>) -> Option<String> {
    match thread.get("id") {
        Some(Value::String(id)) if !id.is_empty() => Some(id.clone()),
        Some(Value::Number(id)) => Some(id.to_string()),
        _ => None,
    }
}

/// The client surface the pane-keyed API dials through `_shared_client`.
/// Python duck-types this; the trait is the Rust seam for the same fakes.
/// Methods returning `Result` model Python's "RPC raised" transport failures.
pub trait DaemonClient: Send + Sync {
    fn turn_start(&self, _thread_id: &str, _text: &str) -> Result<Value, String> {
        unimplemented!("turn_start")
    }
    fn active_turn_id(&self, _thread_id: &str) -> Result<Option<String>, String> {
        unimplemented!("active_turn_id")
    }
    fn turn_interrupt(&self, _thread_id: &str, _turn_id: &str) -> Result<Value, String> {
        unimplemented!("turn_interrupt")
    }
    fn runtime_or_backfill(&self, _thread_id: &str) -> Option<ThreadRuntime> {
        unimplemented!("runtime_or_backfill")
    }
    fn compact_start(&self, _thread_id: &str) -> Value {
        unimplemented!("compact_start")
    }
    fn start_thread(&self, _cwd: &str, _name: &str, _model: &str) -> Option<String> {
        unimplemented!("start_thread")
    }
    fn fork_thread(&self, _thread_id: &str, _name: &str) -> Option<String> {
        unimplemented!("fork_thread")
    }
}

impl DaemonClient for CodexDaemonClient {
    fn turn_start(&self, thread_id: &str, text: &str) -> Result<Value, String> {
        Ok(CodexDaemonClient::turn_start(self, thread_id, text))
    }
    fn active_turn_id(&self, thread_id: &str) -> Result<Option<String>, String> {
        Ok(CodexDaemonClient::active_turn_id(self, thread_id))
    }
    fn turn_interrupt(&self, thread_id: &str, turn_id: &str) -> Result<Value, String> {
        Ok(CodexDaemonClient::turn_interrupt(self, thread_id, turn_id))
    }
    fn runtime_or_backfill(&self, thread_id: &str) -> Option<ThreadRuntime> {
        CodexDaemonClient::runtime_or_backfill(self, thread_id)
    }
    fn compact_start(&self, thread_id: &str) -> Value {
        CodexDaemonClient::compact_start(self, thread_id)
    }
    fn start_thread(&self, cwd: &str, name: &str, model: &str) -> Option<String> {
        CodexDaemonClient::start_thread(self, cwd, name, model)
    }
    fn fork_thread(&self, thread_id: &str, name: &str) -> Option<String> {
        CodexDaemonClient::fork_thread(self, thread_id, name)
    }
}

// --------------------------------------------------------------------------
// daemon lifecycle
// --------------------------------------------------------------------------

/// True when a live daemon answers initialize on this socket.
pub fn probe_socket(socket_path: &Path) -> bool {
    let mut conn = match WsConn::connect(socket_path, Duration::from_secs(2)) {
        Ok(conn) => conn,
        Err(_) => return false,
    };
    let probe = json!({"id": 1, "method": "initialize", "params": {
        "clientInfo": {"name": "hive-probe", "version": "0"},
    }});
    let answered = (|| -> io::Result<bool> {
        conn.send_text(&probe.to_string())?;
        let txt = conn.recv_text()?;
        let msg: Value = serde_json::from_str(&txt)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        Ok(msg.get("id").and_then(Value::as_i64) == Some(1))
    })()
    .unwrap_or(false);
    conn.close();
    answered
}

pub fn daemon_alive() -> bool {
    let sock = shared_socket_path();
    sock.exists() && probe_socket(&sock)
}

/// Daemon env: the shared daemon serves every pane, so per-pane identity
/// markers must not freeze into it — tool subprocesses inherit this env and a
/// stale TMUX_PANE would impersonate whichever pane spawned the daemon.
/// Identity rides codex's own per-thread CODEX_THREAD_ID injection instead.
///
/// CLAUDE*/ANTHROPIC* are washed for the same reason (as the grok leader
/// does): the spawner may itself run inside a claude engine, and an inherited
/// CLAUDE_CODE_MESSAGING_SOCKET makes every hive call from a codex tool shell
/// resolve to *that* engine's pane whenever the thread lookup misses.
pub fn _daemon_env() -> HashMap<String, String> {
    let mut env: HashMap<String, String> = env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| !(key.starts_with("CLAUDE") || key.starts_with("ANTHROPIC")))
        .collect();
    env.remove("TMUX_PANE");
    env.remove("HIVE_CODEX_PANE");
    env
}

/// Ensure the shared app-server daemon is listening; return true if ready.
///
/// Reuses a live daemon if one already answers on the shared socket
/// (idempotent spawn); a stale socket from a dead daemon is removed first.
/// Shares the real CODEX_HOME (auth/model/permission defaults stay correct).
/// The daemon is machine-level state: nothing in hive kills it when panes or
/// teams go away, and the hived re-spawns it if it dies while codex members
/// live. Returns false if the daemon fails to bind or dies before ready.
pub fn spawn_daemon() -> bool {
    spawn_daemon_with("codex", _DAEMON_START_TIMEOUT)
}

pub fn spawn_daemon_with(codex_bin: &str, timeout: f64) -> bool {
    let sock = shared_socket_path();
    if let Some(parent) = sock.parent() {
        if fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    if sock.exists() {
        if probe_socket(&sock) {
            return true; // reuse the live daemon
        }
        let _ = fs::remove_file(&sock); // stale socket from a dead daemon
    }
    let stderr_path = codex_home()
        .join("app-server-control")
        .join("daemon.stderr");
    let stderr_file = match fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(stderr_path)
    {
        Ok(file) => file,
        Err(_) => return false,
    };
    let mut cmd = Command::new(codex_bin);
    cmd.arg("app-server")
        .arg("--listen")
        .arg(format!("unix://{}", sock.display()))
        .env_clear()
        .envs(_daemon_env())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file));
    unsafe {
        // Python start_new_session=True: detach from the short-lived caller.
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(_) => return false,
    };
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    while Instant::now() < deadline {
        if let Ok(Some(_status)) = child.try_wait() {
            return false; // died before binding
        }
        if probe_socket(&sock) {
            let _ = fs::write(shared_pidfile_path(), child.id().to_string());
            return true;
        }
        thread::sleep(Duration::from_millis(200));
    }
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    false
}

// --------------------------------------------------------------------------
// shared client (one per process, lazily connected)
// --------------------------------------------------------------------------

struct SharedSlot {
    client: Option<CodexDaemonClient>,
    cooldown_until: Option<Instant>,
}

static _CLIENT: Mutex<SharedSlot> = Mutex::new(SharedSlot {
    client: None,
    cooldown_until: None,
});

fn _shared_client() -> Option<Arc<dyn DaemonClient>> {
    #[cfg(test)]
    {
        if let Some(overridden) = tests::shared_client_override() {
            return overridden;
        }
    }
    _shared_client_prod().map(|client| {
        let dynamic: Arc<dyn DaemonClient> = Arc::new(client);
        dynamic
    })
}

fn _shared_client_prod() -> Option<CodexDaemonClient> {
    {
        let mut slot = _CLIENT.lock().unwrap();
        if let Some(client) = slot.client.as_ref() {
            if client.is_alive() {
                return Some(client.clone());
            }
        }
        if let Some(client) = slot.client.take() {
            client.close();
        }
        if let Some(until) = slot.cooldown_until {
            if Instant::now() < until {
                return None;
            }
        }
    }
    let sock = shared_socket_path();
    if !sock.exists() {
        _set_cooldown();
        return None;
    }
    let client = match CodexDaemonClient::new(&sock) {
        Ok(client) => client,
        Err(_) => {
            _set_cooldown();
            return None;
        }
    };
    if !client.initialize() {
        client.close();
        _set_cooldown();
        return None;
    }
    client.attach(); // busy late-join recovery
    _CLIENT.lock().unwrap().client = Some(client.clone());
    Some(client)
}

fn _set_cooldown() {
    _CLIENT.lock().unwrap().cooldown_until =
        Some(Instant::now() + Duration::from_secs_f64(_CONNECT_COOLDOWN));
}

/// Eagerly bring hive's client online (spawn time / hived request).
pub fn connect() -> bool {
    _shared_client().is_some()
}

/// Close the process's client so the next use reconnects (daemon respawn).
pub fn drop_client() {
    let client = {
        let mut slot = _CLIENT.lock().unwrap();
        slot.cooldown_until = None;
        slot.client.take()
    };
    if let Some(client) = client {
        client.close();
    }
}

// --------------------------------------------------------------------------
// pane-keyed API (thread resolved through the pane's record)
// --------------------------------------------------------------------------

pub fn runtime_for_pane(pane: &str) -> Option<ThreadRuntime> {
    let tid = thread_id_for_pane(pane)?;
    runtime_for_thread(&tid)
}

pub fn runtime_for_thread(thread_id: &str) -> Option<ThreadRuntime> {
    let client = _shared_client()?;
    client.runtime_or_backfill(thread_id)
}

/// Deliver text as a new turn on the pane's recorded thread.
///
/// Returns `TURN_START_ACCEPTED` when `turn/start` answered with a result —
/// the daemon accepted the turn, which is codex's transport boundary (not
/// proof the turn ran to completion). Returns `None` on transport failure:
/// no recorded thread (unmanaged codex), no daemon, an RPC error response,
/// or a connection failure. There is no keystroke fallback — normal hive
/// delivery never touches the composer. A *busy* thread is not bounced:
/// `turn/start` carries steer semantics in core, so hive hands it straight
/// to the RPC and lets codex pick the landing.
pub fn send_to_pane(pane: &str, text: &str) -> Option<&'static str> {
    let tid = thread_id_for_pane(pane)?;
    send_to_thread(&tid, text)
}

/// Deliver text as a new turn on *thread_id* — the engine-keyed core.
pub fn send_to_thread(thread_id: &str, text: &str) -> Option<&'static str> {
    let client = _shared_client()?;
    let response = client.turn_start(thread_id, text).ok()?;
    if response.get("result").is_some() {
        Some(TURN_START_ACCEPTED)
    } else {
        None
    }
}

/// Abort the running turn on the pane's recorded thread.
///
/// Returns `TURN_INTERRUPT_ACCEPTED` when the daemon took the interrupt,
/// `NO_RUNNING_TURN` when the thread has no in-progress turn (nothing to
/// abort — not a failure), and `None` on transport failure. There is no
/// keystroke fallback: an Escape into the pane would land on whatever the
/// viewer is showing, while `turn/interrupt` is addressed to the thread.
pub fn interrupt_pane(pane: &str) -> Option<&'static str> {
    let tid = thread_id_for_pane(pane)?;
    interrupt_thread(&tid)
}

/// Abort the running turn on *thread_id* — the engine-keyed core.
pub fn interrupt_thread(thread_id: &str) -> Option<&'static str> {
    let client = _shared_client()?;
    let turn_id = client.active_turn_id(thread_id).ok()?;
    let turn_id = match turn_id {
        Some(turn_id) if !turn_id.is_empty() => turn_id,
        _ => return Some(NO_RUNNING_TURN),
    };
    let response = client.turn_interrupt(thread_id, &turn_id).ok()?;
    if response.get("result").is_some() {
        Some(TURN_INTERRUPT_ACCEPTED)
    } else {
        None
    }
}

/// Start context compaction on the pane's recorded thread.
///
/// Compaction is *not* steerable: codex runs it as a Compact turn whose
/// first act is to abort any running turn. Firing it at a busy agent would
/// kill the in-flight work, so hive gates compaction on busy and only
/// compacts an idle thread.
///
/// Returns `"compacted"` (RPC accepted), `"busy"` (agent mid-turn), or
/// `"unavailable"` (no record / no daemon). On anything but `"compacted"`
/// the caller keystrokes `/compact` into the TUI so codex itself surfaces
/// its native "disabled while a task is in progress" refusal.
pub fn compact_pane(pane: &str) -> &'static str {
    let tid = match thread_id_for_pane(pane) {
        Some(tid) => tid,
        None => return "unavailable",
    };
    let client = match _shared_client() {
        Some(client) => client,
        None => return "unavailable",
    };
    if let Some(rt) = client.runtime_or_backfill(&tid) {
        if rt.busy {
            return "busy";
        }
    }
    if client.compact_start(&tid).get("result").is_some() {
        "compacted"
    } else {
        "unavailable"
    }
}

/// Transcript session id of the pane's recorded thread.
///
/// threadId == sessionId on the app-server surface, so this is a plain
/// record read — no daemon round-trip and no lsof.
pub fn session_id_for_pane(pane: &str) -> Option<String> {
    thread_id_for_pane(pane)
}

// --------------------------------------------------------------------------
// spawn-flow helpers
// --------------------------------------------------------------------------

fn _utc_stamp_seconds() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::gmtime_r(&secs, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year as i64 + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Renew ~/.codex/models_cache.json's fetched_at so a mint stays warm.
///
/// thread/start synchronously refetches /models when the cache is older than
/// codex's 300s TTL (~2.5s, up to its 5s timeout). The data barely changes
/// and codex itself renews the stamp without refetching on an etag match, so
/// extending the last real fetch is the same semantic; the daemon's periodic
/// Online refresh still overwrites with real data.
pub fn freshen_models_cache() -> bool {
    let path = codex_home().join("models_cache.json");
    let freshen = || -> Option<()> {
        let text = fs::read_to_string(&path).ok()?;
        let mut entry: Value = serde_json::from_str(&text).ok()?;
        let obj = entry.as_object_mut()?;
        obj.insert(
            "fetched_at".to_string(),
            Value::String(format!("{}.000000Z", _utc_stamp_seconds())),
        );
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string(&entry).ok()?).ok()?;
        fs::rename(&tmp, &path).ok()?;
        Some(())
    };
    freshen().is_some()
}

/// Mint a resumable thread for a new member; None on any failure.
pub fn start_member_thread(cwd: &str, name: &str, model: &str) -> Option<String> {
    let client = _shared_client()?;
    freshen_models_cache();
    client.start_thread(cwd, name, model)
}

/// Server-side fork of *thread_id*; returns the fork's id, None on failure.
pub fn fork_member_thread(thread_id: &str, name: &str) -> Option<String> {
    let client = _shared_client()?;
    freshen_models_cache();
    client.fork_thread(thread_id, name)
}

// --------------------------------------------------------------------------
// test seams
// --------------------------------------------------------------------------

#[cfg(test)]
impl CodexDaemonClient {
    /// Python `_bare_client()`: a client without a socket connection, for
    /// state-logic tests.
    fn bare() -> CodexDaemonClient {
        CodexDaemonClient {
            inner: Arc::new(Inner {
                state: Mutex::new(ClientState::default()),
                pending: Mutex::new(HashMap::new()),
                next_id: Mutex::new(0),
                stream: None,
                closed: AtomicBool::new(false),
                reader: Mutex::new(None),
                call_override: Mutex::new(None),
            }),
        }
    }

    /// Python `c.call = lambda ...` monkeypatch equivalent.
    fn set_call_override(&self, call: impl Fn(&str, &Value) -> Value + Send + 'static) {
        *self.inner.call_override.lock().unwrap() = Some(Box::new(call));
    }

    fn threads_is_empty(&self) -> bool {
        self.inner.state.lock().unwrap().threads.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::TEST_ENV_LOCK;
    use std::cell::RefCell;
    use std::os::unix::net::UnixListener;
    use std::sync::MutexGuard;

    thread_local! {
        static SHARED_CLIENT_OVERRIDE: RefCell<Option<Box<dyn Fn() -> Option<Arc<dyn DaemonClient>>>>> =
            RefCell::new(None);
    }

    /// Some(...) when this test thread monkeypatched `_shared_client`.
    pub(super) fn shared_client_override() -> Option<Option<Arc<dyn DaemonClient>>> {
        SHARED_CLIENT_OVERRIDE.with(|slot| slot.borrow().as_ref().map(|factory| factory()))
    }

    fn set_shared_client_override(factory: impl Fn() -> Option<Arc<dyn DaemonClient>> + 'static) {
        SHARED_CLIENT_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(Box::new(factory)));
    }

    fn override_client<T: DaemonClient + 'static>(fake: Arc<T>) {
        set_shared_client_override(move || {
            let client: Arc<dyn DaemonClient> = fake.clone();
            Some(client)
        });
    }

    fn env_guard() -> MutexGuard<'static, ()> {
        TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner())
    }

    fn _bare_client() -> CodexDaemonClient {
        CodexDaemonClient::bare()
    }

    type Calls = Arc<Mutex<Vec<(String, Value)>>>;

    fn recording_override(
        client: &CodexDaemonClient,
        respond: impl Fn(&str) -> Value + Send + 'static,
    ) -> Calls {
        let calls: Calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        client.set_call_override(move |method, params| {
            seen.lock()
                .unwrap()
                .push((method.to_string(), params.clone()));
            respond(method)
        });
        calls
    }

    // --- paths & records ----------------------------------------------------

    #[test]
    fn test_shared_socket_path_under_app_server_control() {
        let _guard = env_guard();
        env::remove_var("CODEX_HOME");
        let path = shared_socket_path();
        assert_eq!(path.file_name().unwrap(), "hive-shared.sock");
        assert_eq!(
            path.parent().unwrap().file_name().unwrap(),
            "app-server-control"
        );
        // macOS unix socket paths cap at 104 bytes; keep headroom.
        assert!(path.to_string_lossy().len() < 104);
    }

    #[test]
    fn test_shared_pidfile_path() {
        let _guard = env_guard();
        assert_eq!(
            shared_pidfile_path().file_name().unwrap(),
            "hive-shared.pid"
        );
    }

    #[test]
    fn test_pane_thread_record_roundtrip() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        write_pane_thread("%19", "tid-1", "/work").unwrap();
        assert_eq!(
            read_pane_thread("%19"),
            Some(("tid-1".to_string(), "/work".to_string()))
        );
        assert_eq!(thread_id_for_pane("%19").as_deref(), Some("tid-1"));
        assert_eq!(session_id_for_pane("%19").as_deref(), Some("tid-1")); // threadId == sessionId
        clear_pane_thread("%19").unwrap();
        assert_eq!(read_pane_thread("%19"), None);
        clear_pane_thread("%19").unwrap(); // idempotent
    }

    #[test]
    fn test_read_pane_thread_rejects_garbage() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        let path = pane_thread_path("%3");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not json").unwrap();
        assert_eq!(read_pane_thread("%3"), None);
        fs::write(&path, json!({"cwd": "/x"}).to_string()).unwrap(); // no threadId
        assert_eq!(read_pane_thread("%3"), None);
    }

    #[test]
    fn test_pane_for_thread_reverse_lookup() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        write_pane_thread("%19", "tid-a", "/work").unwrap();
        write_pane_thread("%7", "tid-b", "/work").unwrap();
        assert_eq!(pane_for_thread("tid-b").as_deref(), Some("%7"));
        assert_eq!(pane_for_thread("tid-a").as_deref(), Some("%19"));
        assert_eq!(pane_for_thread("missing"), None);
        assert_eq!(pane_for_thread(""), None);
    }

    #[test]
    fn test_list_recorded_panes() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        write_pane_thread("%19", "t1", "/w").unwrap();
        write_pane_thread("%7", "t2", "/w").unwrap();
        fs::write(
            tmp.path()
                .join("app-server-control")
                .join("hive-pane-default.thread"),
            "{}",
        )
        .unwrap();
        let mut panes = list_recorded_panes();
        panes.sort();
        assert_eq!(panes, vec!["%19", "%7"]);
    }

    #[test]
    fn test_list_recorded_panes_missing_dir() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        assert!(list_recorded_panes().is_empty());
    }

    #[test]
    fn test_daemon_env_strips_pane_identity() {
        // The shared daemon serves every pane: a frozen TMUX_PANE in its env
        // would let untagged tool shells impersonate whichever pane spawned
        // it. CLAUDE*/ANTHROPIC* go too: an inherited
        // CLAUDE_CODE_MESSAGING_SOCKET resolves a codex tool shell to the
        // spawning claude engine's pane.
        let _guard = env_guard();
        env::set_var("TMUX_PANE", "%old");
        env::set_var("HIVE_CODEX_PANE", "%old");
        env::set_var("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/cc-socks/321.sock");
        env::set_var("CLAUDE_CODE_ENTRYPOINT", "cli");
        env::set_var("ANTHROPIC_API_KEY", "sk-nope");
        env::set_var("CODEX_HOME", "/tmp/codex-home");
        let env_map = _daemon_env();
        assert!(!env_map.contains_key("TMUX_PANE"));
        assert!(!env_map.contains_key("HIVE_CODEX_PANE"));
        assert!(!env_map
            .keys()
            .any(|key| key.starts_with("CLAUDE") || key.starts_with("ANTHROPIC")));
        assert_eq!(
            env_map.get("CODEX_HOME").map(String::as_str),
            Some("/tmp/codex-home")
        );
        env::remove_var("TMUX_PANE");
        env::remove_var("HIVE_CODEX_PANE");
        env::remove_var("CLAUDE_CODE_MESSAGING_SOCKET");
        env::remove_var("CLAUDE_CODE_ENTRYPOINT");
        env::remove_var("ANTHROPIC_API_KEY");
    }

    // --- status mapping -----------------------------------------------------

    #[test]
    fn test_apply_status_active_ready() {
        let mut rt = ThreadRuntime::default();
        _apply_status(&mut rt, &json!({"type": "active", "activeFlags": []}));
        assert!(rt.busy);
        assert_eq!(rt.input_state, "ready");
        assert_eq!(rt.turn_phase, "tool_open");
    }

    #[test]
    fn test_apply_status_active_waiting_on_user_input() {
        let mut rt = ThreadRuntime::default();
        _apply_status(
            &mut rt,
            &json!({"type": "active", "activeFlags": ["waitingOnUserInput"]}),
        );
        assert_eq!(rt.input_state, "waiting_user");
    }

    #[test]
    fn test_apply_status_active_waiting_on_approval() {
        let mut rt = ThreadRuntime::default();
        _apply_status(
            &mut rt,
            &json!({"type": "active", "activeFlags": ["waitingOnApproval"]}),
        );
        assert_eq!(rt.input_state, "waiting_user");
    }

    #[test]
    fn test_apply_status_idle() {
        let mut rt = ThreadRuntime {
            busy: true,
            ..Default::default()
        };
        _apply_status(&mut rt, &json!({"type": "idle"}));
        assert!(!rt.busy);
        assert_eq!(rt.input_state, "ready");
        assert_eq!(rt.turn_phase, "turn_closed");
    }

    #[test]
    fn test_apply_status_unknown_kind_preserves_prior_fields() {
        let mut rt = ThreadRuntime {
            busy: true,
            input_state: "ready".to_string(),
            turn_phase: "tool_open".to_string(),
            ..Default::default()
        };
        _apply_status(&mut rt, &json!({"type": "systemError"}));
        assert!(rt.busy);
        assert_eq!(rt.input_state, "ready");
        assert_eq!(rt.turn_phase, "tool_open");
    }

    #[test]
    fn test_on_notification_status_changed() {
        let client = _bare_client();
        client._on_notification(
            "thread/status/changed",
            &json!({"threadId": "t1", "status": {"type": "active", "activeFlags": []}}),
        );
        assert!(client.runtime_for("t1").unwrap().busy);
        client._on_notification(
            "thread/status/changed",
            &json!({"threadId": "t1", "status": {"type": "idle"}}),
        );
        assert!(!client.runtime_for("t1").unwrap().busy);
    }

    #[test]
    fn test_on_notification_ignores_turn_events() {
        // turn/* only reaches the turn-owning client on a shared daemon;
        // folding them here would be dead code pretending to be signal.
        let client = _bare_client();
        client._on_notification(
            "turn/started",
            &json!({"threadId": "t1", "turn": {"id": "x"}}),
        );
        client._on_notification("turn/completed", &json!({"threadId": "t1"}));
        assert!(client.threads_is_empty());
    }

    #[test]
    fn test_on_notification_ignores_missing_thread_id() {
        let client = _bare_client();
        client._on_notification(
            "thread/status/changed",
            &json!({"status": {"type": "idle"}}),
        );
        assert!(client.threads_is_empty());
    }

    #[test]
    fn test_runtime_for_returns_copy_not_reference() {
        let client = _bare_client();
        client._on_notification(
            "thread/status/changed",
            &json!({"threadId": "t1", "status": {"type": "idle"}}),
        );
        let mut snap = client.runtime_for("t1").unwrap();
        snap.busy = true;
        assert!(!client.runtime_for("t1").unwrap().busy); // internal state untouched
    }

    // --- resume backfill ----------------------------------------------------

    #[test]
    fn test_resume_backfills_active_runtime_from_thread_status() {
        // Late-join recovery: resume must seed _threads from the thread's
        // status so runtime reads report native busy/turnPhase instead of None.
        let client = _bare_client();
        client.set_call_override(|_method, _params| {
            json!({
                "result": {"thread": {"sessionId": "s", "status": {"type": "active", "activeFlags": []}}}
            })
        });
        assert!(client.resume("t1"));
        let rt = client.runtime_for("t1");
        assert!(rt.is_some() && rt.unwrap().busy);
    }

    #[test]
    fn test_resume_backfills_idle_runtime_from_thread_status() {
        let client = _bare_client();
        client.set_call_override(|_method, _params| {
            json!({"result": {"thread": {"sessionId": "s", "status": {"type": "idle"}}}})
        });
        assert!(client.resume("t1"));
        let rt = client.runtime_for("t1").unwrap();
        assert!(!rt.busy);
        assert_eq!(rt.turn_phase, "turn_closed");
    }

    #[test]
    fn test_resume_returns_false_on_error() {
        let client = _bare_client();
        client.set_call_override(|_method, _params| json!({"__error__": "no rollout found"}));
        assert!(!client.resume("t1"));
        assert!(client.threads_is_empty());
    }

    #[test]
    fn test_attach_resumes_each_loaded_thread() {
        let client = _bare_client();
        let calls = recording_override(&client, |method| {
            if method == "thread/loaded/list" {
                json!({"result": {"data": ["t1", "t2"]}})
            } else {
                json!({"result": {}})
            }
        });
        client.attach();
        let seen: Vec<String> = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| method == "thread/resume")
            .map(|(_, params)| params["threadId"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(seen, vec!["t1", "t2"]);
    }

    #[test]
    fn test_runtime_or_backfill_resumes_once_per_cooldown() {
        let client = _bare_client();
        // keep the runtime missing: every resume answers with an error
        let calls = recording_override(&client, |_method| json!({"__error__": "missing"}));
        assert!(client.runtime_or_backfill("t1").is_none());
        assert!(client.runtime_or_backfill("t1").is_none()); // inside cooldown: no 2nd resume
        let resumes = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| method == "thread/resume")
            .count();
        assert_eq!(resumes, 1);
    }

    #[test]
    fn test_runtime_or_backfill_returns_backfilled_state() {
        let client = _bare_client();
        client.set_call_override(
            |_method, _params| json!({"result": {"thread": {"status": {"type": "idle"}}}}),
        );
        let rt = client.runtime_or_backfill("t1").unwrap();
        assert_eq!(rt.turn_phase, "turn_closed");
    }

    // --- mint / fork protocol -----------------------------------------------

    #[test]
    fn test_start_thread_mints_and_flushes() {
        let client = _bare_client();
        let calls = recording_override(&client, |method| {
            if method == "thread/start" {
                json!({"result": {"thread": {"id": "tid-new", "status": {"type": "idle"}}}})
            } else {
                json!({"result": {}})
            }
        });
        assert_eq!(
            client
                .start_thread("/work", "honey.val", "gpt-x")
                .as_deref(),
            Some("tid-new")
        );
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls[0],
            (
                "thread/start".to_string(),
                json!({"cwd": "/work", "model": "gpt-x"})
            )
        );
        // name/set is the rollout flush: without it the TUI's `codex resume
        // <tid>` fails with `no rollout found` (0.149.0 real-machine verified).
        assert_eq!(
            calls[1],
            (
                "thread/name/set".to_string(),
                json!({"threadId": "tid-new", "name": "honey.val"})
            )
        );
        // the mint seeds the runtime so a fresh member reads idle, not unknown
        assert!(client.runtime_for("tid-new").is_some());
    }

    #[test]
    fn test_start_thread_without_model_omits_param() {
        let client = _bare_client();
        let calls = recording_override(&client, |method| {
            if method == "thread/start" {
                json!({"result": {"thread": {"id": "t"}}})
            } else {
                json!({"result": {}})
            }
        });
        assert_eq!(client.start_thread("/work", "n", "").as_deref(), Some("t"));
        let calls = calls.lock().unwrap();
        let (_, start_params) = calls
            .iter()
            .find(|(method, _)| method == "thread/start")
            .unwrap();
        assert!(start_params.get("model").is_none());
    }

    #[test]
    fn test_start_thread_fails_when_flush_fails() {
        // An unflushed thread is not attachable by the TUI; minting must not
        // report success for a thread `codex resume` would refuse.
        let client = _bare_client();
        client.set_call_override(|method, _params| {
            if method == "thread/start" {
                json!({"result": {"thread": {"id": "t"}}})
            } else {
                json!({"__error__": "boom"})
            }
        });
        assert_eq!(client.start_thread("/work", "n", ""), None);
    }

    #[test]
    fn test_start_thread_fails_on_rpc_error() {
        let client = _bare_client();
        client.set_call_override(|_method, _params| json!({"__error__": "nope"}));
        assert_eq!(client.start_thread("/work", "n", ""), None);
    }

    #[test]
    fn test_fork_thread_returns_fork_id_and_flushes() {
        let client = _bare_client();
        let calls = recording_override(&client, |method| {
            if method == "thread/fork" {
                json!({"result": {"thread": {"id": "tid-fork", "forkedFromId": "tid-src"}}})
            } else {
                json!({"result": {}})
            }
        });
        assert_eq!(
            client.fork_thread("tid-src", "clone").as_deref(),
            Some("tid-fork")
        );
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls[0],
            ("thread/fork".to_string(), json!({"threadId": "tid-src"}))
        );
        assert_eq!(
            calls[1],
            (
                "thread/name/set".to_string(),
                json!({"threadId": "tid-fork", "name": "clone"})
            )
        );
    }

    #[test]
    fn test_fork_thread_fails_on_rpc_error() {
        let client = _bare_client();
        client.set_call_override(|_method, _params| json!({"__error__": "no rollout found"}));
        assert_eq!(client.fork_thread("tid-src", "clone"), None);
    }

    // --- pane-keyed API over the shared client ------------------------------

    fn _record(tmp: &Path, pane: &str, tid: &str) {
        env::set_var("CODEX_HOME", tmp);
        write_pane_thread(pane, tid, "/work").unwrap();
    }

    #[test]
    fn test_send_to_pane_turn_starts_even_when_busy() {
        // Busy is not bounced to the composer: turn/start carries steer
        // semantics in core, so hive hands a busy thread straight to the RPC.
        // The fake deliberately omits runtime methods: send_to_pane must not
        // consult them.
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        _record(tmp.path(), "%1", "t1");

        struct FakeClient {
            sent: Mutex<Vec<(String, String)>>,
        }
        impl DaemonClient for FakeClient {
            fn turn_start(&self, tid: &str, text: &str) -> Result<Value, String> {
                self.sent
                    .lock()
                    .unwrap()
                    .push((tid.to_string(), text.to_string()));
                Ok(json!({"result": {}}))
            }
        }
        let fake = Arc::new(FakeClient {
            sent: Mutex::new(Vec::new()),
        });
        override_client(fake.clone());
        assert_eq!(send_to_pane("%1", "hi"), Some(TURN_START_ACCEPTED));
        assert_eq!(
            *fake.sent.lock().unwrap(),
            vec![("t1".to_string(), "hi".to_string())]
        );
    }

    #[test]
    fn test_send_to_pane_fails_without_record() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        set_shared_client_override(|| -> Option<Arc<dyn DaemonClient>> {
            panic!("no record -> the daemon must not even be dialed")
        });
        assert_eq!(send_to_pane("%1", "hi"), None);
    }

    #[test]
    fn test_send_to_pane_fails_without_daemon() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        _record(tmp.path(), "%1", "t1");
        set_shared_client_override(|| None);
        assert_eq!(send_to_pane("%1", "hi"), None);
    }

    #[test]
    fn test_send_to_pane_fails_on_rpc_error_response() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        _record(tmp.path(), "%1", "t1");

        struct FakeClient;
        impl DaemonClient for FakeClient {
            fn turn_start(&self, _tid: &str, _text: &str) -> Result<Value, String> {
                Ok(json!({"error": {"code": -1, "message": "boom"}}))
            }
        }
        override_client(Arc::new(FakeClient));
        assert_eq!(send_to_pane("%1", "hi"), None);
    }

    #[test]
    fn test_send_to_pane_fails_on_rpc_exception() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        _record(tmp.path(), "%1", "t1");

        struct FakeClient;
        impl DaemonClient for FakeClient {
            fn turn_start(&self, _tid: &str, _text: &str) -> Result<Value, String> {
                Err("socket reset".to_string())
            }
        }
        override_client(Arc::new(FakeClient));
        assert_eq!(send_to_pane("%1", "hi"), None);
    }

    #[test]
    fn test_runtime_for_pane_reads_recorded_thread() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        _record(tmp.path(), "%1", "t1");

        struct FakeClient;
        impl DaemonClient for FakeClient {
            fn runtime_or_backfill(&self, tid: &str) -> Option<ThreadRuntime> {
                assert_eq!(tid, "t1");
                Some(ThreadRuntime {
                    busy: true,
                    ..Default::default()
                })
            }
        }
        override_client(Arc::new(FakeClient));
        let rt = runtime_for_pane("%1");
        assert!(rt.is_some() && rt.unwrap().busy);
    }

    #[test]
    fn test_runtime_for_pane_none_without_record() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        set_shared_client_override(|| -> Option<Arc<dyn DaemonClient>> {
            panic!("no record -> no daemon dial")
        });
        assert_eq!(runtime_for_pane("%1"), None);
    }

    #[test]
    fn test_compact_pane_compacts_when_idle() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        _record(tmp.path(), "%1", "t1");

        struct FakeClient {
            started: Mutex<Vec<String>>,
        }
        impl DaemonClient for FakeClient {
            fn runtime_or_backfill(&self, _tid: &str) -> Option<ThreadRuntime> {
                Some(ThreadRuntime::default())
            }
            fn compact_start(&self, tid: &str) -> Value {
                self.started.lock().unwrap().push(tid.to_string());
                json!({"result": {}})
            }
        }
        let fake = Arc::new(FakeClient {
            started: Mutex::new(Vec::new()),
        });
        override_client(fake.clone());
        assert_eq!(compact_pane("%1"), "compacted");
        assert_eq!(*fake.started.lock().unwrap(), vec!["t1".to_string()]);
    }

    #[test]
    fn test_compact_pane_busy_defers_without_aborting_turn() {
        // A Compact turn aborts any running turn, so a busy agent must never
        // be compacted out from under its in-flight work.
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        _record(tmp.path(), "%1", "t1");

        struct FakeClient;
        impl DaemonClient for FakeClient {
            fn runtime_or_backfill(&self, _tid: &str) -> Option<ThreadRuntime> {
                Some(ThreadRuntime {
                    busy: true,
                    ..Default::default()
                })
            }
            fn compact_start(&self, _tid: &str) -> Value {
                panic!("must not compact a busy agent (would abort its turn)")
            }
        }
        override_client(Arc::new(FakeClient));
        assert_eq!(compact_pane("%1"), "busy");
    }

    #[test]
    fn test_compact_pane_unavailable_without_record() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        assert_eq!(compact_pane("%1"), "unavailable");
    }

    // --- interrupt ----------------------------------------------------------

    /// A client whose thread/read answers with *turns*, recording every call.
    fn _client_reading(turns: Value) -> (CodexDaemonClient, Calls) {
        let client = _bare_client();
        let calls: Calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        client.set_call_override(move |method, params| {
            seen.lock()
                .unwrap()
                .push((method.to_string(), params.clone()));
            json!({"result": {"thread": {"turns": turns}}})
        });
        (client, calls)
    }

    #[test]
    fn test_active_turn_id_reads_the_in_progress_turn() {
        let (client, calls) = _client_reading(json!([
            {"id": "old", "status": "completed"},
            {"id": "live", "status": "inProgress"},
        ]));
        assert_eq!(client.active_turn_id("t1").as_deref(), Some("live"));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(
                "thread/read".to_string(),
                json!({"threadId": "t1", "includeTurns": true})
            )]
        );
    }

    #[test]
    fn test_active_turn_id_none_when_every_turn_is_finished() {
        let (client, _calls) = _client_reading(json!([{"id": "old", "status": "completed"}]));
        assert_eq!(client.active_turn_id("t1"), None);
    }

    #[test]
    fn test_active_turn_id_none_on_rpc_error() {
        let client = _bare_client();
        client.set_call_override(|_method, _params| json!({"__error__": "boom"}));
        assert_eq!(client.active_turn_id("t1"), None);
    }

    #[test]
    fn test_turn_interrupt_carries_thread_and_turn_id() {
        // The turnId is mandatory on this RPC and is checked against the live
        // turn, so it must be passed through verbatim.
        let client = _bare_client();
        let calls = recording_override(&client, |_method| json!({"result": {}}));
        assert!(client.turn_interrupt("t1", "live").get("result").is_some());
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(
                "turn/interrupt".to_string(),
                json!({"threadId": "t1", "turnId": "live"})
            )]
        );
    }

    #[test]
    fn test_interrupt_pane_aborts_the_running_turn() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        _record(tmp.path(), "%1", "t1");

        struct FakeClient {
            aborted: Mutex<Vec<(String, String)>>,
        }
        impl DaemonClient for FakeClient {
            fn active_turn_id(&self, tid: &str) -> Result<Option<String>, String> {
                assert_eq!(tid, "t1");
                Ok(Some("live".to_string()))
            }
            fn turn_interrupt(&self, tid: &str, turn_id: &str) -> Result<Value, String> {
                self.aborted
                    .lock()
                    .unwrap()
                    .push((tid.to_string(), turn_id.to_string()));
                Ok(json!({"result": {}}))
            }
        }
        let fake = Arc::new(FakeClient {
            aborted: Mutex::new(Vec::new()),
        });
        override_client(fake.clone());
        assert_eq!(interrupt_pane("%1"), Some(TURN_INTERRUPT_ACCEPTED));
        assert_eq!(
            *fake.aborted.lock().unwrap(),
            vec![("t1".to_string(), "live".to_string())]
        );
    }

    #[test]
    fn test_interrupt_pane_reports_an_idle_thread_without_interrupting() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        _record(tmp.path(), "%1", "t1");

        struct FakeClient;
        impl DaemonClient for FakeClient {
            fn active_turn_id(&self, _tid: &str) -> Result<Option<String>, String> {
                Ok(None)
            }
            fn turn_interrupt(&self, _tid: &str, _turn_id: &str) -> Result<Value, String> {
                panic!("no running turn -> nothing to interrupt")
            }
        }
        override_client(Arc::new(FakeClient));
        assert_eq!(interrupt_pane("%1"), Some(NO_RUNNING_TURN));
    }

    #[test]
    fn test_interrupt_pane_fails_without_record() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        set_shared_client_override(|| -> Option<Arc<dyn DaemonClient>> {
            panic!("no record -> the daemon must not even be dialed")
        });
        assert_eq!(interrupt_pane("%1"), None);
    }

    #[test]
    fn test_interrupt_pane_fails_without_daemon() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        _record(tmp.path(), "%1", "t1");
        set_shared_client_override(|| None);
        assert_eq!(interrupt_pane("%1"), None);
    }

    #[test]
    fn test_interrupt_pane_fails_on_rpc_error_response() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        _record(tmp.path(), "%1", "t1");

        struct FakeClient;
        impl DaemonClient for FakeClient {
            fn active_turn_id(&self, _tid: &str) -> Result<Option<String>, String> {
                Ok(Some("live".to_string()))
            }
            fn turn_interrupt(&self, _tid: &str, _turn_id: &str) -> Result<Value, String> {
                Ok(json!({"__error__": {"code": -32600, "message": "expected active turn id"}}))
            }
        }
        override_client(Arc::new(FakeClient));
        assert_eq!(interrupt_pane("%1"), None);
    }

    #[test]
    fn test_interrupt_pane_fails_on_rpc_exception() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        _record(tmp.path(), "%1", "t1");

        struct FakeClient;
        impl DaemonClient for FakeClient {
            fn active_turn_id(&self, _tid: &str) -> Result<Option<String>, String> {
                Err("socket reset".to_string())
            }
        }
        override_client(Arc::new(FakeClient));
        assert_eq!(interrupt_pane("%1"), None);
    }

    #[test]
    fn test_connect_true_when_client_established() {
        struct FakeClient;
        impl DaemonClient for FakeClient {}
        override_client(Arc::new(FakeClient));
        assert!(connect());
    }

    #[test]
    fn test_connect_false_when_no_daemon() {
        set_shared_client_override(|| None);
        assert!(!connect());
    }

    #[test]
    fn test_start_member_thread_delegates_to_client() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path()); // freshen must not touch the real cache

        struct FakeClient;
        impl DaemonClient for FakeClient {
            fn start_thread(&self, cwd: &str, name: &str, model: &str) -> Option<String> {
                if (cwd, name, model) == ("/w", "n", "m") {
                    Some("tid-x".to_string())
                } else {
                    None
                }
            }
        }
        override_client(Arc::new(FakeClient));
        assert_eq!(
            start_member_thread("/w", "n", "m").as_deref(),
            Some("tid-x")
        );
        set_shared_client_override(|| None);
        assert_eq!(start_member_thread("/w", "n", ""), None);
    }

    #[test]
    fn test_fork_member_thread_delegates_to_client() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());

        struct FakeClient;
        impl DaemonClient for FakeClient {
            fn fork_thread(&self, tid: &str, name: &str) -> Option<String> {
                if (tid, name) == ("src", "n") {
                    Some("tid-f".to_string())
                } else {
                    None
                }
            }
        }
        override_client(Arc::new(FakeClient));
        assert_eq!(fork_member_thread("src", "n").as_deref(), Some("tid-f"));
        set_shared_client_override(|| None);
        assert_eq!(fork_member_thread("src", "n"), None);
    }

    // --- directory trust ----------------------------------------------------

    #[test]
    fn test_ensure_dir_trusted_creates_config() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        ensure_dir_trusted("/work/dir").unwrap();
        let text = fs::read_to_string(tmp.path().join("config.toml")).unwrap();
        assert!(text.contains("[projects.\"/work/dir\"]"));
        assert!(text.contains("trust_level = \"trusted\""));
    }

    #[test]
    fn test_ensure_dir_trusted_appends_to_existing_config() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        let config = tmp.path().join("config.toml");
        fs::write(&config, "model = \"gpt-x\"\n").unwrap();
        ensure_dir_trusted("/work/dir").unwrap();
        let text = fs::read_to_string(&config).unwrap();
        assert!(text.starts_with("model = \"gpt-x\"\n"));
        assert!(text.contains("[projects.\"/work/dir\"]\ntrust_level = \"trusted\""));
    }

    #[test]
    fn test_ensure_dir_trusted_idempotent() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        let config = tmp.path().join("config.toml");
        ensure_dir_trusted("/work/dir").unwrap();
        let first = fs::read_to_string(&config).unwrap();
        let before = fs::metadata(&config).unwrap().modified().unwrap();
        ensure_dir_trusted("/work/dir").unwrap();
        assert_eq!(fs::read_to_string(&config).unwrap(), first);
        assert_eq!(fs::metadata(&config).unwrap().modified().unwrap(), before); // no rewrite on no-op
    }

    #[test]
    fn test_ensure_dir_trusted_upgrades_existing_entry() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        let config = tmp.path().join("config.toml");
        fs::write(
            &config,
            "[projects.\"/work/dir\"]\ntrust_level = \"untrusted\"\n\n[other]\nk = 1\n",
        )
        .unwrap();
        ensure_dir_trusted("/work/dir").unwrap();
        let text = fs::read_to_string(&config).unwrap();
        assert!(text.contains("trust_level = \"trusted\""));
        assert!(!text.contains("trust_level = \"untrusted\""));
        assert_eq!(text.matches("[projects.\"/work/dir\"]").count(), 1); // no duplicate table
        assert!(text.contains("[other]"));
    }

    #[test]
    fn test_ensure_dir_trusted_adds_missing_key_to_existing_section() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        let config = tmp.path().join("config.toml");
        fs::write(&config, "[projects.\"/work/dir\"]\nother = 1\n").unwrap();
        ensure_dir_trusted("/work/dir").unwrap();
        let text = fs::read_to_string(&config).unwrap();
        assert_eq!(text.matches("[projects.\"/work/dir\"]").count(), 1);
        assert!(text.contains("trust_level = \"trusted\""));
        assert!(text.contains("other = 1"));
    }

    #[test]
    fn test_ensure_dir_trusted_escapes_quotes() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        ensure_dir_trusted("/work/we\"ird").unwrap();
        let text = fs::read_to_string(tmp.path().join("config.toml")).unwrap();
        assert!(text.contains("[projects.\"/work/we\\\"ird\"]"));
    }

    #[test]
    fn test_ensure_dir_trusted_matches_literal_string_header() {
        // A hand-edited literal-string header must not gain a duplicate table
        // — duplicate tables make the whole config.toml unparsable.
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        let config = tmp.path().join("config.toml");
        fs::write(
            &config,
            "[projects.'/work/dir']\ntrust_level = \"trusted\"\n",
        )
        .unwrap();
        ensure_dir_trusted("/work/dir").unwrap();
        let text = fs::read_to_string(&config).unwrap();
        assert_eq!(text.matches("/work/dir").count(), 1);
    }

    // --- transport: reader must survive daemon silence ----------------------

    #[test]
    fn test_wsconn_read_survives_silence_longer_than_handshake_timeout() {
        // The handshake timeout must not stay armed on post-handshake reads.
        //
        // Guards the mint-hang regression: the daemon legally goes silent for
        // 5.00s mid thread/start (its models refresh stalls on a stale cache),
        // and an armed 5.0s socket timeout killed the reader right before the
        // response.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ws.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut conn, _addr) = listener.accept().unwrap();
            let mut data: Vec<u8> = Vec::new();
            while _find(&data, b"\r\n\r\n").is_none() {
                let mut buf = [0u8; 4096];
                let n = conn.read(&mut buf).unwrap();
                data.extend_from_slice(&buf[..n]);
            }
            conn.write_all(b"HTTP/1.1 101 Switching Protocols\r\n\r\n")
                .unwrap();
            thread::sleep(Duration::from_millis(800)); // silence > the 0.3s handshake timeout
            let payload = br#"{"id":1,"result":{}}"#;
            let mut frame = vec![0x81u8, payload.len() as u8];
            frame.extend_from_slice(payload);
            conn.write_all(&frame).unwrap();
        });
        let mut conn = WsConn::connect(&path, Duration::from_millis(300)).unwrap();
        let txt = conn.recv_text().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&txt).unwrap(),
            json!({"id": 1, "result": {}})
        );
        conn.close();
        server.join().unwrap();
    }

    // --- models cache freshening --------------------------------------------

    #[test]
    fn test_freshen_models_cache_renews_stamp_and_keeps_data() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        let path = tmp.path().join("models_cache.json");
        fs::write(
            &path,
            json!({
                "fetched_at": "2026-08-26T05:00:00.000000Z",
                "etag": "W/\"abc\"",
                "client_version": "0.149.1",
                "models": [{"slug": "m1"}],
            })
            .to_string(),
        )
        .unwrap();
        assert!(freshen_models_cache());
        let entry: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_ne!(entry["fetched_at"], json!("2026-08-26T05:00:00.000000Z"));
        assert!(entry["fetched_at"].as_str().unwrap().ends_with('Z'));
        assert_eq!(entry["etag"], json!("W/\"abc\""));
        assert_eq!(entry["models"], json!([{"slug": "m1"}]));
    }

    #[test]
    fn test_freshen_models_cache_tolerates_missing_and_garbage() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        env::set_var("CODEX_HOME", tmp.path());
        assert!(!freshen_models_cache()); // no file
        fs::write(tmp.path().join("models_cache.json"), "not json").unwrap();
        assert!(!freshen_models_cache());
    }
}
