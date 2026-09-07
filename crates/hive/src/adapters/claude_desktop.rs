//! The Claude desktop app's own record of a conversation.
//!
//! The desktop app names each conversation by a stable id (`local_<uuid>`)
//! and exports it to the CLI it launches as `CLAUDE_CODE_HOST_SESSION_ID`.
//! Its record `<Application Support>/Claude/claude-code-sessions/<account>/
//! <org>/<stable id>.json` carries the CLI session currently under that
//! conversation (`cliSessionId`) and every CLI session it has used before
//! (`priorCliSessionIds`): a rewind-and-resend, a `/clear`, a return to the
//! pre-clear session each restart the CLI under a new session id and append
//! the old one there. A conversation forked by the user gets a stable id of
//! its own (and `forkedFromSessionId`), so its record never names the
//! parent's CLI sessions as its own. Layout and keys observed on desktop
//! 1.46388 with Claude Code 2.1.263, not a published contract: every read is
//! defensive, disagreement between the copies the app keeps per account is
//! "unknown", and nothing here writes.

use std::env;
use std::fs;
use std::path::PathBuf;

use serde_json::Value;

use crate::adapters::base::read_json_object;
use crate::adapters::claude_sessions::ClaudeSession;

pub const HOST_SESSION_ENV: &str = "CLAUDE_CODE_HOST_SESSION_ID";
const DESKTOP_ENTRYPOINT: &str = "claude-desktop";

/// What a desktop record says about its conversation's CLI sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopRecord {
    pub cli_session_id: String,
    pub prior_cli_session_ids: Vec<String>,
}

pub fn host_session_id_env() -> String {
    env::var(HOST_SESSION_ENV).unwrap_or_default()
}

fn sessions_root() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_default())
        .join("Library/Application Support/Claude/claude-code-sessions")
}

/// A stable id is `local_` plus a uuid; anything else never names a file.
fn is_host_session_id(id: &str) -> bool {
    id.starts_with("local_")
        && id.len() > 6
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn parse_record(obj: &serde_json::Map<String, Value>) -> Option<DesktopRecord> {
    let cli_session_id = obj.get("cliSessionId")?.as_str()?.to_string();
    if cli_session_id.is_empty() {
        return None;
    }
    let prior_cli_session_ids = obj
        .get("priorCliSessionIds")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Some(DesktopRecord {
        cli_session_id,
        prior_cli_session_ids,
    })
}

/// The desktop's record for *host_session_id*, or None when there is none,
/// one is unreadable, or the copies the app keeps under different accounts
/// disagree — an unknown never drives a roster write.
pub fn desktop_record(host_session_id: &str) -> Option<DesktopRecord> {
    if !is_host_session_id(host_session_id) {
        return None;
    }
    let root = sessions_root();
    let mut found: Option<DesktopRecord> = None;
    // a directory that cannot be listed may hold a copy that disagrees:
    // unknown, never "absent"
    for account in list_dirs(&root)? {
        for org in list_dirs(&account)? {
            let path = org.join(format!("{host_session_id}.json"));
            match fs::metadata(&path) {
                Ok(m) if m.is_file() => {}
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return None,
            }
            let record = read_json_object(&path).and_then(|o| parse_record(&o))?;
            match &found {
                Some(seen) if *seen != record => return None,
                _ => found = Some(record),
            }
        }
    }
    found
}

/// The subdirectories of *dir*; an absent *dir* is no directories, any
/// other failure to list it is None.
fn list_dirs(dir: &std::path::Path) -> Option<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.ok()?;
        if entry.file_type().ok()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Some(dirs)
}

/// The host session id a roster row may carry for *session*: only for an
/// interactive session the desktop launched, and only when the desktop's
/// record names this very CLI session as the conversation's current one.
/// A child CLI that merely inherited the variable, or a bg job, gets none,
/// so the id on a roster row always means "this desktop conversation".
pub fn enrol_host_session_id(session: &ClaudeSession) -> Option<String> {
    let host = host_session_id_env();
    if host.is_empty() || session.kind != "interactive" || session.entrypoint != DESKTOP_ENTRYPOINT
    {
        return None;
    }
    let record = desktop_record(&host)?;
    (record.cli_session_id == session.session_id).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;
    use serde_json::json;
    use tempfile::TempDir;

    fn write_record(home: &std::path::Path, account: &str, org: &str, host: &str, body: Value) {
        let dir = home
            .join("Library/Application Support/Claude/claude-code-sessions")
            .join(account)
            .join(org);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{host}.json")), body.to_string()).unwrap();
    }

    fn session(kind: &str, entrypoint: &str, sid: &str) -> ClaudeSession {
        ClaudeSession {
            name: "desk".to_string(),
            pid: 1,
            cwd: "/w".to_string(),
            kind: kind.to_string(),
            entrypoint: entrypoint.to_string(),
            socket_path: "/tmp/d.sock".to_string(),
            session_id: sid.to_string(),
            title: String::new(),
        }
    }

    #[test]
    fn test_desktop_record_reads_current_and_prior_cli_sessions() {
        let tmp = TempDir::new().unwrap();
        let mut env = EnvGuard::new();
        env.set("HOME", tmp.path());
        write_record(
            tmp.path(),
            "acct",
            "org",
            "local_abc",
            json!({"sessionId": "local_abc", "cliSessionId": "new", "priorCliSessionIds": ["old"], "title": "t"}),
        );
        assert_eq!(
            desktop_record("local_abc"),
            Some(DesktopRecord {
                cli_session_id: "new".to_string(),
                prior_cli_session_ids: vec!["old".to_string()],
            })
        );
        // no prior list is an empty list, never a missing record
        write_record(
            tmp.path(),
            "acct",
            "org",
            "local_fresh",
            json!({"cliSessionId": "only"}),
        );
        assert_eq!(
            desktop_record("local_fresh").unwrap().prior_cli_session_ids,
            Vec::<String>::new()
        );
        assert_eq!(desktop_record("local_missing"), None);
        assert_eq!(desktop_record("../etc"), None);
        assert_eq!(desktop_record(""), None);
    }

    #[test]
    fn test_desktop_record_is_unknown_when_account_copies_disagree() {
        let tmp = TempDir::new().unwrap();
        let mut env = EnvGuard::new();
        env.set("HOME", tmp.path());
        let same = json!({"cliSessionId": "new", "priorCliSessionIds": ["old"]});
        write_record(tmp.path(), "a1", "o1", "local_x", same.clone());
        write_record(tmp.path(), "a2", "o2", "local_x", same);
        assert!(desktop_record("local_x").is_some());
        write_record(
            tmp.path(),
            "a2",
            "o2",
            "local_x",
            json!({"cliSessionId": "other", "priorCliSessionIds": ["old"]}),
        );
        assert_eq!(desktop_record("local_x"), None);
    }

    #[cfg(unix)]
    #[test]
    fn test_desktop_record_is_unknown_when_a_copy_cannot_be_listed() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let mut env = EnvGuard::new();
        env.set("HOME", tmp.path());
        let same = json!({"cliSessionId": "new", "priorCliSessionIds": ["old"]});
        write_record(tmp.path(), "a1", "o1", "local_y", same.clone());
        write_record(tmp.path(), "a2", "o2", "local_y", same);
        let sealed = tmp
            .path()
            .join("Library/Application Support/Claude/claude-code-sessions/a2");
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000)).unwrap();
        let got = desktop_record("local_y");
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(got, None);
        assert!(desktop_record("local_y").is_some());
    }

    #[test]
    fn test_enrol_host_session_id_needs_a_desktop_interactive_session_the_record_names() {
        let tmp = TempDir::new().unwrap();
        let mut env = EnvGuard::new();
        env.set("HOME", tmp.path());
        env.set(HOST_SESSION_ENV, "local_h");
        write_record(
            tmp.path(),
            "a",
            "o",
            "local_h",
            json!({"cliSessionId": "cur", "priorCliSessionIds": []}),
        );
        assert_eq!(
            enrol_host_session_id(&session("interactive", "claude-desktop", "cur")),
            Some("local_h".to_string())
        );
        // a child CLI that inherited the variable: its own session is not
        // the conversation's current one
        assert_eq!(
            enrol_host_session_id(&session("interactive", "cli", "child")),
            None
        );
        assert_eq!(
            enrol_host_session_id(&session("interactive", "claude-desktop", "stale")),
            None
        );
        assert_eq!(
            enrol_host_session_id(&session("bg", "claude-desktop", "cur")),
            None
        );
        env.remove(HOST_SESSION_ENV);
        assert_eq!(
            enrol_host_session_id(&session("interactive", "claude-desktop", "cur")),
            None
        );
    }
}
