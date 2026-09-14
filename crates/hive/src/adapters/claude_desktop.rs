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
//! defensive, and nothing here writes. The app keeps a copy per account and
//! org it has been signed into and updates only the current account's, so
//! copies disagree after an account switch: the copy with the latest
//! `lastActivityAt` is the conversation's, older ones are the previous
//! account's frozen view. Copies that disagree at the same activity time are
//! "unknown".

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
/// disagree at the same activity time — an unknown never drives a roster
/// write. Among disagreeing copies the latest `lastActivityAt` wins: the
/// app updates only the current account's copy.
pub fn desktop_record(host_session_id: &str) -> Option<DesktopRecord> {
    scan_record(host_session_id).ok().flatten()
}

/// Presence of a readable, consistent desktop record across account copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordPresence {
    Present,
    Absent,
    Unknown,
}

/// Absent only after a complete readable search finds no record. Invalid ids,
/// unreadable directories, malformed records and conflicting copies are unknown.
/// This observation neither creates directories nor changes desktop records.
pub fn record_presence(host_session_id: &str) -> RecordPresence {
    match scan_record(host_session_id) {
        Ok(Some(_)) => RecordPresence::Present,
        Ok(None) => RecordPresence::Absent,
        Err(()) => RecordPresence::Unknown,
    }
}

fn scan_record(host_session_id: &str) -> Result<Option<DesktopRecord>, ()> {
    if !is_host_session_id(host_session_id) {
        return Err(());
    }
    let root = sessions_root();
    // every copy with its activity time: the latest is the conversation's,
    // older ones a previous account's frozen view whatever they say, and
    // only copies that disagree at the latest time conflict
    let mut copies: Vec<(DesktopRecord, f64)> = Vec::new();
    // a directory that cannot be listed may hold a copy that disagrees:
    // unknown, never "absent"
    for account in list_dirs(&root).ok_or(())? {
        for org in list_dirs(&account).ok_or(())? {
            let path = org.join(format!("{host_session_id}.json"));
            match fs::metadata(&path) {
                Ok(m) if m.is_file() => {}
                Ok(_) => return Err(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err(()),
            }
            let obj = read_json_object(&path).ok_or(())?;
            let record = parse_record(&obj).ok_or(())?;
            let at = obj
                .get("lastActivityAt")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            copies.push((record, at));
        }
    }
    let Some(latest) = copies.iter().map(|(_, at)| *at).reduce(f64::max) else {
        return Ok(None);
    };
    let mut at_latest = copies
        .into_iter()
        .filter(|(_, at)| *at == latest)
        .map(|(record, _)| record);
    let record = at_latest.next().ok_or(())?;
    if at_latest.any(|other| other != record) {
        return Err(());
    }
    Ok(Some(record))
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

/// Whether the desktop app launched *session*: the registry entry's own
/// `entrypoint`, the one signal that tells a desktop conversation from a
/// terminal's claude. The mirror lane (`cli/team` create and join outside
/// tmux) is the desktop's alone; the host-session enrolment below is the
/// stricter, best-effort follow-up that also needs the desktop's record.
pub fn is_desktop_launched(session: &ClaudeSession) -> bool {
    session.entrypoint == DESKTOP_ENTRYPOINT
}

/// The host session id a roster row may carry for *session*: only for an
/// interactive session the desktop launched, and only when the desktop's
/// record names this very CLI session as the conversation's current one.
/// A child CLI that merely inherited the variable, or a bg job, gets none,
/// so the id on a roster row always means "this desktop conversation".
pub fn enrol_host_session_id(session: &ClaudeSession) -> Option<String> {
    let host = host_session_id_env();
    if host.is_empty() || session.kind != "interactive" || !is_desktop_launched(session) {
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
    fn test_desktop_record_is_unknown_when_account_copies_disagree_at_the_same_time() {
        let tmp = TempDir::new().unwrap();
        let mut env = EnvGuard::new();
        env.set("HOME", tmp.path());
        let same =
            json!({"cliSessionId": "new", "priorCliSessionIds": ["old"], "lastActivityAt": 5});
        write_record(tmp.path(), "a1", "o1", "local_x", same.clone());
        write_record(tmp.path(), "a2", "o2", "local_x", same);
        assert!(desktop_record("local_x").is_some());
        assert_eq!(record_presence("local_x"), RecordPresence::Present);
        write_record(
            tmp.path(),
            "a2",
            "o2",
            "local_x",
            json!({"cliSessionId": "other", "priorCliSessionIds": ["old"], "lastActivityAt": 5}),
        );
        assert_eq!(desktop_record("local_x"), None);
        assert_eq!(record_presence("local_x"), RecordPresence::Unknown);
        // copies without an activity time are as old as each other
        write_record(
            tmp.path(),
            "a1",
            "o1",
            "local_z",
            json!({"cliSessionId": "one"}),
        );
        write_record(
            tmp.path(),
            "a2",
            "o2",
            "local_z",
            json!({"cliSessionId": "two"}),
        );
        assert_eq!(record_presence("local_z"), RecordPresence::Unknown);
    }

    #[test]
    fn test_desktop_record_takes_the_latest_copy_over_a_previous_accounts() {
        // An account switch leaves the old account's copy behind, frozen at
        // the CLI session the conversation ran then; the app updates the
        // current account's copy alone, and its activity time says so.
        let tmp = TempDir::new().unwrap();
        let mut env = EnvGuard::new();
        env.set("HOME", tmp.path());
        write_record(
            tmp.path(),
            "old-account",
            "o1",
            "local_x",
            json!({"cliSessionId": "before-switch", "priorCliSessionIds": ["first"], "lastActivityAt": 1000}),
        );
        write_record(
            tmp.path(),
            "new-account",
            "o2",
            "local_x",
            json!({"cliSessionId": "after-switch", "priorCliSessionIds": ["first", "before-switch"], "lastActivityAt": 2000}),
        );
        assert_eq!(
            desktop_record("local_x"),
            Some(DesktopRecord {
                cli_session_id: "after-switch".to_string(),
                prior_cli_session_ids: vec!["first".to_string(), "before-switch".to_string()],
            })
        );
        assert_eq!(record_presence("local_x"), RecordPresence::Present);
        // directory order does not decide: the older copy listed later loses too
        write_record(
            tmp.path(),
            "zz-account",
            "o3",
            "local_x",
            json!({"cliSessionId": "before-switch", "lastActivityAt": 1500}),
        );
        assert_eq!(
            desktop_record("local_x").unwrap().cli_session_id,
            "after-switch"
        );
        // two older copies that disagree with each other are both history:
        // the one latest copy still decides, wherever the listing puts it
        for (accounts, latest) in [
            (["a1", "a2", "a3"], "a3"),
            (["b1", "b2", "b3"], "b1"),
            (["c1", "c2", "c3"], "c2"),
        ] {
            let host = format!("local_{latest}");
            for account in accounts {
                let body = if account == latest {
                    json!({"cliSessionId": "current", "lastActivityAt": 3000})
                } else {
                    json!({"cliSessionId": format!("old-{account}"), "lastActivityAt": 1000})
                };
                write_record(tmp.path(), account, "org", &host, body);
            }
            assert_eq!(
                desktop_record(&host).map(|r| r.cli_session_id),
                Some("current".to_string()),
                "{host}"
            );
            assert_eq!(record_presence(&host), RecordPresence::Present);
        }
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
        let presence = record_presence("local_y");
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(got, None);
        assert_eq!(presence, RecordPresence::Unknown);
        assert!(desktop_record("local_y").is_some());
    }

    #[test]
    fn test_record_presence_requires_complete_readable_search() {
        let tmp = TempDir::new().unwrap();
        let mut env = EnvGuard::new();
        env.set("HOME", tmp.path());
        assert_eq!(record_presence("local_missing"), RecordPresence::Absent);
        assert!(!sessions_root().exists());
        for id in ["", "local_", "../etc", "local_../etc"] {
            assert_eq!(record_presence(id), RecordPresence::Unknown);
        }
        let org = sessions_root().join("account/org");
        fs::create_dir_all(&org).unwrap();
        assert_eq!(record_presence("local_missing"), RecordPresence::Absent);
        let path = org.join("local_missing.json");
        fs::write(&path, "broken").unwrap();
        assert_eq!(record_presence("local_missing"), RecordPresence::Unknown);
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert_eq!(record_presence("local_missing"), RecordPresence::Unknown);
    }

    #[test]
    fn test_is_desktop_launched_reads_the_entrypoint_alone() {
        assert!(is_desktop_launched(&session(
            "interactive",
            "claude-desktop",
            "s1"
        )));
        // kind is not part of the question
        assert!(is_desktop_launched(&session("", "claude-desktop", "s1")));
        assert!(!is_desktop_launched(&session("interactive", "cli", "s1")));
        assert!(!is_desktop_launched(&session("interactive", "", "s1")));
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
