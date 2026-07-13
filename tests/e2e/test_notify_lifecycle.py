import shutil
import subprocess
import uuid

import pytest

from hive import notify_ui
from tests.e2e._helpers import run_tmux, wait_for


@pytest.mark.skipif(shutil.which("tmux") is None, reason="tmux is required for e2e tests")
def test_e2e_cleanup_selected_window_clears_durable_state_without_hook():
    session = f"hive-e2e-notify-{uuid.uuid4().hex[:8]}"
    window_target = ""
    try:
        window_target = run_tmux([
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{session_name}:#{window_index}",
            "-s",
            session,
            "-n",
            "target",
            "sleep",
            "60",
        ]).stdout.strip()
        pane = run_tmux(["display-message", "-p", "-t", window_target, "#{pane_id}"]).stdout.strip()
        token = f"{pane}:manual-fire"
        run_tmux(["rename-window", "-t", window_target, "[!] target"])
        run_tmux(["set-window-option", "-t", window_target, "@hive-notify-token", token])
        run_tmux(["set-window-option", "-t", window_target, "@hive-notify-original-name", "target"])
        run_tmux(["set-window-option", "-t", window_target, "@hive-notify-hook", notify_ui.SELECT_HOOK_NAME])
        run_tmux(["set-window-option", "-t", window_target, "window-status-style", "reverse,bold"])
        run_tmux(["set-window-option", "-t", window_target, "window-status-current-style", "reverse,bold"])
        run_tmux(["set-option", "-p", "-t", pane, "@hive-notify-active", token])

        assert notify_ui.cleanup_selected_window(window_target) is True

        assert run_tmux(["display-message", "-p", "-t", window_target, "#{@hive-notify-token}"]).stdout.strip() == ""
        assert run_tmux(["display-message", "-p", "-t", window_target, "#{window_name}"]).stdout.strip() == "target"
        assert run_tmux(["show-window-option", "-v", "-t", window_target, "window-status-style"]).stdout.strip() == ""
        assert run_tmux(["show-window-option", "-v", "-t", window_target, "window-status-current-style"]).stdout.strip() == ""
        assert run_tmux(["display-message", "-p", "-t", pane, "#{@hive-notify-active}"]).stdout.strip() == ""
    finally:
        subprocess.run(["tmux", "kill-session", "-t", session], text=True, capture_output=True, timeout=5, check=False)


@pytest.mark.skipif(shutil.which("tmux") is None, reason="tmux is required for e2e tests")
def test_e2e_notify_select_hook_cleans_selected_window():
    # after-select-window is a command hook: a scripted `select-window` fires
    # it on a detached session too, so the whole flow runs inside its own
    # session. The previous version required an attached client, created its
    # window in the developer's live session, and stole the focus.
    session = f"hive-e2e-notify-{uuid.uuid4().hex[:8]}"
    try:
        run_tmux([
            "new-session", "-d", "-x", "80", "-y", "24",
            "-s", session, "-n", "home", "sleep", "60",
        ])
        window_target = run_tmux([
            "new-window",
            "-d",
            "-t",
            f"{session}:",
            "-P",
            "-F",
            "#{session_name}:#{window_index}",
            "-n",
            "hive-notify-test",
            "sleep",
            "60",
        ]).stdout.strip()
        pane = run_tmux(["display-message", "-p", "-t", window_target, "#{pane_id}"]).stdout.strip()

        notify_ui.show_window_flash(
            "Agent finished",
            pane,
            window_target,
            "hive-notify-test",
            agent_name="orch",
            animate_on_arrival=False,
        )

        token = run_tmux(["display-message", "-p", "-t", window_target, "#{@hive-notify-token}"]).stdout.strip()
        assert token.startswith(f"{pane}:")
        hooks = run_tmux(["show-hooks", "-t", session]).stdout
        assert notify_ui.SELECT_HOOK_NAME in hooks

        run_tmux(["select-window", "-t", window_target])

        wait_for(
            lambda: run_tmux(["display-message", "-p", "-t", window_target, "#{@hive-notify-token}"]).stdout.strip() == "",
            timeout=5.0,
        )
        assert run_tmux(["display-message", "-p", "-t", window_target, "#{window_name}"]).stdout.strip() == "hive-notify-test"
        assert run_tmux(["show-window-option", "-v", "-t", window_target, "window-status-style"]).stdout.strip() == ""
        assert run_tmux(["show-window-option", "-v", "-t", window_target, "window-status-current-style"]).stdout.strip() == ""
    finally:
        subprocess.run(["tmux", "kill-session", "-t", session], text=True, capture_output=True, timeout=5, check=False)
