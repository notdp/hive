//! Real-tmux e2e for the notify lifecycle: fire, select-window, clear.
//! Runs entirely inside its own detached session; never touches a live one.

use std::time::{Duration, Instant};

mod common;
use common::{kill_session, private_server, require_tmux, run_tmux, EnvVarGuard};

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", std::process::id())
}

#[test]
fn test_e2e_cleanup_selected_window_clears_durable_state_without_hook() {
    require_tmux();
    let _server = private_server();
    let session = unique("hive-e2e-notify-a");
    let result = std::panic::catch_unwind(|| {
        let window_target = run_tmux(&[
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{session_name}:#{window_index}",
            "-s",
            &session,
            "-n",
            "target",
            "sleep",
            "60",
        ]);
        let pane = run_tmux(&["display-message", "-p", "-t", &window_target, "#{pane_id}"]);
        let token = format!("{pane}:manual-fire");
        run_tmux(&["rename-window", "-t", &window_target, "[!] target"]);
        run_tmux(&[
            "set-window-option",
            "-t",
            &window_target,
            "@hive-notify-token",
            &token,
        ]);
        run_tmux(&[
            "set-window-option",
            "-t",
            &window_target,
            "@hive-notify-original-name",
            "target",
        ]);
        run_tmux(&[
            "set-window-option",
            "-t",
            &window_target,
            "@hive-notify-hook",
            hive::notify_ui::SELECT_HOOK_NAME,
        ]);
        run_tmux(&[
            "set-window-option",
            "-t",
            &window_target,
            "window-status-style",
            "reverse,bold",
        ]);
        run_tmux(&[
            "set-window-option",
            "-t",
            &window_target,
            "window-status-current-style",
            "reverse,bold",
        ]);
        run_tmux(&[
            "set-option",
            "-p",
            "-t",
            &pane,
            "@hive-notify-active",
            &token,
        ]);

        assert!(hive::notify_ui::cleanup_selected_window(&window_target, ""));

        assert_eq!(
            run_tmux(&[
                "display-message",
                "-p",
                "-t",
                &window_target,
                "#{@hive-notify-token}"
            ]),
            ""
        );
        assert_eq!(
            run_tmux(&[
                "display-message",
                "-p",
                "-t",
                &window_target,
                "#{window_name}"
            ]),
            "target"
        );
        assert_eq!(
            run_tmux(&[
                "show-window-option",
                "-v",
                "-t",
                &window_target,
                "window-status-style"
            ]),
            ""
        );
        assert_eq!(
            run_tmux(&[
                "show-window-option",
                "-v",
                "-t",
                &window_target,
                "window-status-current-style"
            ]),
            ""
        );
        assert_eq!(
            run_tmux(&[
                "display-message",
                "-p",
                "-t",
                &pane,
                "#{@hive-notify-active}"
            ]),
            ""
        );
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
    require_tmux();
    let _server = private_server();
    let session = unique("hive-e2e-notify-b");
    // The select hook must call back into the real hive binary, not this
    // test harness (current_exe here is the test executable); `self_exe`
    // reads HIVE_BIN first. The guard puts the variable back on the way out.
    let _hive_bin = EnvVarGuard::set("HIVE_BIN", env!("CARGO_BIN_EXE_hive"));
    let result = std::panic::catch_unwind(|| {
        run_tmux(&[
            "new-session",
            "-d",
            "-x",
            "80",
            "-y",
            "24",
            "-s",
            &session,
            "-n",
            "home",
            "sleep",
            "60",
        ]);
        let window_target = run_tmux(&[
            "new-window",
            "-d",
            "-t",
            &format!("{session}:"),
            "-P",
            "-F",
            "#{session_name}:#{window_index}",
            "-n",
            "hive-notify-test",
            "sleep",
            "60",
        ]);
        let pane = run_tmux(&["display-message", "-p", "-t", &window_target, "#{pane_id}"]);

        hive::notify_ui::show_window_flash(
            "Agent finished",
            &pane,
            &window_target,
            "hive-notify-test",
            "orch",
            false,
            "",
        )
        .expect("flash succeeds");

        let token = run_tmux(&[
            "display-message",
            "-p",
            "-t",
            &window_target,
            "#{@hive-notify-token}",
        ]);
        assert!(token.starts_with(&format!("{pane}:")), "token {token:?}");
        let hooks = run_tmux(&["show-hooks", "-t", &session]);
        assert!(hooks.contains(hive::notify_ui::SELECT_HOOK_NAME));

        run_tmux(&["select-window", "-t", &window_target]);

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if run_tmux(&[
                "display-message",
                "-p",
                "-t",
                &window_target,
                "#{@hive-notify-token}",
            ])
            .is_empty()
            {
                break;
            }
            assert!(Instant::now() < deadline, "notify token never cleared");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            run_tmux(&[
                "display-message",
                "-p",
                "-t",
                &window_target,
                "#{window_name}"
            ]),
            "hive-notify-test"
        );
        assert_eq!(
            run_tmux(&[
                "show-window-option",
                "-v",
                "-t",
                &window_target,
                "window-status-style"
            ]),
            ""
        );
        assert_eq!(
            run_tmux(&[
                "show-window-option",
                "-v",
                "-t",
                &window_target,
                "window-status-current-style"
            ]),
            ""
        );
    });
    kill_session(&session);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
