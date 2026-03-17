import json
import shutil
import subprocess
import uuid
from pathlib import Path

import pytest

from tests.e2e._helpers import (
    base_env,
    hive_shell_command,
    run_hive,
    run_tmux,
    send_tmux_command,
    wait_for,
    wait_for_file,
    write_fake_droid,
)


@pytest.mark.skipif(shutil.which("tmux") is None, reason="tmux is required for e2e tests")
def test_e2e_init_current_exec_and_terminal_management(tmp_path: Path):
    fake_droid = write_fake_droid(tmp_path)
    env = base_env(tmp_path, fake_droid)
    session = f"hive-e2e-{uuid.uuid4().hex[:8]}"
    workspace = tmp_path / "ws"
    current_before = tmp_path / "current-before.json"
    init_out = tmp_path / "init.json"

    pane_a = run_tmux(["new-session", "-d", "-s", session, "-x", "120", "-y", "40", "-P", "-F", "#{pane_id}"]).stdout.strip()
    pane_b = run_tmux(["split-window", "-t", pane_a, "-d", "-h", "-P", "-F", "#{pane_id}"]).stdout.strip()

    try:
        send_tmux_command(pane_a, hive_shell_command(["current"], env=env, cwd=tmp_path, stdout_path=current_before))
        before_payload = json.loads(wait_for_file(current_before))
        assert before_payload["team"] is None
        assert before_payload["tmux"]["paneCount"] == 2
        assert all("role" in pane for pane in before_payload["tmux"]["panes"])

        send_tmux_command(pane_a, hive_shell_command(["init", "--workspace", str(workspace)], env=env, cwd=tmp_path, stdout_path=init_out))
        init_payload = json.loads(wait_for_file(init_out))
        assert init_payload["team"] == session
        assert any(p["role"] == "lead" for p in init_payload["panes"])
        assert any(p["role"] == "terminal" for p in init_payload["panes"])

        result = run_hive(["who", "--team", session], env=env, cwd=tmp_path)
        assert result.returncode == 0, result.stderr or result.stdout
        payload = json.loads(result.stdout)
        assert payload["terminals"] == {"term-1": {"alive": True, "pane": pane_b}}

        result = run_hive(["exec", "term-1", "echo terminal-ok", "--team", session], env=env, cwd=tmp_path)
        assert result.returncode == 0, result.stderr or result.stdout

        def terminal_capture() -> str:
            return run_tmux(["capture-pane", "-t", pane_b, "-p", "-S", "-20"]).stdout

        wait_for(lambda: "terminal-ok" in terminal_capture())
        assert "terminal-ok" in terminal_capture()

        result = run_hive(["terminal", "remove", "term-1", "--team", session], env=env, cwd=tmp_path)
        assert result.returncode == 0, result.stderr or result.stdout
        result = run_hive(["who", "--team", session], env=env, cwd=tmp_path)
        assert result.returncode == 0, result.stderr or result.stdout
        assert json.loads(result.stdout)["terminals"] == {}

        result = run_hive(["terminal", "add", "shell", "--team", session, "--pane", pane_b], env=env, cwd=tmp_path)
        assert result.returncode == 0, result.stderr or result.stdout
        result = run_hive(["who", "--team", session], env=env, cwd=tmp_path)
        assert result.returncode == 0, result.stderr or result.stdout
        assert json.loads(result.stdout)["terminals"] == {"shell": {"alive": True, "pane": pane_b}}

        tags = run_tmux(["list-panes", "-t", f"{session}:0", "-F", "#{pane_id} role=#{@hive-role} agent=#{@hive-agent} team=#{@hive-team}"]).stdout
        assert f"{pane_a} role=lead agent=orchestrator team={session}" in tags
        assert f"{pane_b} role=terminal agent=shell team={session}" in tags

        result = run_hive(["delete", session], env=env, cwd=tmp_path)
        assert result.returncode == 0, result.stderr or result.stdout
        session_check = subprocess.run(["tmux", "has-session", "-t", session], capture_output=True, text=True)
        assert session_check.returncode != 0
        assert not ((tmp_path / ".hive" / "teams" / session).exists())
    finally:
        subprocess.run(["tmux", "kill-session", "-t", session], capture_output=True, text=True)
