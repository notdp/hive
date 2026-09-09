//! A terminal hclaude's viewer belongs to its launcher until create/join.
//!
//! One authenticated connection asks that launcher to stop its own child.
//! The roster write commits the transfer; an EOF before it restores the old
//! viewer, an EOF after it finishes the transfer. The launcher never infers
//! a handoff from a viewer exit, and a committed job never reopens outside
//! the team's pane. Team membership remains in the registry.

mod session;
mod terminal;
pub(crate) use session::Session;

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::adapters::claude_bg;
use crate::adapters::claude_sessions::ClaudeSession;
use crate::json_fields::map_str;
use crate::shell::shlex_quote;
use crate::{registry, tmux};

const FRAME_LIMIT: usize = 32 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

fn read_frame(reader: &mut BufReader<UnixStream>) -> Result<Value> {
    let mut bytes = Vec::new();
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            bail!("launcher connection closed");
        }
        let end = chunk.iter().position(|b| *b == b'\n');
        let n = end.map_or(chunk.len(), |i| i + 1);
        if bytes.len() + n > FRAME_LIMIT {
            bail!("launcher frame is too large");
        }
        bytes.extend_from_slice(&chunk[..n]);
        reader.consume(n);
        if end.is_some() {
            return Ok(serde_json::from_slice(&bytes)?);
        }
    }
}

fn send_frame(stream: &mut UnixStream, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    Ok(())
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("handoff is missing {key}"))
}

/// A prepared pane belongs to this attempt until its member row commits.
/// The marker guards cleanup against a recycled tmux pane/window id.
#[derive(Clone, Debug)]
pub(crate) struct Target {
    pub team: String,
    pub member: String,
    pub created_at: String,
    pub pane: String,
    pub window: String,
    pub token: String,
    pub owns_window: bool,
    pub new_session: bool,
}

impl Target {
    fn json(&self) -> Value {
        json!({"team": self.team, "member": self.member, "createdAt": self.created_at,
            "pane": self.pane, "window": self.window, "token": self.token,
            "ownsWindow": self.owns_window, "newSession":self.new_session})
    }

    fn parse(value: &Value) -> Result<Self> {
        Ok(Self {
            team: field(value, "team")?.into(),
            member: field(value, "member")?.into(),
            created_at: field(value, "createdAt")?.into(),
            pane: field(value, "pane")?.into(),
            window: field(value, "window")?.into(),
            token: field(value, "token")?.into(),
            owns_window: value
                .get("ownsWindow")
                .and_then(Value::as_bool)
                .ok_or_else(|| anyhow!("handoff is missing ownsWindow"))?,
            new_session: value
                .get("newSession")
                .and_then(Value::as_bool)
                .ok_or_else(|| anyhow!("handoff is missing newSession"))?,
        })
    }

    pub(crate) fn mark(&self) -> Result<()> {
        tmux::run(
            &[
                "set-option",
                "-p",
                "-t",
                &self.pane,
                "@hive-handoff",
                &self.token,
            ],
            true,
            5,
        )?;
        Ok(())
    }

    fn owned(&self) -> bool {
        tmux::get_pane_option(&self.pane, "hive-handoff").as_deref() == Some(&self.token)
            && tmux::display_value(&self.pane, "#{window_id}").as_deref() == Some(&self.window)
    }

    pub(crate) fn committed(&self, session: &Session) -> bool {
        let Some(entry) = registry::load(&self.team) else {
            return false;
        };
        let Ok(actual) = map_str(&entry, "createdAt").parse::<f64>() else {
            return false;
        };
        if Some(actual) != self.created_at.parse::<f64>().ok() {
            return false;
        }
        entry
            .get("members")
            .and_then(Value::as_array)
            .is_some_and(|rows| {
                rows.iter().any(|r| {
                    r.get("name").and_then(Value::as_str) == Some(&self.member)
                        && r.get("sessionId").and_then(Value::as_str) == Some(&session.id)
                        && r.get("cli").and_then(Value::as_str) == Some(session.cli)
                })
            })
    }

    pub(crate) fn rollback(&self, session: &Session) {
        if self.committed(session) || !self.owned() {
            return;
        }
        session.clear_binding(self);
        crate::context::clear_context_for_pane(&self.pane);
        if self.owns_window {
            tmux::kill_window(&self.window);
        } else {
            tmux::kill_pane(&self.pane);
        }
    }
}

/// Connected to the original terminal, without interrupting its viewer yet.
pub(crate) struct Client {
    reader: BufReader<UnixStream>,
    pub session: Session,
}

impl Client {
    pub(crate) fn for_session(session: &ClaudeSession) -> Result<Option<Self>> {
        if session.kind != "bg" {
            return Ok(None);
        }
        let engine = claude_bg::engine_session_for_pid(session.pid as u32)
            .ok_or_else(|| anyhow!("this Claude background job is no longer live"))?;
        Self::connect(Session::claude(engine)).map(Some)
    }

    pub(crate) fn for_engine(cli: &str, id: &str) -> Result<Self> {
        let record = Self::record(cli, id)?;
        let session = Session::parse(&record["session"])?;
        if session.cli != cli || session.id != id {
            bail!("launcher record belongs to a different engine session");
        }
        Self::connect(session)
    }

    fn record(cli: &str, id: &str) -> Result<Value> {
        fs::read(Session::record_path(cli, id)?)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(|| {
                anyhow!("this {cli} session has no live hive launcher; resume it through h{cli}")
            })
    }

    fn connect(session: Session) -> Result<Self> {
        let record = Self::record(session.cli, &session.id)?;
        let stream = UnixStream::connect(field(&record, "socket")?)
            .context("the original terminal launcher is no longer reachable")?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        let mut reader = BufReader::new(stream);
        send_frame(
            reader.get_mut(),
            &json!({"op":"hello", "job":session.id,
            "cli":session.cli, "nonce":field(&record, "nonce")?}),
        )?;
        let reply = read_frame(&mut reader)?;
        if reply.get("ok") != Some(&Value::Bool(true)) {
            bail!("launcher refused: {}", reply);
        }
        Ok(Self { reader, session })
    }

    pub(crate) fn begin(&mut self, target: &Target) -> Result<()> {
        send_frame(
            self.reader.get_mut(),
            &json!({"op":"prepare", "target":target.json()}),
        )?;
        let reply = read_frame(&mut self.reader)?;
        if reply.get("ok") != Some(&Value::Bool(true)) {
            bail!("launcher could not release its viewer: {reply}");
        }
        Ok(())
    }

    pub(crate) fn commit(&mut self) -> Result<()> {
        send_frame(self.reader.get_mut(), &json!({"op":"commit"}))?;
        let reply = read_frame(&mut self.reader)?;
        if reply.get("ok") != Some(&Value::Bool(true)) {
            bail!("team committed, but its viewer did not start: {reply}");
        }
        Ok(())
    }
}

/// Launcher capability before membership exists. The held lock distinguishes
/// a live owner from a stale record; create/join also authenticates its socket.
pub(crate) fn launcher_registered(cli: &str, id: &str) -> bool {
    let Ok(path) = Session::record_path(cli, id) else {
        return false;
    };
    let Ok(record) = Client::record(cli, id) else {
        return false;
    };
    let Ok(session) = Session::parse(&record["session"]) else {
        return false;
    };
    if session.cli != cli || session.id != id {
        return false;
    }
    let Ok(lock) = File::open(path.with_extension("lock")) else {
        return false;
    };
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return false;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK)
}

struct Registration {
    path: PathBuf,
    socket_dir: PathBuf,
    nonce: String,
    _lock: File,
}

impl Registration {
    fn create(session: &Session) -> Result<(Self, UnixListener)> {
        let path = Session::record_path(session.cli, &session.id)?;
        let job = &session.id;
        fs::create_dir_all(path.parent().expect("record parent"))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path.with_extension("lock"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("another hive launcher already owns session {job}");
        }
        let socket_dir = crate::paths::mkdtemp_in(&std::env::temp_dir(), "hv-")?;
        let socket = socket_dir.join("s");
        let listener = match UnixListener::bind(&socket) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = fs::remove_dir_all(&socket_dir);
                return Err(error.into());
            }
        };
        listener.set_nonblocking(true)?;
        let nonce = crate::agent::uuid4();
        let registration = Self {
            path,
            socket_dir,
            nonce,
            _lock: lock,
        };
        let (mut file, pending) = crate::paths::mkstemp_in(
            registration.path.parent().expect("record parent"),
            "launch-",
            ".tmp",
        )?;
        let written = (|| -> Result<()> {
            serde_json::to_writer(
                &mut file,
                &json!({"job":job, "session":session.json(), "nonce":registration.nonce, "socket":socket, "pid":std::process::id()}),
            )?;
            fs::rename(&pending, &registration.path)?;
            Ok(())
        })();
        if written.is_err() {
            let _ = fs::remove_file(pending);
        }
        written?;
        Ok((registration, listener))
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir_all(&self.socket_dir);
    }
}

fn viewer(
    term: &terminal::Terminal,
    session: &Session,
    args: &[String],
) -> Result<terminal::Foreground> {
    term.spawn(&mut session.command(args))
}

// A new session gets the roots this launch uses. An empty value is not an
// unset one. Joining an existing team never rewrites its session environment.
const ROOTS: &[&str] = &[
    "HIVE_HOME",
    "CLAUDE_HOME",
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "GROK_HOME",
];
const MARKERS: &[&str] = &[
    "CODEX_THREAD_ID",
    "GROK_SESSION_ID",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_HOST_SESSION_ID",
];

pub(crate) fn set_session_roots(pane: &str) -> Result<()> {
    let sid = tmux::display_value(pane, "#{session_id}")
        .ok_or_else(|| anyhow!("handoff session disappeared"))?;
    for key in ROOTS {
        match std::env::var(key) {
            Ok(value) => {
                tmux::run(&["set-environment", "-t", &sid, key, &value], true, 5)?;
            }
            Err(_) => {
                tmux::run(&["set-environment", "-r", "-t", &sid, key], true, 5)?;
            }
        }
    }
    for key in MARKERS {
        tmux::run(&["set-environment", "-r", "-t", &sid, key], true, 5)?;
    }
    Ok(())
}

fn viewer_command(session: &Session) -> String {
    let mut args = vec!["env".to_string()];
    for key in ROOTS.iter().chain(MARKERS.iter()) {
        args.extend(["-u".into(), (*key).into()]);
    }
    for key in ROOTS.iter().copied().chain(["PATH"]) {
        if let Ok(value) = std::env::var(key) {
            args.push(shlex_quote(&format!("{key}={value}")));
        }
    }
    args.extend([
        shlex_quote(&crate::paths::self_exe()),
        session.team_viewer(),
    ]);
    args.join(" ")
}

pub(crate) fn recover_viewer(target: &Target, session: &Session) -> Result<()> {
    let path = Session::record_path(session.cli, &session.id)?.with_extension("viewer-lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // Both the launcher and the caller can recover a lost commit reply.
    // Only one starts the viewer; the other sees the cleared attempt mark.
    if target.committed(session)
        && session.binding_matches(target)
        && tmux::get_pane_option(&target.pane, "hive-handoff").is_none()
    {
        return Ok(());
    }
    if !target.owned() || !target.committed(session) || !session.binding_matches(target) {
        bail!("handoff target is no longer bound to this job");
    }
    // respawn reports whether the command could be scheduled, not whether
    // Claude rendered a frame. A later viewer failure leaves a committed
    // team recoverable through attach/resume, never a second local viewer.

    tmux::run(
        &[
            "respawn-pane",
            "-k",
            "-t",
            &target.pane,
            "-c",
            &session.cwd,
            &viewer_command(session),
        ],
        true,
        5,
    )?;
    tmux::run(
        &["set-option", "-pu", "-t", &target.pane, "@hive-handoff"],
        false,
        5,
    )?;
    Ok(())
}

fn transfer(
    stream: UnixStream,
    registration: &Registration,
    session: &Session,
    term: &terminal::Terminal,
    child: &mut terminal::Foreground,
) -> Result<Option<Target>> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut reader = BufReader::new(stream);
    let hello = read_frame(&mut reader)?;
    if hello.get("op").and_then(Value::as_str) != Some("hello")
        || hello.get("nonce").and_then(Value::as_str) != Some(&registration.nonce)
        || hello.get("job").and_then(Value::as_str) != Some(&session.id)
    {
        bail!("invalid launcher request");
    }
    send_frame(reader.get_mut(), &json!({"ok":true}))?;
    let prepare = read_frame(&mut reader)?;
    if prepare.get("op").and_then(Value::as_str) != Some("prepare") {
        bail!("expected a handoff request");
    }
    let target = Target::parse(&prepare["target"])?;
    if !target.owned() || target.committed(session) {
        bail!("handoff target is stale");
    }
    if child.poll()?.is_some() {
        bail!("the original viewer has already exited");
    }
    if target.new_session {
        set_session_roots(&target.pane)?;
    }
    reader.get_mut().set_read_timeout(None)?;
    child.stop()?;
    term.restore(true);
    eprintln!("hive: moving this conversation into team {}", target.team);
    // Once released, wait for an explicit commit or EOF. A timeout cannot
    // safely revoke a peer still writing the binding and registry row.
    let result =
        send_frame(reader.get_mut(), &json!({"ok":true})).and_then(|_| read_frame(&mut reader));
    if target.committed(session) {
        let started = recover_viewer(&target, session);
        let reply = match &started {
            Ok(()) => json!({"ok":true}),
            Err(error) => json!({"ok":false, "error":error.to_string()}),
        };
        let _ = send_frame(reader.get_mut(), &reply);
        if let Err(error) = started {
            eprintln!("hive: {error}; the team remains registered");
        }
        return Ok(Some(target));
    }
    target.rollback(session);
    let _ = result;
    Ok(None)
}

pub(crate) fn run(session: &Session, initial_args: &[String]) -> Result<i32> {
    let term = terminal::Terminal::capture()?;
    let (registration, listener) = Registration::create(session)?;
    let mut child = viewer(&term, session, initial_args)?;
    loop {
        if let Some(code) = child.poll()? {
            term.restore(false);
            return Ok(code);
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let transferred = transfer(stream, &registration, session, &term, &mut child);
                match transferred {
                    Ok(Some(target)) => {
                        drop(registration);
                        return attach_terminal(&term, &target);
                    }
                    Ok(None) => {
                        child = viewer(&term, session, &session.resume_args())?;
                    }
                    Err(error) => {
                        // Before release, the original child is still ours.
                        // After release, an EOF is handled above by the row.
                        eprintln!("hive: handoff did not complete: {error}");
                        if let Some(code) = child.poll()? {
                            return Ok(code);
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25))
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn attach_terminal(term: &terminal::Terminal, target: &Target) -> Result<i32> {
    let sid = tmux::display_value(&target.pane, "#{session_id}")
        .ok_or_else(|| anyhow!("team window disappeared; run `hive attach {}`", target.team))?;
    let result = term
        .spawn(
            Command::new("tmux")
                .args([
                    "attach-session",
                    "-E",
                    "-t",
                    &sid,
                    ";",
                    "select-window",
                    "-t",
                    &target.window,
                    ";",
                    "select-pane",
                    "-t",
                    &target.pane,
                ])
                .env_remove("TMUX")
                .env_remove("TMUX_PANE"),
        )
        .and_then(|mut child| child.wait());
    term.restore(false);
    match result {
        Ok(0) => Ok(0),
        other => {
            eprintln!(
                "hive: team {} is still available; run `hive attach {}`",
                target.team, target.team
            );
            other
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude(id: &str) -> Session {
        Session {
            cli: "claude",
            id: id.into(),
            cwd: "/tmp".into(),
            data: json!({"sessionId":"engine"}),
        }
    }

    #[test]
    fn test_launcher_lock_refuses_second_viewer_for_the_same_job() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = crate::testenv::EnvGuard::new();
        env.set("CLAUDE_HOME", tmp.path());
        let (first, _listener) = Registration::create(&claude("abc12345")).unwrap();
        assert!(Registration::create(&claude("abc12345")).is_err());
        assert!(launcher_registered("claude", "abc12345"));
        let stale = fs::read(&first.path).unwrap();
        let path = first.path.clone();
        drop(first);
        fs::write(&path, stale).unwrap();
        assert!(!launcher_registered("claude", "abc12345"));
        let (second, _) = Registration::create(&claude("abc12345")).unwrap();
        assert!(second.path.exists());
    }

    #[test]
    fn test_frames_keep_coalesced_messages_and_reject_oversized_input() {
        let (read, mut write) = UnixStream::pair().unwrap();
        send_frame(&mut write, &json!({"op":"hello"})).unwrap();
        send_frame(&mut write, &json!({"op":"prepare"})).unwrap();
        let mut reader = BufReader::new(read);
        assert_eq!(read_frame(&mut reader).unwrap()["op"], "hello");
        assert_eq!(read_frame(&mut reader).unwrap()["op"], "prepare");
        let writer = thread::spawn(move || {
            let _ = write.write_all(&vec![b'x'; FRAME_LIMIT + 1]);
        });
        assert!(read_frame(&mut reader).is_err());
        drop(reader);
        writer.join().unwrap();
    }

    #[test]
    fn test_target_commit_needs_the_same_team_instance_and_job() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = crate::testenv::EnvGuard::new();
        env.set("HIVE_HOME", tmp.path());
        let row = json!({"name":"orch","cli":"claude","sessionId":"abc12345"});
        registry::record_team(
            "test",
            "",
            "12.5",
            &[row.as_object().unwrap().clone()],
            "@1",
        )
        .unwrap();
        let mut target = Target {
            team: "test".into(),
            member: "orch".into(),
            created_at: "12.500".into(),
            pane: "%1".into(),
            window: "@1".into(),
            token: "token".into(),
            owns_window: true,
            new_session: true,
        };
        assert!(target.committed(&claude("abc12345")));
        assert!(!target.committed(&claude("abc12346")));
        target.created_at = "12.6".into();
        assert!(!target.committed(&claude("abc12345")));
        target.created_at = "not-a-number".into();
        assert!(!target.committed(&claude("abc12345")));
    }

    #[test]
    fn test_viewer_command_preserves_empty_and_unset_roots_without_shell_expansion() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let probe = tmp.path().join("probe with spaces");
        fs::write(&probe, "#!/bin/sh\nprintf '%s\\n' \"$HIVE_HOME\" \"${CLAUDE_HOME-UNSET}\" \"${CODEX_HOME-UNSET}\"\n").unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
        let mut env = crate::testenv::EnvGuard::new();
        env.set("HIVE_HOME", "a space '$x' `y`\nend");
        env.set("CLAUDE_HOME", "");
        env.remove("CODEX_HOME");
        env.set("HIVE_BIN", &probe);
        let out = Command::new("sh")
            .args(["-c", &viewer_command(&claude("abc12345"))])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "a space '$x' `y`\nend\n\nUNSET\n"
        );
    }
}
