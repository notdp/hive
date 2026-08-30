//! Real-tmux e2e for the notify lifecycle (port of tests/e2e/test_notify_lifecycle.py).
//! Runs entirely inside its own detached session; never touches a live one.

use std::process::Command;
use std::time::{Duration, Instant};

fn have_tmux() -> bool {
    Command::new("tmux").arg("-V").output().is_ok()
}

fn run_tmux(args: &[&str]) -> String {
    let out = Command::new("tmux").args(args).output().expect("tmux runs");
    assert!(
        out.status.success(),
        "tmux {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string()
}

fn kill_session(session: &str) {
    let _ = Command::new("tmux").args(["kill-session", "-t", session]).output();
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", std::process::id())
}

#[test]
fn test_e2e_cleanup_selected_window_clears_durable_state_without_hook() {
    if !have_tmux() {
        return;
    }
    let session = unique("hive-e2e-notify-a");
    let result = std::panic::catch_unwind(|| {
        let window_target = run_tmux(&[
            "new-session", "-d", "-P", "-F", "#{session_name}:#{window_index}",
            "-s", &session, "-n", "target", "sleep", "60",
        ]);
        let pane = run_tmux(&["display-message", "-p", "-t", &window_target, "#{pane_id}"]);
        let token = format!("{pane}:manual-fire");
        run_tmux(&["rename-window", "-t", &window_target, "[!] target"]);
        run_tmux(&["set-window-option", "-t", &window_target, "@hive-notify-token", &token]);
        run_tmux(&["set-window-option", "-t", &window_target, "@hive-notify-original-name", "target"]);
        run_tmux(&["set-window-option", "-t", &window_target, "@hive-notify-hook", hive::notify_ui::SELECT_HOOK_NAME]);
        run_tmux(&["set-window-option", "-t", &window_target, "window-status-style", "reverse,bold"]);
        run_tmux(&["set-window-option", "-t", &window_target, "window-status-current-style", "reverse,bold"]);
        run_tmux(&["set-option", "-p", "-t", &pane, "@hive-notify-active", &token]);

        assert!(hive::notify_ui::cleanup_selected_window(&window_target, ""));

        assert_eq!(run_tmux(&["display-message", "-p", "-t", &window_target, "#{@hive-notify-token}"]), "");
        assert_eq!(run_tmux(&["display-message", "-p", "-t", &window_target, "#{window_name}"]), "target");
        assert_eq!(run_tmux(&["show-window-option", "-v", "-t", &window_target, "window-status-style"]), "");
        assert_eq!(run_tmux(&["show-window-option", "-v", "-t", &window_target, "window-status-current-style"]), "");
        assert_eq!(run_tmux(&["display-message", "-p", "-t", &pane, "#{@hive-notify-active}"]), "");
    });
    kill_session(&session);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[test]
fn test_e2e_notify_select_hook_cleans_selected_window() {
    // after-select-window is a command hook: a scripted `select-window` fires
    // it on a detached session too, so the whole flow runs inside its own
    // session.
    if !have_tmux() {
        return;
    }
    let session = unique("hive-e2e-notify-b");
    // The select hook must call back into the real hive binary, not this
    // test harness (current_exe here is the test executable).
    std::env::set_var("HIVE_BIN", env!("CARGO_BIN_EXE_hive"));
    let result = std::panic::catch_unwind(|| {
        run_tmux(&["new-session", "-d", "-x", "80", "-y", "24", "-s", &session, "-n", "home", "sleep", "60"]);
        let window_target = run_tmux(&[
            "new-window", "-d", "-t", &format!("{session}:"), "-P", "-F",
            "#{session_name}:#{window_index}", "-n", "hive-notify-test", "sleep", "60",
        ]);
        let pane = run_tmux(&["display-message", "-p", "-t", &window_target, "#{pane_id}"]);

        hive::notify_ui::show_window_flash(
            "Agent finished", &pane, &window_target, "hive-notify-test", "orch", false, "",
        )
        .expect("flash succeeds");

        let token = run_tmux(&["display-message", "-p", "-t", &window_target, "#{@hive-notify-token}"]);
        assert!(token.starts_with(&format!("{pane}:")), "token {token:?}");
        let hooks = run_tmux(&["show-hooks", "-t", &session]);
        assert!(hooks.contains(hive::notify_ui::SELECT_HOOK_NAME));

        run_tmux(&["select-window", "-t", &window_target]);

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if run_tmux(&["display-message", "-p", "-t", &window_target, "#{@hive-notify-token}"]).is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "notify token never cleared");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(run_tmux(&["display-message", "-p", "-t", &window_target, "#{window_name}"]), "hive-notify-test");
        assert_eq!(run_tmux(&["show-window-option", "-v", "-t", &window_target, "window-status-style"]), "");
        assert_eq!(run_tmux(&["show-window-option", "-v", "-t", &window_target, "window-status-current-style"]), "");
    });
    kill_session(&session);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
