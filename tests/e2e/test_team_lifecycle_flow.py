import json
import os
import shutil
import subprocess
import tempfile
import uuid
from pathlib import Path

import pytest

from tests.e2e._helpers import (
    base_env,
    hive_binary_argv,
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
        registry_entry = workdir / ".hive" / "teams" / team / "team.json"
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


@pytest.mark.skipif(shutil.which("tmux") is None, reason="tmux is required for e2e tests")
def test_e2e_create_outside_tmux_builds_a_session_window_and_delete_closes_it():
    """`hive create` from no tmux client at all (a plain shell, a Workflow's
    Bash) builds a detached session named after the team holding the team
    window, and `hive delete` closes that session again. The commands run
    as plain subprocesses with every tmux and engine marker stripped; the
    session lands on the default server under a unique name, like the
    sessions the other e2e tests create."""
    workdir = Path(tempfile.mkdtemp(prefix="hive-e2e-", dir="/tmp"))
    env = {**os.environ, **base_env(workdir)}
    for key in ("TMUX", "TMUX_PANE"):
        env.pop(key, None)
    team = f"e2e-{uuid.uuid4().hex[:8]}"
    workspace = workdir / "ws"
    registry_entry = workdir / ".hive" / "teams" / team / "team.json"

    def hive(args: list[str]) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [*hive_binary_argv(), *args], env=env, cwd=workdir, capture_output=True, text=True, timeout=60
        )

    try:
        create_result = hive(["create", team, "--workspace", str(workspace)])
        assert create_result.returncode == 0, create_result.stderr
        assert f"Team '{team}' created" in create_result.stdout, create_result.stdout
        run_tmux(["has-session", "-t", f"={team}"])
        # `-u`: a client without a UTF-8 locale gets tabs sanitized to `_`.
        rows = [
            line.split("\t")
            for line in run_tmux(["-u", "list-windows", "-t", f"={team}", "-F", "#{window_id}\t#{@hive-team}"]).stdout.splitlines()
        ]
        assert [row[1] for row in rows] == [team], rows
        entry = json.loads(registry_entry.read_text())
        assert entry["display"] == rows[0][0]
        assert entry["workspace"] == str(workspace)

        team_result = hive(["team", "-t", team])
        assert team_result.returncode == 0, team_result.stderr
        team_payload = json.loads(team_result.stdout)
        assert team_payload["name"] == team
        assert team_payload["tmuxSession"] == team
        assert team_payload["members"] == []

        delete_result = hive(["delete", team, "--delete-workspace"])
        assert delete_result.returncode == 0, delete_result.stderr
        assert not registry_entry.exists()
        assert not workspace.exists()
        # The team window was the session's only window: closing it took
        # the session hive had built with it.
        assert subprocess.run(["tmux", "has-session", "-t", f"={team}"], capture_output=True).returncode != 0
    finally:
        subprocess.run([*hive_binary_argv(), "delete", team, "--delete-workspace"], env=env, cwd=workdir, capture_output=True, text=True, timeout=60)
        subprocess.run(["tmux", "kill-session", "-t", f"={team}"], capture_output=True, text=True)
        shutil.rmtree(workdir, ignore_errors=True)
