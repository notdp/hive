use std::process::Command;

#[test]
fn test_codex_launcher_exits_when_process_cwd_was_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let gone = tmp.path().join("gone");
    std::fs::create_dir(&gone).unwrap();
    let codex_home = tmp.path().join("codex");
    std::fs::create_dir_all(
        codex_home
            .join("plugins/cache/hive/hive")
            .join(env!("CARGO_PKG_VERSION")),
    )
    .unwrap();
    let output = Command::new("/bin/sh")
        .args([
            "-c",
            "rmdir \"$PWD\"; exec \"$1\" codex",
            "cwd-test",
            env!("CARGO_BIN_EXE_hive"),
        ])
        .current_dir(&gone)
        .env("HOME", tmp.path())
        .env("HIVE_HOME", tmp.path().join("hive"))
        .env("CODEX_HOME", &codex_home)
        .env("CLAUDE_HOME", tmp.path().join("claude"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("GROK_SESSION_ID")
        .env_remove("CLAUDE_CODE_MESSAGING_SOCKET")
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("working directory is unavailable"),
        "{output:?}"
    );
    assert!(!codex_home.join("config.toml").exists());
    assert!(!codex_home.join("app-server-control").exists());
}
