// --------------------------------------------------------------------------
// the daemon's auth baseline
// --------------------------------------------------------------------------
//
// codex's AuthManager loads auth.json once, at process start, and reloads
// it later only through a guarded reload that requires the on-disk account
// id to equal the cached one (`reload_if_account_id_matches`, codex 0.153.4
// `login/src/auth/manager.rs`; the 401 recovery and the pre-refresh reload
// both go through it). A login to another account or workspace, or a login
// while the daemon holds no account, therefore leaves the daemon unable to
// recover from the revoked token: every turn ends with "Your access token
// could not be refreshed because you have since logged out or signed in to
// another account". No RPC reloads unconditionally (only the app-server's
// own login flows do), so the only repair is a fresh process.
//
// Hive records the account id the daemon was born with beside the pidfile
// and restarts the daemon when the disk moves away from it. A same-account
// refresh or re-login keeps the id and is the daemon's own business: it
// reloads that itself. Only `tokens.account_id` is read from auth.json,
// never a token. A daemon without a baseline (spawned by an older hive)
// is asked for its account over `account/rateLimits/read` — the backend
// names the account of the token the daemon holds — so the baseline is
// the running process's identity, never a disk snapshot taken later.

use std::fs;

use serde_json::{json, Value};

use super::daemon::{daemon_account, DaemonAccount};
use super::records::{codex_home, shared_auth_baseline_path};

/// The on-disk auth's account id: `Some(None)` is a logged-out CODEX_HOME
/// (no auth.json, or no tokens); the outer `None` is a file that could not
/// be read as JSON — a login mid-write, which must never count as a change.
pub fn disk_account_id() -> Option<Option<String>> {
    let path = codex_home().join("auth.json");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Some(None),
        Err(_) => return None,
    };
    let auth: Value = serde_json::from_str(&text).ok()?;
    Some(account_id_of(&auth))
}

fn account_id_of(auth: &Value) -> Option<String> {
    auth.get("tokens")?
        .get("account_id")?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// The account id recorded when hive last spawned the daemon; `None` when
/// no baseline is on disk (a daemon spawned by an older hive, or one whose
/// spawn could not read auth.json).
pub fn baseline_account_id() -> Option<Option<String>> {
    let text = fs::read_to_string(shared_auth_baseline_path()).ok()?;
    let record: Value = serde_json::from_str(&text).ok()?;
    let account = record.get("accountId")?;
    match account {
        Value::Null => Some(None),
        Value::String(id) => Some(Some(id.clone())),
        _ => None,
    }
}

/// Record *account* (`None`: logged out) as the running daemon's.
pub fn write_auth_baseline(account: Option<&str>) -> bool {
    let record = json!({ "accountId": account });
    fs::write(shared_auth_baseline_path(), record.to_string()).is_ok()
}

pub fn clear_auth_baseline() {
    let _ = fs::remove_file(shared_auth_baseline_path());
}

/// Whether the live daemon's account no longer matches the disk. A daemon
/// without a baseline is asked (`daemon_account`): its answer becomes the
/// baseline, a daemon that cannot authenticate at all is stale outright,
/// and no answer leaves the question for the next look.
pub fn daemon_auth_stale() -> bool {
    let Some(disk) = disk_account_id() else {
        return false;
    };
    if let Some(baseline) = baseline_account_id() {
        return baseline != disk;
    }
    match daemon_account() {
        DaemonAccount::Account(account) => {
            write_auth_baseline(Some(&account));
            Some(account) != disk
        }
        DaemonAccount::Unauthorized => true,
        DaemonAccount::Unknown => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::codex_app_server::client::DaemonClient;
    use crate::adapters::codex_app_server::tests::set_shared_client_override;
    use crate::testenv::EnvGuard;
    use std::sync::Arc;

    struct AccountClient(Value);

    impl DaemonClient for AccountClient {
        fn account_rate_limits(&self) -> Value {
            self.0.clone()
        }
    }

    fn daemon_answers(answer: Value) {
        let answer = Arc::new(answer);
        set_shared_client_override(move || {
            let client: Arc<dyn DaemonClient> = Arc::new(AccountClient((*answer).clone()));
            Some(client)
        });
    }

    fn write_auth(codex_home: &std::path::Path, body: &str) {
        fs::create_dir_all(codex_home).unwrap();
        fs::write(codex_home.join("auth.json"), body).unwrap();
    }

    fn auth_json(account_id: &str) -> String {
        json!({
            "auth_mode": "chatgpt",
            "last_refresh": "2026-09-07T13:16:24.609946Z",
            "tokens": {
                "access_token": "a.b.c",
                "refresh_token": "r",
                "id_token": "i.d.t",
                "account_id": account_id,
            },
        })
        .to_string()
    }

    #[test]
    fn test_disk_account_id_reads_only_the_account_field() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("CODEX_HOME", tmp.path());
        write_auth(tmp.path(), &auth_json("acct-a"));
        assert_eq!(disk_account_id(), Some(Some("acct-a".to_string())));
    }

    #[test]
    fn test_disk_account_id_logged_out_is_none_and_garbage_is_unreadable() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("CODEX_HOME", tmp.path());
        assert_eq!(disk_account_id(), Some(None));
        write_auth(tmp.path(), r#"{"auth_mode":"chatgpt"}"#);
        assert_eq!(disk_account_id(), Some(None));
        write_auth(tmp.path(), "{\"tokens\": {\"account_id\": \"acct");
        assert_eq!(disk_account_id(), None);
    }

    #[test]
    fn test_baseline_roundtrip_and_stale_on_account_change() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("CODEX_HOME", tmp.path());
        fs::create_dir_all(shared_auth_baseline_path().parent().unwrap()).unwrap();
        write_auth(tmp.path(), &auth_json("acct-a"));
        assert!(write_auth_baseline(Some("acct-a")));
        assert_eq!(baseline_account_id(), Some(Some("acct-a".to_string())));
        assert!(!daemon_auth_stale());

        // A same-account refresh rewrites the file: not stale.
        write_auth(
            tmp.path(),
            &auth_json("acct-a").replace("13:16:24", "14:00:00"),
        );
        assert!(!daemon_auth_stale());

        // Another account or workspace on disk: stale.
        write_auth(tmp.path(), &auth_json("acct-b"));
        assert!(daemon_auth_stale());

        // Logged out on disk: also a change the daemon cannot follow.
        fs::remove_file(tmp.path().join("auth.json")).unwrap();
        assert!(daemon_auth_stale());
    }

    #[test]
    fn test_unreadable_disk_is_never_stale() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("CODEX_HOME", tmp.path());
        fs::create_dir_all(shared_auth_baseline_path().parent().unwrap()).unwrap();
        write_auth(tmp.path(), &auth_json("acct-a"));
        assert!(write_auth_baseline(Some("acct-a")));
        write_auth(tmp.path(), "{\"tokens\": {\"account_id\": \"acct");
        assert!(!daemon_auth_stale());
        assert_eq!(baseline_account_id(), Some(Some("acct-a".to_string())));
    }

    #[test]
    fn test_missing_baseline_takes_the_daemons_own_account() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("CODEX_HOME", tmp.path());
        fs::create_dir_all(shared_auth_baseline_path().parent().unwrap()).unwrap();
        // Disk already moved to B while the daemon still holds A.
        write_auth(tmp.path(), &auth_json("acct-b"));
        daemon_answers(json!({"result": {"accountId": "acct-a"}}));
        assert_eq!(baseline_account_id(), None);
        assert!(daemon_auth_stale());
        assert_eq!(baseline_account_id(), Some(Some("acct-a".to_string())));
    }

    #[test]
    fn test_missing_baseline_daemon_on_the_disk_account_is_not_stale() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("CODEX_HOME", tmp.path());
        fs::create_dir_all(shared_auth_baseline_path().parent().unwrap()).unwrap();
        write_auth(tmp.path(), &auth_json("acct-a"));
        daemon_answers(json!({"result": {"accountId": "acct-a"}}));
        assert!(!daemon_auth_stale());
        assert_eq!(baseline_account_id(), Some(Some("acct-a".to_string())));
        write_auth(tmp.path(), &auth_json("acct-b"));
        assert!(daemon_auth_stale());
    }

    #[test]
    fn test_missing_baseline_unauthorized_daemon_is_stale_and_unknown_waits() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("CODEX_HOME", tmp.path());
        fs::create_dir_all(shared_auth_baseline_path().parent().unwrap()).unwrap();
        write_auth(tmp.path(), &auth_json("acct-a"));
        daemon_answers(json!({"__rejected__": true, "__error__": {"code": -32600,
            "message": "Your access token could not be refreshed because you have since logged out or signed in to another account. Please sign in again."}}));
        assert!(daemon_auth_stale());
        assert_eq!(baseline_account_id(), None);
        // A transport failure or a network error is not a verdict.
        daemon_answers(json!({"__timeout__": true}));
        assert!(!daemon_auth_stale());
        daemon_answers(json!({"__rejected__": true, "__error__": {"code": -32603,
            "message": "failed to fetch codex rate limits: connection reset"}}));
        assert!(!daemon_auth_stale());
        assert_eq!(baseline_account_id(), None);
    }

    #[test]
    fn test_logged_out_baseline_goes_stale_on_login() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("CODEX_HOME", tmp.path());
        fs::create_dir_all(shared_auth_baseline_path().parent().unwrap()).unwrap();
        assert!(write_auth_baseline(None));
        assert_eq!(baseline_account_id(), Some(None));
        assert!(!daemon_auth_stale());
        write_auth(tmp.path(), &auth_json("acct-a"));
        assert!(daemon_auth_stale());
        clear_auth_baseline();
        assert_eq!(baseline_account_id(), None);
    }
}
