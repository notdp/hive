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
def test_e2e_create_team_inspect_and_delete():
    """Real-tmux smoke for the CLI-only team lifecycle: create a team from a
    tmux pane, read it back with `hive team`, delete it. Agent spawn/delivery
    flows need a live agent CLI and are covered at the cli layer with mocks."""
    # Short path: the workspace's hived socket stays in-tree (a long path
    # relocates it under /tmp/hive-<uid>, outside the rmtree below).
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
        registry_entry = workdir / ".hive" / "state" / "teams" / f"{team}.json"
        assert registry_entry.is_file()

        team_result = run_in_pane(["team"])
        assert team_result.returncode == 0, team_result.stdout
        team_payload = json.loads(team_result.stdout)
        assert team_payload["name"] == team
        assert team_payload["self"] == "orch"
        assert team_payload["members"] == []  # shell-pane create: no agents yet

        delete_result = run_in_pane(["delete", team])
        assert delete_result.returncode == 0, delete_result.stdout
        assert not registry_entry.exists()
    finally:
        subprocess.run(["tmux", "kill-session", "-t", session], capture_output=True, text=True)
        shutil.rmtree(workdir, ignore_errors=True)
