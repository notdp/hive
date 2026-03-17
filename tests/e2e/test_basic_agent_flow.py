import json
import shutil
import uuid
from pathlib import Path

import pytest

from tests.e2e._helpers import base_env, run_hive, wait_for, write_fake_droid


@pytest.mark.skipif(shutil.which("tmux") is None, reason="tmux is required for e2e tests")
def test_e2e_create_spawn_send_capture_and_status(tmp_path: Path):
    fake_droid = write_fake_droid(tmp_path)
    env = base_env(tmp_path, fake_droid)
    team = f"e2e-{uuid.uuid4().hex[:8]}"
    workspace = tmp_path / "ws"

    result = run_hive(["create", team, "--workspace", str(workspace)], env=env, cwd=tmp_path)
    assert result.returncode == 0, result.stderr or result.stdout

    result = run_hive(["teams"], env=env, cwd=tmp_path)
    assert result.returncode == 0, result.stderr or result.stdout
    payload = json.loads(result.stdout)
    assert any(row["name"] == team for row in payload)

    result = run_hive(["spawn", "claude", "--team", team, "--cwd", str(tmp_path), "--skill", "none"], env=env, cwd=tmp_path)
    assert result.returncode == 0, result.stderr or result.stdout

    wait_for(lambda: "for help" in run_hive(["capture", "claude", "--team", team], env=env, cwd=tmp_path).stdout)

    assert run_hive(["type", "claude", "plain ping", "--team", team], env=env, cwd=tmp_path).returncode == 0
    assert run_hive(["send", "claude", "hello envelope", "--team", team], env=env, cwd=tmp_path).returncode == 0

    def capture() -> str:
        return run_hive(["capture", "claude", "--team", team, "--lines", "80"], env=env, cwd=tmp_path).stdout

    wait_for(lambda: "plain ping" in capture() and "hello envelope" in capture())
    captured = capture()
    assert "<HIVE from=orchestrator to=claude>" in captured
    assert "plain ping" in captured

    result = run_hive(["status-set", "busy", "working", "--team", team, "--workspace", str(workspace), "--agent", "orchestrator"], env=env, cwd=tmp_path)
    assert result.returncode == 0, result.stderr or result.stdout

    result = run_hive(["status-show", "--team", team, "--workspace", str(workspace)], env=env, cwd=tmp_path)
    assert result.returncode == 0, result.stderr or result.stdout
    payload = json.loads(result.stdout)
    assert payload["orchestrator"]["state"] == "busy"

    result = run_hive(["wait-status", "orchestrator", "--team", team, "--workspace", str(workspace), "--state", "busy", "--timeout", "1"], env=env, cwd=tmp_path)
    assert result.returncode == 0, result.stderr or result.stdout
    assert '"state": "busy"' in result.stdout

    result = run_hive(["delete", team], env=env, cwd=tmp_path)
    assert result.returncode == 0, result.stderr or result.stdout
    assert not ((tmp_path / ".hive" / "teams" / team).exists())
