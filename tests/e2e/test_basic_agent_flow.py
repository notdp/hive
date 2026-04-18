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
def test_e2e_create_spawn_send_capture_and_status(tmp_path: Path):
    workdir = Path(tempfile.mkdtemp(prefix="hive-e2e-", dir="/tmp"))
    fake_droid = write_fake_droid(workdir)
    env = base_env(workdir, fake_droid)
    team = f"e2e-{uuid.uuid4().hex[:8]}"
    session = f"hive-e2e-{uuid.uuid4().hex[:8]}"
    workspace = workdir / "ws"
    artifact = workdir / "root-message.md"
    artifact.write_text("hello from artifact")

    pane_a = run_tmux(["new-session", "-d", "-s", session, "-x", "120", "-y", "40", "-P", "-F", "#{pane_id}"]).stdout.strip()

    def run_in_pane(args: list[str]) -> subprocess.CompletedProcess[str]:
        return run_hive_in_tmux_pane(pane_a, args, env=env, cwd=workdir)

    def capture_claude() -> str:
        return run_tmux(["capture-pane", "-t", claude_pane, "-p", "-S", "-80"]).stdout

    try:
        create_result = run_in_pane(["create", team, "--workspace", str(workspace)])
        assert create_result.returncode == 0, create_result.stdout
        assert f"Team '{team}' created." in create_result.stdout

        spawn_result = run_in_pane(["spawn", "claude", "--cwd", str(workdir), "--skill", "none", "--cli", "droid"])
        assert spawn_result.returncode == 0, spawn_result.stdout
        assert "Agent 'claude' spawned in pane" in spawn_result.stdout

        team_result = run_in_pane(["team"])
        assert team_result.returncode == 0, team_result.stdout
        team_payload = json.loads(team_result.stdout)
        claude_pane = next(member["pane"] for member in team_payload["members"] if member["name"] == "claude")
        wait_for(lambda: "for help" in capture_claude())

        inject_result = run_in_pane(["inject", "claude", "plain ping"])
        assert inject_result.returncode == 0, inject_result.stdout
        assert "Injected raw input into claude." in inject_result.stdout
        send_result = run_in_pane(["send", "claude", "hello envelope", "--artifact", str(artifact)])
        assert send_result.returncode == 0, send_result.stdout
        send_payload = json.loads(send_result.stdout)
        assert send_payload["to"] == "claude"
        assert send_payload["state"] in {"queued", "pending", "confirmed"}

        wait_for(lambda: "plain ping" in capture_claude() and "hello envelope" in capture_claude())
        captured = capture_claude()
        assert "<HIVE from=orch to=claude msgId=" in captured
        assert "plain ping" in captured

        delete_result = run_in_pane(["delete", team])
        assert delete_result.returncode == 0, delete_result.stdout
        assert f"Team '{team}' deleted." in delete_result.stdout
        assert not ((workdir / ".hive" / "teams" / team).exists())
    finally:
        subprocess.run(["tmux", "kill-session", "-t", session], capture_output=True, text=True)
        shutil.rmtree(workdir, ignore_errors=True)
