"""`hive join` refuses a bare interactive claude pane as a member, with or
without `--no-notify`, and writes nothing before refusing.

A claude pane member must be a hive bg job (a pane↔job binding written by
`hive claude` / spawn). Before this gate, `join --no-notify` skipped the
only check in the way (the join message's reachability) and enrolled the
bare pane with an empty session id. The oracle is the registry entry and
the pane's tmux tags, never the error text: a refusal that left a roster
row or a `@hive-agent` tag behind would be a half-registration.

The "claude" on the target pane is a copy of `sleep` first on PATH: hive
classifies the pane by its current command, and nothing in the gated path
talks to the process.
"""

import json
import os
import shutil
import stat
import subprocess
import tempfile
import uuid
from pathlib import Path

import pytest

from tests.e2e._helpers import (
    base_env,
    hive_binary_argv,
    kill_private_server,
    run_hive_in_tmux_pane,
    run_tmux,
    send_tmux_command,
    wait_for,
)


def _pane_tags(pane_id: str, env: dict[str, str]) -> tuple[str, str, str]:
    row = run_tmux(
        ["-u", "display-message", "-p", "-t", pane_id, "#{@hive-role}\t#{@hive-agent}\t#{@hive-team}"],
        env=env,
    ).stdout.rstrip("\n")
    role, agent, team = (row.split("\t") + ["", "", ""])[:3]
    return role, agent, team


def _members(registry_entry: Path) -> list[str]:
    return [m["name"] for m in json.loads(registry_entry.read_text()).get("members", [])]


@pytest.mark.skipif(shutil.which("tmux") is None, reason="tmux is required for e2e tests")
def test_e2e_join_refuses_a_bare_claude_pane_with_and_without_notify():
    workdir = Path(tempfile.mkdtemp(prefix="hive-e2e-", dir="/tmp"))
    bindir = workdir / "bin"
    bindir.mkdir()
    # A `claude` that is nothing but a process named claude on the pane tty:
    # a copy of sleep (a script would show its interpreter as the pane's
    # current command).
    stub = bindir / "claude"
    shutil.copy(shutil.which("sleep"), stub)
    stub.chmod(stub.stat().st_mode | stat.S_IXUSR)

    env = {
        **base_env(workdir),
        "CLAUDE_CONFIG_DIR": str(workdir / ".claude"),
        "CLAUDE_HOME": str(workdir / ".claude"),
    }
    team = f"e2e-{uuid.uuid4().hex[:8]}"
    session = f"hive-e2e-{uuid.uuid4().hex[:8]}"
    workspace = workdir / "ws"
    registry_entry = workdir / ".hive" / "teams" / team / "team.json"

    pane_shell = run_tmux(
        ["new-session", "-d", "-s", session, "-x", "160", "-y", "48", "-c", str(workdir), "-P", "-F", "#{pane_id}", "/bin/sh"],
        env=env,
    ).stdout.strip()

    def hive(args: list[str]) -> subprocess.CompletedProcess[str]:
        return run_hive_in_tmux_pane(pane_shell, args, env=env, cwd=workdir)

    try:
        create = hive(["create", team, "--workspace", str(workspace)])
        assert create.returncode == 0, create.stdout
        assert _members(registry_entry) == []

        # The bare claude: split into the team window, PATH-first stub.
        pane_claude = run_tmux(
            ["split-window", "-t", pane_shell, "-d", "-c", str(workdir), "-P", "-F", "#{pane_id}", "/bin/sh"],
            env=env,
        ).stdout.strip()
        send_tmux_command(pane_claude, f"export PATH={bindir}:$PATH; exec claude 300", env=env)
        wait_for(
            lambda: run_tmux(["display-message", "-p", "-t", pane_claude, "#{pane_current_command}"], env=env).stdout.strip()
            == "claude",
            timeout=15.0,
        )

        # Pane options inherit the team window's `@hive-team`; the member
        # tags a registration writes are `@hive-role` / `@hive-agent`.
        untouched = _pane_tags(pane_claude, env)
        assert untouched[:2] == ("", ""), untouched
        # The pane context a registration would save for the target.
        context_file = workdir / ".hive" / "contexts" / f"{pane_claude.replace('%', 'pane-')}.json"
        # The caller's pane carries a job binding of its own: the gate must
        # read the target's, and a caller's binding must not vouch for it.
        control_dir = workdir / ".claude" / "hive-control"
        control_dir.mkdir(parents=True)
        (control_dir / f"hive-pane-{pane_shell.lstrip('%')}.job").write_text(
            json.dumps({"jobId": "caller0", "sessionId": "", "cwd": str(workdir)})
        )

        for flags in ([], ["--notify"], ["--no-notify"]):
            joined = hive(["join", team, "--pane", pane_claude, "--as", "bare", *flags])
            assert joined.returncode != 0, (flags, joined.stdout)
            assert "background-job binding" in joined.stdout, (flags, joined.stdout)
            # Nothing was written for the target: no roster row, no member
            # tags, no pane context.
            assert _members(registry_entry) == [], (flags, registry_entry.read_text())
            assert _pane_tags(pane_claude, env) == untouched, flags
            assert not context_file.exists(), flags

        # A shell pane in the same window is still refused as a non-agent
        # pane, not by the claude gate: the gate is claude-specific.
        pane_other = run_tmux(
            ["split-window", "-t", pane_shell, "-d", "-c", str(workdir), "-P", "-F", "#{pane_id}", "/bin/sh"],
            env=env,
        ).stdout.strip()
        joined = hive(["join", team, "--pane", pane_other, "--no-notify"])
        assert joined.returncode != 0, joined.stdout
        assert "background-job binding" not in joined.stdout, joined.stdout
        assert _members(registry_entry) == []
    finally:
        subprocess.run(
            [*hive_binary_argv(), "delete", team, "--delete-workspace"],
            env={**os.environ, **env},
            cwd=workdir,
            capture_output=True,
            text=True,
            timeout=60,
        )
        kill_private_server(env)
        shutil.rmtree(workdir, ignore_errors=True)
