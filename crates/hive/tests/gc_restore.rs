//! Failed restore writes must leave the archived entry available for retry.

use std::fs;
use std::os::unix::process::CommandExt;
use std::process::Command;

use serde_json::json;

#[test]
fn test_restore_write_limit_preserves_the_original_entry() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let hive_home = home.join(".hive");
    let archive = hive_home.join("trash/limited");
    let payload = archive.join("payload");
    fs::create_dir_all(&payload).unwrap();
    fs::write(
        archive.join("manifest.json"),
        json!({
            "schema": 1, "archiveId": "limited", "team": "old",
            "createdAt": "1", "workspace": "", "origin": "delete",
            "state": "quarantined", "quarantinedAt": 1,
            "purgeAfter": null, "members": []
        })
        .to_string(),
    )
    .unwrap();
    let original = json!({
        "team": "old", "createdAt": "1", "members": [],
        "workspace": hive_home.join("teams/old"), "extra": "x".repeat(4096)
    })
    .to_string();
    let entry_path = payload.join("team.json");
    fs::write(&entry_path, &original).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_hive"));
    command.args(["gc", "restore", "limited", "--as", "new"]);
    for key in ["HOME", "CLAUDE_HOME", "CODEX_HOME", "GROK_HOME"] {
        command.env(key, &home);
    }
    command.env("HIVE_HOME", &hive_home);
    for key in [
        "CODEX_THREAD_ID",
        "GROK_SESSION_ID",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_HOST_SESSION_ID",
        "TMUX",
        "TMUX_PANE",
    ] {
        command.env_remove(key);
    }
    // Only the child is limited; nextest and its other tests are untouched.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 1024,
                rlim_max: 1024,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            Ok(())
        });
    }
    let output = command.output().unwrap();

    assert!(!output.status.success());
    assert_eq!(fs::read(&entry_path).unwrap(), original.as_bytes());
    assert!(!hive_home.join("teams/new").exists());
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(archive.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["state"], "quarantined");
}
