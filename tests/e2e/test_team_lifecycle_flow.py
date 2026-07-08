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
)


@pytest.mark.skipif(shutil.which("tmux") is None, reason="tmux is required for e2e tests")
def test_e2e_create_team_inspect_and_delete(tmp_path: Path):
    """Real-tmux smoke for the CLI-only team lifecycle: create a team from a
    tmux pane, read it back with `hive team`, delete it. Agent spawn/delivery
    flows need a live agent CLI and are covered at the cli layer with mocks."""
    workdir = Path(tempfile.mkdtemp(prefix="hive-e2e-", dir="/tmp"))
    env = base_env(workdir)
    team = f"e2e-{uuid.uuid4().hex[:8]}"
    session = f"hive-e2e-{uuid.uuid4().hex[:8]}"
    workspace = workdir / "ws"

    pane_a = run_tmux(["new-session", "-d", "-s", session, "-x", "120", "-y", "40", "-P", "-F", "#{pane_id}"]).stdout.strip()

    def run_in_pane(args: list[str]) -> subprocess.CompletedProcess[str]:
        return run_hive_in_tmux_pane(pane_a, args, env=env, cwd=workdir)

    try:
        create_result = run_in_pane(["create", team, "--workspace", str(workspace)])
        assert create_result.returncode == 0, create_result.stdout
        assert f"Team '{team}' created." in create_result.stdout

        team_result = run_in_pane(["team"])
        assert team_result.returncode == 0, team_result.stdout
        team_payload = json.loads(team_result.stdout)
        assert team_payload["name"] == team
        assert team_payload["self"] == "orch"
        assert team_payload["members"] == []  # shell-pane create: no agents yet

        delete_result = run_in_pane(["delete", team])
        assert delete_result.returncode == 0, delete_result.stdout
        assert f"Team '{team}' deleted." in delete_result.stdout
        assert not ((workdir / ".hive" / "teams" / team).exists())
    finally:
        subprocess.run(["tmux", "kill-session", "-t", session], capture_output=True, text=True)
        shutil.rmtree(workdir, ignore_errors=True)
