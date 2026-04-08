import json
import shutil
import subprocess
import tempfile
import uuid
from pathlib import Path

import pytest

from tests.e2e._helpers import (
    base_env,
    run_hive_in_tmux_pane,
    run_tmux,
    wait_for,
    write_fake_droid,
)


@pytest.mark.skipif(shutil.which("tmux") is None, reason="tmux is required for e2e tests")
def test_e2e_init_current_exec_and_terminal_management(tmp_path: Path):
    workdir = Path(tempfile.mkdtemp(prefix="hive-e2e-", dir="/tmp"))
    fake_droid = write_fake_droid(workdir)
    env = base_env(workdir, fake_droid)
    session = f"hive-e2e-{uuid.uuid4().hex[:8]}"
    workspace = workdir / "ws"
    pane_a = run_tmux(["new-session", "-d", "-s", session, "-x", "120", "-y", "40", "-P", "-F", "#{pane_id}"]).stdout.strip()
    pane_b = run_tmux(["split-window", "-t", pane_a, "-d", "-h", "-P", "-F", "#{pane_id}"]).stdout.strip()

    def run_in_pane(args: list[str]) -> subprocess.CompletedProcess[str]:
        return run_hive_in_tmux_pane(pane_a, args, env=env, cwd=workdir)

    try:
        before_result = run_in_pane(["current"])
        assert before_result.returncode == 0, before_result.stdout
        before_payload = json.loads(before_result.stdout)
        assert before_payload["team"] is None
        assert before_payload["tmux"]["paneCount"] == 2
        assert all("role" in pane for pane in before_payload["tmux"]["panes"])

        init_result = run_in_pane(["init", "--workspace", str(workspace)])
        assert init_result.returncode == 0, init_result.stdout
        init_payload = json.loads(init_result.stdout)
        team_name = init_payload["team"]
        window_index = str(init_payload["window"]).split(":")[-1]
        assert team_name == f"{session}-{window_index}"
        assert any(p["role"] == "terminal" for p in init_payload["panes"])
        assert not any(p["role"] == "agent" for p in init_payload["panes"])

        who_result = run_in_pane(["who"])
        assert who_result.returncode == 0, who_result.stdout
        payload = json.loads(who_result.stdout)
        term_1 = next(member for member in payload["members"] if member["name"] == "term-1")
        assert term_1["role"] == "terminal"
        assert term_1["alive"] is True
        assert term_1["pane"] == pane_b

        exec_result = run_in_pane(["exec", "term-1", "echo terminal-ok"])
        assert exec_result.returncode == 0, exec_result.stdout
        assert "Sent to term-1" in exec_result.stdout

        def terminal_capture() -> str:
            return run_tmux(["capture-pane", "-t", pane_b, "-p", "-S", "-20"]).stdout

        wait_for(lambda: "terminal-ok" in terminal_capture())
        assert "terminal-ok" in terminal_capture()

        remove_result = run_in_pane(["terminal", "remove", "term-1"])
        assert remove_result.returncode == 0, remove_result.stdout
        assert "Terminal 'term-1' removed." in remove_result.stdout
        who_after_remove = run_in_pane(["who"])
        assert who_after_remove.returncode == 0, who_after_remove.stdout
        remaining_members = json.loads(who_after_remove.stdout)["members"]
        assert not any(member["name"] == "term-1" for member in remaining_members)

        add_result = run_in_pane(["terminal", "add", "shell", "--pane", pane_b])
        assert add_result.returncode == 0, add_result.stdout
        assert "Terminal 'shell' registered" in add_result.stdout
        who_after_add = run_in_pane(["who"])
        assert who_after_add.returncode == 0, who_after_add.stdout
        shell = next(member for member in json.loads(who_after_add.stdout)["members"] if member["name"] == "shell")
        assert shell["role"] == "terminal"
        assert shell["alive"] is True
        assert shell["pane"] == pane_b

        tags = run_tmux(["list-panes", "-t", init_payload["window"], "-F", "#{pane_id} role=#{@hive-role} agent=#{@hive-agent} team=#{@hive-team}"]).stdout
        assert f"{pane_a} role=terminal agent=orch team={team_name}" in tags
        assert f"{pane_b} role=terminal agent=shell team={team_name}" in tags

        delete_result = run_in_pane(["delete", team_name])
        assert delete_result.returncode == 0, delete_result.stdout
        assert f"Team '{team_name}' deleted." in delete_result.stdout
        session_check = subprocess.run(["tmux", "has-session", "-t", session], capture_output=True, text=True)
        assert session_check.returncode == 0
        assert not ((workdir / ".hive" / "teams" / team_name).exists())
    finally:
        subprocess.run(["tmux", "kill-session", "-t", session], capture_output=True, text=True)
        shutil.rmtree(workdir, ignore_errors=True)
