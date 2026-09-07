//! Claude Code sessions on this machine and their cross-session inboxes.
//!
//! Every Claude Code session with cross-session messaging (2.1.224+) registers
//! itself in `<claude-config>/sessions/<pid>.json` and binds an inbox socket
//! (`messagingSocketPath`); `/list-agents` reads the same files. One line of
//! JSON written to that socket is queued for the session as a peer message.
//! The registry layout and the inbox line shape are what Claude Code does
//! today (observed on 2.1.237), not a published contract: every read here is
//! defensive, and `send` claims only that the socket accepted the bytes.

use std::env;
use std::fs;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::adapters::base::read_json_object;

pub const ACCEPTED_UDS_WRITE: &str = "udsWriteAccepted";
pub const ACCEPTED_DAEMON_REPLY: &str = "daemonReplyAccepted";
// The listener accepted the connection but did not read the whole frame in
// time — a stalled session, not an absent one; the frame may sit truncated on
// its side.
pub const WRITE_TIMED_OUT: &str = "udsWriteTimedOut";
// The status vocabulary a session reports in its registry entry (observed on
// 2.1.240, not a documented enum).
pub const STATUS_VALUES: [&str; 4] = ["busy", "shell", "idle", "waiting"];
const CONNECT_TIMEOUT: f64 = 2.0;
const WRITE_TIMEOUT: f64 = 10.0;
// Daemon control-socket lane (op "reply"): retry codes are the daemon's own
// readiness vocabulary (observed on 2.1.240) — the worker exists but cannot
// take input this instant. The bound keeps a hived RPC from hanging on a
// worker that never comes up; past it the caller falls back to the inbox lane.
const DAEMON_PROTO: u64 = 1;
const DAEMON_RETRY_CODES: [&str; 3] = ["ESTARTING", "ENOREPLY", "ERESPAWNING"];
const DAEMON_RETRY_LIMIT: u32 = 24;
const DAEMON_RETRY_DELAY: f64 = 0.2;
// The hived submit budget must cover a daemon_reply retry run plus a full
// fallback send() worst case. The retry run is costed as prompt answers
// (a retry code comes back immediately); a daemon that stalls mid-roundtrip
// is bounded by that attempt's own read timeout, not by this budget.
pub const SUBMIT_TIMEOUT: f64 =
    CONNECT_TIMEOUT + WRITE_TIMEOUT + DAEMON_RETRY_LIMIT as f64 * DAEMON_RETRY_DELAY + 2.0;

// Transcript bytes scanned for the desktop title: the `custom-title` record is
// written when the title is set and re-emitted near the tail as the session
// runs, so the tail window finds the current title; the head window catches a
// title set once at the start of a short session.
const TITLE_TAIL_BYTES: u64 = 512 * 1024;
const TITLE_HEAD_BYTES: u64 = 64 * 1024;

fn write_timeout() -> Duration {
    Duration::from_secs_f64(WRITE_TIMEOUT)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeSession {
    pub name: String,
    pub pid: i32,
    pub cwd: String,
    pub kind: String,
    /// What launched the session (`cli`, `claude-desktop`; observed on
    /// 2.1.263), empty when the registry entry does not say.
    pub entrypoint: String,
    pub socket_path: String,
    pub session_id: String,
    pub title: String,
}

impl ClaudeSession {
    /// *label* is this session's Claude Code name, its desktop title, or its
    /// pid (the one address that is always unique).
    pub fn answers_to(&self, label: &str) -> bool {
        label == self.name
            || (!self.title.is_empty() && label == self.title)
            || label == self.pid.to_string()
    }
}

pub(crate) fn config_dir() -> PathBuf {
    // CLAUDE_HOME is hive's own sandbox lever (tests and dev lanes point it at
    // a disposable tree); CLAUDE_CONFIG_DIR is Claude Code's relocation knob.
    // Honour both so a sandboxed run never reads — or messages — the
    // developer's real sessions. Every other reader of the tree
    // (`claude::claude_home`) delegates here.
    for key in ["CLAUDE_HOME", "CLAUDE_CONFIG_DIR"] {
        if let Ok(v) = env::var(key) {
            if !v.is_empty() {
                return PathBuf::from(v);
            }
        }
    }
    PathBuf::from(format!("{}/.claude", env::var("HOME").unwrap_or_default()))
}

pub(crate) fn registry_dir() -> PathBuf {
    config_dir().join("sessions")
}

/// The scalar fields the claude registry, job ledger and pane records
/// carry, as a string (containers never appear on them): a string as-is,
/// a non-zero number rendered, `true` as "True", else "".
pub(crate) fn truthy_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => {
            if n.as_f64() == Some(0.0) {
                String::new()
            } else {
                n.to_string()
            }
        }
        Some(Value::Bool(true)) => "True".to_string(),
        _ => String::new(),
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn title_in(chunk: &[u8]) -> String {
    let mut title = String::new();
    for line in chunk.split(|&b| b == b'\n') {
        if !contains_subslice(line, b"\"custom-title\"") {
            continue;
        }
        let Ok(rec) = serde_json::from_slice::<Value>(line) else {
            continue; // a partial line at a window edge
        };
        if rec.is_object() && rec.get("type").and_then(Value::as_str) == Some("custom-title") {
            // the last record wins, a cleared title included
            title = truthy_str(rec.get("customTitle"));
        }
    }
    title
}

/// The desktop app's title for *session_id* ("" when none was set).
///
/// Claude Code records it in the session transcript as a `custom-title`
/// line, so the title lives beside the conversation, not in the registry.
pub fn session_title(session_id: &str) -> String {
    if session_id.is_empty() {
        return String::new();
    }
    let fname = format!("{session_id}.jsonl");
    let Some(path) =
        crate::adapters::claude::stat_project_dirs(&config_dir().join("projects"), &fname)
    else {
        return String::new();
    };
    read_title(&path).unwrap_or_default()
}

fn read_title(path: &Path) -> std::io::Result<String> {
    let size = fs::metadata(path)?.len();
    let mut fh = fs::File::open(path)?;
    fh.seek(SeekFrom::Start(size.saturating_sub(TITLE_TAIL_BYTES)))?;
    let mut tail = Vec::new();
    fh.read_to_end(&mut tail)?;
    let mut title = title_in(&tail);
    if title.is_empty() && size > TITLE_TAIL_BYTES {
        fh.seek(SeekFrom::Start(0))?;
        let mut head = Vec::new();
        (&mut fh).take(TITLE_HEAD_BYTES).read_to_end(&mut head)?;
        title = title_in(&head);
    }
    Ok(title)
}

pub(crate) fn pid_alive(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    // EPERM: the process exists, we just may not signal it. Anything else
    // (ESRCH included) reads as dead.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Live sessions that bind an inbox socket, sorted by name.
///
/// A registration whose process is gone, records no socket (an older CLI,
/// bare mode) or is a warm spare is not a session anyone is talking to, and
/// is left out — the same three cuts `/list-agents` makes.
pub fn list_sessions() -> Vec<ClaudeSession> {
    let root = registry_dir();
    if !root.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut rows: Vec<ClaudeSession> = Vec::new();
    for entry in entries.flatten() {
        let fname = entry.file_name().to_string_lossy().into_owned();
        if fname.starts_with('.') || !fname.ends_with(".json") {
            continue;
        }
        let Some(obj) = read_json_object(&entry.path()) else {
            continue;
        };
        let name = truthy_str(obj.get("name"));
        let sock = truthy_str(obj.get("messagingSocketPath"));
        let pid = match obj.get("pid") {
            Some(Value::Number(n)) => n.as_i64().and_then(|p| i32::try_from(p).ok()),
            _ => None,
        };
        let (Some(pid), false, false) = (pid, name.is_empty(), sock.is_empty()) else {
            continue;
        };
        if crate::json_fields::is_set(obj.get("spare")) {
            continue; // a warm spare claude pre-started; nobody is behind it yet
        }
        if !pid_alive(pid) {
            continue;
        }
        let session_id = truthy_str(obj.get("sessionId"));
        let title = session_title(&session_id);
        rows.push(ClaudeSession {
            name,
            pid,
            cwd: truthy_str(obj.get("cwd")),
            kind: truthy_str(obj.get("kind")),
            entrypoint: truthy_str(obj.get("entrypoint")),
            socket_path: sock,
            session_id,
            title,
        });
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name).then(a.pid.cmp(&b.pid)));
    rows
}

/// (status, waitingFor) reported by the session running as *pid*.
///
/// Real terminal TUI sessions report `status` in their registry entry;
/// headless/desktop-hosted sessions never do. None when the entry is
/// missing, the process is dead, or no status is reported.
pub fn session_status(pid: Option<i32>) -> Option<(String, String)> {
    let pid = pid?;
    if pid == 0 {
        return None;
    }
    let obj = read_json_object(&registry_dir().join(format!("{pid}.json")))?;
    if !pid_alive(pid) {
        return None;
    }
    let status = obj.get("status").and_then(Value::as_str)?;
    if !STATUS_VALUES.contains(&status) {
        return None;
    }
    Some((status.to_string(), truthy_str(obj.get("waitingFor"))))
}

/// Fold a registry `status` into hive runtime fields.
///
/// `shell` is the session sitting at its own shell — not mid-turn, and not
/// waiting on an answer, so it reads exactly like `idle`.
pub fn runtime_from_status(status: &str, waiting_for: &str) -> Map<String, Value> {
    fn fields(busy: bool, input_state: &str, input_reason: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("busy".to_string(), Value::Bool(busy));
        m.insert(
            "inputState".to_string(),
            Value::String(input_state.to_string()),
        );
        m.insert(
            "inputReason".to_string(),
            Value::String(input_reason.to_string()),
        );
        m
    }
    match status {
        "busy" => fields(true, "ready", ""),
        "waiting" => fields(
            false,
            "waiting_user",
            &format!(
                "registry:{}",
                if waiting_for.is_empty() {
                    "unknown"
                } else {
                    waiting_for
                }
            ),
        ),
        "idle" | "shell" => fields(false, "ready", ""),
        _ => fields(false, "unknown", "no_registry_status"),
    }
}

/// The inbox socket of the Claude session hosting this process ("" when this
/// process is not a child of one).
pub fn own_socket() -> String {
    env::var("CLAUDE_CODE_MESSAGING_SOCKET").unwrap_or_default()
}

/// The registry entry of the Claude session this process runs inside.
///
/// Identity is the socket, never a saved slot: whichever live registration
/// names this process's own inbox is us.
pub fn self_session() -> Option<ClaudeSession> {
    let sock = own_socket();
    if sock.is_empty() {
        return None;
    }
    list_sessions().into_iter().find(|s| s.socket_path == sock)
}

/// Every live session answering to *label* — its Claude Code name (what
/// `/list-agents` shows), its desktop title, or its pid. The caller decides
/// on >1.
pub fn resolve(label: &str) -> Vec<ClaudeSession> {
    list_sessions()
        .into_iter()
        .filter(|s| s.answers_to(label))
        .collect()
}

/// Ask the session on *sock_path* to take *name* as its own.
///
/// A `control/rename` frame is handled at dispatch — immediately, busy or
/// idle — and never touches the composer or the transcript. *session_id*,
/// when given, must match the target's own id or the frame is silently
/// dropped: the guard against a recycled socket path renaming a stranger.
/// True means the frame was written, not that the name changed — the caller
/// confirms against the registry.
pub fn rename(sock_path: &str, name: &str, session_id: &str) -> bool {
    if sock_path.is_empty() || name.is_empty() {
        return false;
    }
    let mut payload = json!({"type": "control", "action": "rename", "name": name});
    if !session_id.is_empty() {
        payload["session_id"] = json!(session_id);
    }
    // ponytail: std has no AF_UNIX connect timeout; connect is
    // instant-or-refused, so CONNECT_TIMEOUT only shapes SUBMIT_TIMEOUT.
    let Ok(mut conn) = UnixStream::connect(sock_path) else {
        return false;
    };
    let _ = conn.set_write_timeout(Some(write_timeout()));
    conn.write_all(format!("{payload}\n").as_bytes()).is_ok()
}

/// Queue *text* for the session listening on *sock_path*.
///
/// Returns `ACCEPTED_UDS_WRITE`; `WRITE_TIMED_OUT` when the session accepted
/// the connection but did not read the frame in time; or `None` when nothing
/// is listening. `priority: next` folds a mid-turn arrival into the running
/// turn at the next tool boundary. *sender* is what the receiving session
/// shows as the message's origin. *session_id*, when given, must match the
/// target's own id or the frame is silently dropped — the guard against a
/// recycled `<pid>.sock` taking a dead session's mail.
pub fn send(sock_path: &str, text: &str, sender: &str, session_id: &str) -> Option<&'static str> {
    send_with_write_timeout(sock_path, text, sender, session_id, write_timeout())
}

fn send_with_write_timeout(
    sock_path: &str,
    text: &str,
    sender: &str,
    session_id: &str,
    timeout: Duration,
) -> Option<&'static str> {
    if sock_path.is_empty() {
        return None;
    }
    let mut frame = json!({
        "type": "user",
        "priority": "next",
        "from": sender,
        "message": {"role": "user", "content": peer_card_envelope(sender, text)},
    });
    if !session_id.is_empty() {
        frame["session_id"] = json!(session_id);
    }
    let mut conn = match UnixStream::connect(sock_path) {
        Ok(c) => c,
        Err(_) => return None,
    };
    let _ = conn.set_write_timeout(Some(timeout));
    match conn.write_all(format!("{frame}\n").as_bytes()) {
        Ok(()) => Some(ACCEPTED_UDS_WRITE),
        Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
            Some(WRITE_TIMED_OUT)
        }
        Err(_) => None,
    }
}

/// The tag Claude Code's own `SendMessage` wraps a cross-session body in.
const PEER_TAG: &str = "cross-session-message";

/// *text* as the receiving Claude Code draws it like one of its own peer
/// messages: `<cross-session-message from="…">\n<body>\n</cross-session-message>`.
///
/// The receiver's message card (terminal `UserCrossSessionMessage`, the
/// desktop's peer card; observed on 2.1.263) parses the row's text for this
/// exact shape and, when it parses, draws `@ <from>` over the inner body
/// alone — the "Another Claude session sent a message" lead line and the
/// safety paragraph the receiver appends stay out of view. A body it cannot
/// parse (a bare `<HIVE>` envelope) is drawn whole, wrapper included. Only
/// the display changes: the model still reads the receiver's wrapper, and
/// the frame's `from` field (the origin) still names the sender, which the
/// desktop card requires to equal the tag's `from`. The parse is strict, so
/// the shape follows the receiver's own builder: `from` restricted to
/// `[A-Za-z0-9%:_/.\-]` with everything else percent-encoded, one `\n`
/// either side of the body, and a `<` opening the closing tag inside the
/// body spelled `<\` so the body cannot end the wrapper early.
pub fn peer_card_envelope(sender: &str, text: &str) -> String {
    format!(
        "<{PEER_TAG} from=\"{}\">\n{}\n</{PEER_TAG}>",
        peer_from_attr(sender),
        escape_peer_body(text)
    )
}

fn peer_from_attr(sender: &str) -> String {
    let mut out = String::with_capacity(sender.len());
    for b in sender.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b':' | b'_' | b'/' | b'.' | b'-' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ponytail: the receiver's escaper also catches lookalike glyphs and filler
// characters spelling the closing tag; a hive body is an envelope plus a
// member's prose, so only the literal spelling is covered here. A body that
// slips past leaves the card drawing the whole row, never a lost message.
fn escape_peer_body(text: &str) -> String {
    let close = format!("</{PEER_TAG}");
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut from = 0;
    while let Some(i) = lower[from..].find(&close) {
        let at = from + i;
        let end = at + close.len();
        // the receiver's pattern ends the tag name here: `</cross-session-messages`
        // is some other tag and stays as written
        let name_goes_on = text[end..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !name_goes_on {
            out.push_str(&text[last..at]);
            out.push_str("<\\");
            last = at + 1;
        }
        from = end;
    }
    out.push_str(&text[last..]);
    out
}

fn daemon_control_sock() -> PathBuf {
    // The supervisor daemon namespaces itself by config dir: sha256 of the
    // resolved path, first 8 hex (observed on 2.1.240). /tmp is fixed — the
    // daemon does not honour $TMPDIR.
    let cfg = config_dir();
    let abs = if cfg.is_absolute() {
        cfg
    } else {
        env::current_dir().unwrap_or_default().join(cfg)
    };
    let mut hasher = Sha256::new();
    hasher.update(abs.to_string_lossy().as_bytes());
    let ns: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .chars()
        .take(8)
        .collect();
    PathBuf::from("/tmp")
        .join(format!("cc-daemon-{}", unsafe { libc::getuid() }))
        .join(ns)
        .join("control.sock")
}

fn daemon_control_key() -> String {
    fs::read_to_string(config_dir().join("daemon").join("control.key"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn daemon_roundtrip(sock_path: &Path, frame: &Value) -> Option<Map<String, Value>> {
    let mut conn = UnixStream::connect(sock_path).ok()?;
    let _ = conn.set_write_timeout(Some(write_timeout()));
    let _ = conn.set_read_timeout(Some(write_timeout()));
    conn.write_all(format!("{frame}\n").as_bytes()).ok()?;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 65536];
    while !buf.ends_with(b"\n") {
        match conn.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
    }
    match serde_json::from_slice::<Value>(&buf) {
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// Hand *text* to the claude supervisor daemon as the job's own input.
///
/// `op: "reply"` routes to the worker's reply channel: the engine enqueues it
/// `origin: {kind: "human"}` / `priority: next` — the typed-keystroke lane,
/// so it lands with no peer wrapper in any state. The job is addressed by
/// `short` — the first 8 hex of its session id. Returns
/// `ACCEPTED_DAEMON_REPLY`, or `None` when this lane is unavailable (no
/// daemon, unknown job, retries exhausted) — the caller falls back to the
/// inbox-socket lane, which still delivers.
pub fn daemon_reply(session_id: &str, text: &str) -> Option<&'static str> {
    if session_id.chars().count() < 8 {
        return None; // mirrored early: computing the sock path reads the env
    }
    daemon_reply_via(
        session_id,
        text,
        &daemon_control_sock(),
        Duration::from_secs_f64(DAEMON_RETRY_DELAY),
    )
}

fn daemon_reply_via(
    session_id: &str,
    text: &str,
    sock_path: &Path,
    retry_delay: Duration,
) -> Option<&'static str> {
    let short: String = session_id.chars().take(8).collect();
    if short.chars().count() != 8 {
        return None;
    }
    let auth = daemon_control_key();
    if auth.is_empty() {
        return None;
    }
    let mut frame = json!({
        "proto": DAEMON_PROTO,
        "op": "reply",
        "short": short,
        "auth": auth,
        "text": text,
    });
    let mut reauthed = false;
    for _ in 0..DAEMON_RETRY_LIMIT {
        let resp = daemon_roundtrip(sock_path, &frame)?;
        if resp.get("ok") == Some(&Value::Bool(true)) {
            return Some(ACCEPTED_DAEMON_REPLY);
        }
        let code = resp.get("code").and_then(Value::as_str).unwrap_or("");
        if code == "EAUTH" && !reauthed {
            // One re-read: the daemon may have rotated the key under us.
            reauthed = true;
            let auth = daemon_control_key();
            if auth.is_empty() {
                return None;
            }
            frame["auth"] = json!(auth);
            continue;
        }
        if DAEMON_RETRY_CODES.contains(&code) {
            std::thread::sleep(retry_delay);
            continue;
        }
        return None;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::{EnvGuard, CLAUDE_VARS};
    use std::os::unix::net::UnixListener;
    use std::thread::JoinHandle;

    fn short_tmp() -> tempfile::TempDir {
        // AF_UNIX sun_path caps near 104 bytes: sockets cannot live under a
        // long tmp path (the same reason production sockets live under
        // $HIVE_HOME).
        let base = if Path::new("/tmp").is_dir() {
            PathBuf::from("/tmp")
        } else {
            env::temp_dir()
        };
        tempfile::Builder::new()
            .prefix("hive-cs-")
            .tempdir_in(base)
            .unwrap()
    }

    fn write_entry(root: &Path, fname: &str, fields: Value) {
        let dir = root.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(fname), fields.to_string()).unwrap();
    }

    fn dead_pid() -> i32 {
        // a pid nothing is using, by the adapter's own liveness rule
        let mut pid = 4_000_000;
        while pid_alive(pid) {
            pid += 1;
        }
        pid
    }

    fn me() -> i32 {
        std::process::id() as i32
    }

    fn write_transcript(root: &Path, slug: &str, session_id: &str, lines: &[String]) {
        let d = root.join("projects").join(slug);
        fs::create_dir_all(&d).unwrap();
        fs::write(
            d.join(format!("{session_id}.jsonl")),
            format!("{}\n", lines.join("\n")),
        )
        .unwrap();
    }

    #[test]
    fn test_list_sessions_keeps_only_live_entries_with_an_inbox() {
        let mut guard = EnvGuard::cleared(&CLAUDE_VARS);
        let tmp = tempfile::tempdir().unwrap();
        guard.set("CLAUDE_CONFIG_DIR", tmp.path());
        let me = me();
        write_entry(
            tmp.path(),
            "1.json",
            json!({"name": "alpha", "pid": me, "cwd": "/w/a", "kind": "interactive", "messagingSocketPath": "/tmp/a.sock"}),
        );
        write_entry(
            tmp.path(),
            "2.json",
            json!({"name": "dead", "pid": dead_pid(), "cwd": "/w/d", "kind": "interactive", "messagingSocketPath": "/tmp/d.sock"}),
        );
        write_entry(
            tmp.path(),
            "3.json",
            json!({"name": "nosock", "pid": me, "cwd": "/w/n", "kind": "interactive"}),
        );
        write_entry(
            tmp.path(),
            "4.json",
            json!({"name": "", "pid": me, "messagingSocketPath": "/tmp/x.sock"}),
        );
        fs::write(tmp.path().join("sessions").join("5.json"), "{not json").unwrap();
        fs::write(tmp.path().join("sessions").join("6.json"), "[1, 2]").unwrap();
        write_entry(
            tmp.path(),
            "7.json",
            json!({"name": "spare", "pid": me, "cwd": "/w/s", "kind": "interactive", "messagingSocketPath": "/tmp/s.sock", "spare": true}),
        );

        let rows = list_sessions();

        let seen: Vec<_> = rows
            .iter()
            .map(|s| {
                (
                    s.name.as_str(),
                    s.pid,
                    s.cwd.as_str(),
                    s.socket_path.as_str(),
                )
            })
            .collect();
        assert_eq!(seen, vec![("alpha", me, "/w/a", "/tmp/a.sock")]);
        assert!(resolve("spare").is_empty()); // a warm spare is nobody's address
        assert_eq!(
            resolve("alpha")
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha"]
        );
        assert!(resolve("nosock").is_empty());
        assert!(resolve("dead").is_empty());
    }

    #[test]
    fn test_sessions_carry_the_desktop_title_and_answer_to_it() {
        // the desktop title lives in the transcript as a `custom-title`
        // record; the registry only knows the sessionId — join them so
        // `hive msg` accepts what the human actually sees in the sidebar
        let mut guard = EnvGuard::cleared(&CLAUDE_VARS);
        let tmp = tempfile::tempdir().unwrap();
        guard.set("CLAUDE_CONFIG_DIR", tmp.path());
        let me = me();
        write_entry(
            tmp.path(),
            "1.json",
            json!({"name": "nice-almeida-dd", "pid": me, "cwd": "/w/a", "messagingSocketPath": "/tmp/a.sock", "sessionId": "sid-a"}),
        );
        write_entry(
            tmp.path(),
            "2.json",
            json!({"name": "plain-b", "pid": me, "cwd": "/w/b", "messagingSocketPath": "/tmp/b.sock", "sessionId": "sid-b"}),
        );
        write_transcript(
            tmp.path(),
            "-w-a",
            "sid-a",
            &[
                json!({"type": "custom-title", "customTitle": "old title", "sessionId": "sid-a"})
                    .to_string(),
                json!({"type": "user", "message": {"role": "user", "content": "hi"}}).to_string(),
                json!({"type": "custom-title", "customTitle": "PR70 审查", "sessionId": "sid-a"})
                    .to_string(),
            ],
        );
        write_transcript(
            tmp.path(),
            "-w-b",
            "sid-b",
            &[json!({"type": "user", "message": {"role": "user", "content": "x"}}).to_string()],
        );

        let sessions = list_sessions();
        let by_name = |n: &str| sessions.iter().find(|s| s.name == n).unwrap().clone();
        assert_eq!(by_name("nice-almeida-dd").title, "PR70 审查"); // the latest record wins
        assert_eq!(by_name("plain-b").title, "");
        assert_eq!(
            resolve("PR70 审查")
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["nice-almeida-dd"]
        );
        assert_eq!(
            resolve("nice-almeida-dd")
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["nice-almeida-dd"]
        );
        assert!(resolve("old title").is_empty());
    }

    #[test]
    fn test_session_title_scans_a_long_transcript_from_the_tail() {
        let mut guard = EnvGuard::cleared(&CLAUDE_VARS);
        let tmp = tempfile::tempdir().unwrap();
        guard.set("CLAUDE_CONFIG_DIR", tmp.path());
        let filler =
            json!({"type": "assistant", "message": {"content": "x".repeat(4000)}}).to_string();
        let mut lines = vec![
            json!({"type": "custom-title", "customTitle": "first", "sessionId": "sid-l"})
                .to_string(),
        ];
        lines.extend(std::iter::repeat_n(filler.clone(), 300)); // ~1.2 MB, well past the tail window
        lines.push(
            json!({"type": "custom-title", "customTitle": "current", "sessionId": "sid-l"})
                .to_string(),
        );
        lines.extend(std::iter::repeat_n(filler.clone(), 3));
        write_transcript(tmp.path(), "-w-l", "sid-l", &lines);
        assert_eq!(session_title("sid-l"), "current");
        // a title set only at the start of a long session is still found
        let mut lines2 = vec![
            json!({"type": "custom-title", "customTitle": "early", "sessionId": "sid-e"})
                .to_string(),
        ];
        lines2.extend(std::iter::repeat_n(filler, 300));
        write_transcript(tmp.path(), "-w-e", "sid-e", &lines2);
        assert_eq!(session_title("sid-e"), "early");
        assert_eq!(session_title(""), "");
        assert_eq!(session_title("sid-missing"), "");
    }

    #[test]
    fn test_list_sessions_without_registry_dir_is_empty() {
        let mut guard = EnvGuard::cleared(&CLAUDE_VARS);
        let tmp = tempfile::tempdir().unwrap();
        guard.set("CLAUDE_CONFIG_DIR", tmp.path().join("missing"));
        assert!(list_sessions().is_empty());
    }

    #[test]
    fn test_registry_follows_claude_home_first() {
        // CLAUDE_HOME is hive's sandbox lever: a dev lane must never
        // enumerate (or message) the developer's real sessions
        let mut guard = EnvGuard::cleared(&CLAUDE_VARS);
        let tmp = tempfile::tempdir().unwrap();
        guard.set("CLAUDE_HOME", tmp.path().join("lane"));
        guard.set("CLAUDE_CONFIG_DIR", tmp.path().join("real"));
        write_entry(
            &tmp.path().join("real"),
            "1.json",
            json!({"name": "real", "pid": me(), "messagingSocketPath": "/tmp/r.sock"}),
        );
        write_entry(
            &tmp.path().join("lane"),
            "2.json",
            json!({"name": "lane", "pid": me(), "messagingSocketPath": "/tmp/l.sock"}),
        );
        assert_eq!(
            list_sessions()
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["lane"]
        );
        guard.remove("CLAUDE_HOME");
        assert_eq!(
            list_sessions()
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["real"]
        );
    }

    #[test]
    fn test_sessions_answer_to_their_pid() {
        let mut guard = EnvGuard::cleared(&CLAUDE_VARS);
        let tmp = tempfile::tempdir().unwrap();
        guard.set("CLAUDE_CONFIG_DIR", tmp.path());
        let me = me();
        write_entry(
            tmp.path(),
            "1.json",
            json!({"name": "worker", "pid": me, "cwd": "/w/1", "messagingSocketPath": "/tmp/1.sock"}),
        );
        assert_eq!(
            resolve(&me.to_string())
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["worker"]
        );
    }

    #[test]
    fn test_a_cleared_desktop_title_is_forgotten() {
        let mut guard = EnvGuard::cleared(&CLAUDE_VARS);
        let tmp = tempfile::tempdir().unwrap();
        guard.set("CLAUDE_CONFIG_DIR", tmp.path());
        write_entry(
            tmp.path(),
            "1.json",
            json!({"name": "n", "pid": me(), "messagingSocketPath": "/tmp/n.sock", "sessionId": "sid-c"}),
        );
        write_transcript(
            tmp.path(),
            "-w-c",
            "sid-c",
            &[
                json!({"type": "custom-title", "customTitle": "was named", "sessionId": "sid-c"})
                    .to_string(),
                json!({"type": "custom-title", "customTitle": "", "sessionId": "sid-c"})
                    .to_string(),
            ],
        );
        assert_eq!(list_sessions()[0].title, "");
        assert!(resolve("was named").is_empty());
    }

    #[test]
    fn test_resolve_returns_every_live_session_sharing_a_name() {
        let mut guard = EnvGuard::cleared(&CLAUDE_VARS);
        let tmp = tempfile::tempdir().unwrap();
        guard.set("CLAUDE_CONFIG_DIR", tmp.path());
        let me = me();
        write_entry(
            tmp.path(),
            "1.json",
            json!({"name": "worker", "pid": me, "cwd": "/w/1", "messagingSocketPath": "/tmp/1.sock"}),
        );
        write_entry(
            tmp.path(),
            "2.json",
            json!({"name": "worker", "pid": me, "cwd": "/w/2", "messagingSocketPath": "/tmp/2.sock"}),
        );
        let mut cwds: Vec<String> = resolve("worker").iter().map(|s| s.cwd.clone()).collect();
        cwds.sort();
        assert_eq!(cwds, ["/w/1", "/w/2"]);
    }

    /// A throwaway inbox listener on *path*; the handle yields the one frame
    /// it read.
    fn spawn_listener(path: &str) -> JoinHandle<Vec<u8>> {
        let srv = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let (mut conn, _) = srv.accept().unwrap();
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.ends_with(b"\n") {
                match conn.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
            }
            buf
        })
    }

    #[test]
    fn test_send_writes_one_peer_message_line_and_reports_acceptance() {
        let tmp = short_tmp();
        let path = tmp.path().join("s.sock");
        let handle = spawn_listener(path.to_str().unwrap());

        assert_eq!(
            send(path.to_str().unwrap(), "hello there", "t.w", ""),
            Some(ACCEPTED_UDS_WRITE)
        );
        let got = handle.join().unwrap();

        let frame: Value = serde_json::from_slice(&got).unwrap();
        assert_eq!(
            frame,
            json!({
                "type": "user",
                "priority": "next",
                "from": "t.w",
                "message": {
                    "role": "user",
                    "content": "<cross-session-message from=\"t.w\">\nhello there\n</cross-session-message>",
                },
            })
        );
    }

    #[test]
    fn test_peer_card_envelope_follows_the_receivers_own_shape() {
        // the exact bytes claude's SendMessage writes for the same sender and
        // body: this is what the receiver's card parses, and a byte off falls
        // back to drawing the whole wrapped row
        assert_eq!(
            peer_card_envelope("hornet.sage", "<HIVE from=hornet.sage to=hornet.orch>\nhi\n</HIVE>"),
            "<cross-session-message from=\"hornet.sage\">\n<HIVE from=hornet.sage to=hornet.orch>\nhi\n</HIVE>\n</cross-session-message>"
        );
    }

    #[test]
    fn test_peer_card_envelope_percent_encodes_the_sender_and_escapes_a_closing_tag_in_the_body() {
        assert_eq!(peer_from_attr("ccd.my session#1"), "ccd.my%20session%231");
        assert_eq!(peer_from_attr("hornet.sage"), "hornet.sage");
        assert_eq!(
            escape_peer_body("a </cross-session-message> b </CROSS-SESSION-MESSAGE>"),
            "a <\\/cross-session-message> b <\\/CROSS-SESSION-MESSAGE>"
        );
        assert_eq!(escape_peer_body("<HIVE>\nx\n</HIVE>"), "<HIVE>\nx\n</HIVE>");
        // a longer tag name is another tag: the receiver's escaper leaves it
        assert_eq!(
            escape_peer_body("</cross-session-message-extra> </cross-session-messageX>"),
            "</cross-session-message-extra> </cross-session-messageX>"
        );
        assert_eq!(
            escape_peer_body("</cross-session-message"),
            "<\\/cross-session-message"
        );
    }

    #[test]
    fn test_send_carries_the_session_id_guard_only_when_given() {
        // claude drops a frame whose session_id is not the target's own: that
        // is what keeps a recycled `<pid>.sock` from taking a dead session's
        // mail. With no id there is no guard — the frame must not carry an
        // empty one.
        let tmp = short_tmp();
        let path = tmp.path().join("g.sock");
        let handle = spawn_listener(path.to_str().unwrap());
        assert_eq!(
            send(path.to_str().unwrap(), "x", "t.w", "sid-1"),
            Some(ACCEPTED_UDS_WRITE)
        );
        let frame: Value = serde_json::from_slice(&handle.join().unwrap()).unwrap();
        assert_eq!(frame["session_id"], json!("sid-1"));

        let path = tmp.path().join("n.sock");
        let handle = spawn_listener(path.to_str().unwrap());
        assert_eq!(
            send(path.to_str().unwrap(), "x", "t.w", ""),
            Some(ACCEPTED_UDS_WRITE)
        );
        let frame: Value = serde_json::from_slice(&handle.join().unwrap()).unwrap();
        assert!(frame.get("session_id").is_none());
    }

    #[test]
    fn test_send_to_a_dead_socket_is_none() {
        let tmp = short_tmp();
        assert_eq!(
            send(
                tmp.path().join("gone.sock").to_str().unwrap(),
                "x",
                "hive",
                ""
            ),
            None
        );
        assert_eq!(send("", "x", "hive", ""), None);
    }

    #[test]
    fn test_send_to_a_listener_that_never_reads_times_out_distinctly() {
        // accepted-but-stalled is reported apart from absent: the CLI words
        // them differently, and the second one may have left a truncated
        // frame behind
        let tmp = short_tmp();
        let path = tmp.path().join("stall.sock");
        let _srv = UnixListener::bind(&path).unwrap();
        // nobody calls accept()/recv(): the kernel accepts the connect into
        // the backlog and the socket buffers fill on a large enough frame
        assert_eq!(
            send_with_write_timeout(
                path.to_str().unwrap(),
                &"x".repeat(4_000_000),
                "hive",
                "",
                Duration::from_secs_f64(0.3),
            ),
            Some(WRITE_TIMED_OUT)
        );
    }

    #[test]
    fn test_self_session_is_identified_by_its_own_socket() {
        // identity is the socket, never a saved slot: whichever live
        // registration names this process's own inbox is us
        let mut guard = EnvGuard::cleared(&CLAUDE_VARS);
        let tmp = tempfile::tempdir().unwrap();
        guard.set("CLAUDE_CONFIG_DIR", tmp.path());
        let me = me();
        write_entry(
            tmp.path(),
            "1.json",
            json!({"name": "mine", "pid": me, "messagingSocketPath": "/tmp/mine.sock"}),
        );
        write_entry(
            tmp.path(),
            "2.json",
            json!({"name": "other", "pid": me, "messagingSocketPath": "/tmp/other.sock"}),
        );
        guard.set("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/mine.sock");
        assert_eq!(self_session().unwrap().name, "mine");
        guard.set("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/ghost.sock");
        assert!(self_session().is_none());
        guard.remove("CLAUDE_CODE_MESSAGING_SOCKET");
        assert!(self_session().is_none());
    }

    #[test]
    fn test_session_status_reports_only_live_tui_vocabulary() {
        let mut guard = EnvGuard::cleared(&CLAUDE_VARS);
        let tmp = tempfile::tempdir().unwrap();
        guard.set("CLAUDE_CONFIG_DIR", tmp.path());
        let me = me();
        write_entry(
            tmp.path(),
            &format!("{me}.json"),
            json!({"name": "w", "pid": me, "kind": "interactive", "status": "waiting", "waitingFor": "input needed"}),
        );
        assert_eq!(
            session_status(Some(me)),
            Some(("waiting".to_string(), "input needed".to_string()))
        );

        write_entry(
            tmp.path(),
            &format!("{me}.json"),
            json!({"name": "w", "pid": me, "kind": "interactive", "status": "busy"}),
        );
        assert_eq!(
            session_status(Some(me)),
            Some(("busy".to_string(), String::new()))
        );

        // `shell` is in the registry's own vocabulary — dropping it made a
        // session at its shell read as "nothing reported" and fall into the
        // transcript gate
        write_entry(
            tmp.path(),
            &format!("{me}.json"),
            json!({"name": "w", "pid": me, "kind": "interactive", "status": "shell"}),
        );
        assert_eq!(
            session_status(Some(me)),
            Some(("shell".to_string(), String::new()))
        );

        // headless/desktop-hosted sessions never report status
        write_entry(
            tmp.path(),
            &format!("{me}.json"),
            json!({"name": "w", "pid": me, "kind": "interactive"}),
        );
        assert_eq!(session_status(Some(me)), None);
        // unknown vocabulary is not trusted
        write_entry(
            tmp.path(),
            &format!("{me}.json"),
            json!({"name": "w", "pid": me, "status": "warming"}),
        );
        assert_eq!(session_status(Some(me)), None);
        // dead process / missing entry / no pid
        let dead = dead_pid();
        write_entry(
            tmp.path(),
            &format!("{dead}.json"),
            json!({"name": "w", "pid": dead, "status": "idle"}),
        );
        assert_eq!(session_status(Some(dead)), None);
        assert_eq!(session_status(Some(me + 1)), None);
        assert_eq!(session_status(None), None);
    }

    #[test]
    fn test_runtime_from_status_maps_the_registry_vocabulary() {
        assert_eq!(
            Value::Object(runtime_from_status("busy", "")),
            json!({"busy": true, "inputState": "ready", "inputReason": ""})
        );
        assert_eq!(
            Value::Object(runtime_from_status("idle", "")),
            json!({"busy": false, "inputState": "ready", "inputReason": ""})
        );
        // at its shell: not mid-turn, and not waiting on an answer either
        assert_eq!(
            Value::Object(runtime_from_status("shell", "")),
            json!({"busy": false, "inputState": "ready", "inputReason": ""})
        );
        assert_eq!(
            Value::Object(runtime_from_status("waiting", "input needed")),
            json!({"busy": false, "inputState": "waiting_user", "inputReason": "registry:input needed"})
        );
        assert_eq!(
            runtime_from_status("waiting", "")["inputReason"],
            json!("registry:unknown")
        );
        assert_eq!(runtime_from_status("", "")["inputState"], json!("unknown"));
    }

    /// A throwaway daemon control socket: answers one JSON line per
    /// connection from *replies* in order; the handle yields the received
    /// frames.
    fn control_server(path: &Path, replies: Vec<Value>) -> JoinHandle<Vec<Value>> {
        let srv = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let mut got: Vec<Value> = Vec::new();
            for reply in replies {
                let Ok((mut conn, _)) = srv.accept() else {
                    return got;
                };
                let mut buf: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 65536];
                while !buf.ends_with(b"\n") {
                    match conn.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(_) => break,
                    }
                }
                got.push(serde_json::from_slice(&buf).unwrap());
                conn.write_all(format!("{reply}\n").as_bytes()).unwrap();
            }
            got
        })
    }

    fn wire_daemon(
        replies: Vec<Value>,
    ) -> (EnvGuard, tempfile::TempDir, PathBuf, JoinHandle<Vec<Value>>) {
        let mut env_guard = EnvGuard::cleared(&CLAUDE_VARS);
        let tmp = short_tmp();
        let sock_path = tmp.path().join("control.sock");
        let key = tmp.path().join("daemon").join("control.key");
        fs::create_dir_all(key.parent().unwrap()).unwrap();
        fs::write(&key, "k3y\n").unwrap();
        env_guard.set("CLAUDE_HOME", tmp.path());
        let handle = control_server(&sock_path, replies);
        (env_guard, tmp, sock_path, handle)
    }

    const FAST_RETRY: Duration = Duration::from_millis(10);

    #[test]
    fn test_daemon_reply_sends_the_exact_frame_and_reports_acceptance() {
        let (_env, _tmp, sock, handle) = wire_daemon(vec![json!({"ok": true, "op": "reply"})]);
        let out = daemon_reply_via(
            "a65300e6-fed7-460f-ae17-9a94752d6fce",
            "<HIVE>hi</HIVE>",
            &sock,
            FAST_RETRY,
        );
        let got = handle.join().unwrap();
        assert_eq!(out, Some(ACCEPTED_DAEMON_REPLY));
        assert_eq!(
            got,
            vec![json!({
                "proto": 1,
                "op": "reply",
                "short": "a65300e6",
                "auth": "k3y",
                "text": "<HIVE>hi</HIVE>",
            })]
        );
    }

    #[test]
    fn test_daemon_reply_retries_readiness_codes_then_lands() {
        let (_env, _tmp, sock, handle) = wire_daemon(vec![
            json!({"ok": false, "code": "ESTARTING"}),
            json!({"ok": false, "code": "ERESPAWNING"}),
            json!({"ok": true, "op": "reply"}),
        ]);
        let out = daemon_reply_via("a65300e6-0000", "ping", &sock, FAST_RETRY);
        let got = handle.join().unwrap();
        assert_eq!(out, Some(ACCEPTED_DAEMON_REPLY));
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn test_daemon_reply_does_not_retry_a_terminal_code() {
        let (_env, _tmp, sock, handle) = wire_daemon(vec![json!({"ok": false, "code": "ENOJOB"})]);
        let out = daemon_reply_via("a65300e6-0000", "ping", &sock, FAST_RETRY);
        let got = handle.join().unwrap();
        assert_eq!(out, None);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn test_daemon_reply_rereads_the_key_once_on_eauth() {
        let (_env, _tmp, sock, handle) = wire_daemon(vec![
            json!({"ok": false, "code": "EAUTH"}),
            json!({"ok": false, "code": "EAUTH"}),
        ]);
        let out = daemon_reply_via("a65300e6-0000", "ping", &sock, FAST_RETRY);
        let got = handle.join().unwrap();
        assert_eq!(out, None);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn test_daemon_reply_without_a_daemon_is_none() {
        let mut guard = EnvGuard::cleared(&CLAUDE_VARS);
        let tmp = short_tmp();
        fs::create_dir_all(tmp.path().join("daemon")).unwrap();
        fs::write(tmp.path().join("daemon").join("control.key"), "k3y").unwrap();
        guard.set("CLAUDE_HOME", tmp.path());
        assert_eq!(
            daemon_reply_via(
                "a65300e6-0000",
                "ping",
                &tmp.path().join("no.sock"),
                FAST_RETRY
            ),
            None
        );
    }

    #[test]
    fn test_daemon_reply_rejects_a_short_session_id() {
        assert_eq!(daemon_reply("abc", "ping"), None);
        assert_eq!(daemon_reply("", "ping"), None);
    }

    #[test]
    fn test_daemon_control_sock_derives_from_the_config_dir() {
        let mut guard = EnvGuard::cleared(&CLAUDE_VARS);
        let tmp = tempfile::tempdir().unwrap();
        guard.set("CLAUDE_HOME", tmp.path());
        let mut hasher = Sha256::new();
        hasher.update(tmp.path().to_string_lossy().as_bytes());
        let ns: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
            .chars()
            .take(8)
            .collect();
        assert_eq!(
            daemon_control_sock(),
            PathBuf::from("/tmp")
                .join(format!("cc-daemon-{}", unsafe { libc::getuid() }))
                .join(ns)
                .join("control.sock")
        );
    }
}
