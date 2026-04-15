import json
import shlex
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
    send_tmux_command,
    wait_for,
    write_fake_droid,
)


@pytest.mark.skipif(
    shutil.which("tmux") is None or shutil.which("zsh") is None,
    reason="tmux and zsh are required for e2e tests",
)
def test_e2e_fork_join_as_prompt_registers_agent_and_delivers_prompt(tmp_path: Path):
    workdir = Path(tempfile.mkdtemp(prefix="hive-e2e-", dir="/tmp"))
    zdotdir = workdir / "zdot"
    zdotdir.mkdir()
    fake_droid = write_fake_droid(workdir)
    droid_bin = workdir / "droid"
    droid_bin.write_text(fake_droid.read_text())
    droid_bin.chmod(0o755)
    (zdotdir / ".zshrc").write_text(
        f'export PATH={shlex.quote(str(workdir))}:$PATH\n'
        'PROMPT="%~ » "\n'
    )

    env = base_env(workdir, droid_bin)
    session = f"hive-e2e-{uuid.uuid4().hex[:8]}"
    team = f"e2e-fork-{uuid.uuid4().hex[:8]}"
    workspace = workdir / "ws"
    pane_a = run_tmux(["new-session", "-d", "-s", session, "-x", "160", "-y", "40", "-P", "-F", "#{pane_id}"]).stdout.strip()
    run_tmux(["set-option", "-t", session, "default-shell", shutil.which("zsh") or "zsh"])
    run_tmux(["set-environment", "-t", session, "ZDOTDIR", str(zdotdir)])

    def run_in_pane(args: list[str], *, timeout: float = 20.0) -> subprocess.CompletedProcess[str]:
        return run_hive_in_tmux_pane(pane_a, args, env=env, cwd=workdir, timeout=timeout)

    def capture(pane_id: str) -> str:
        return run_tmux(["capture-pane", "-t", pane_id, "-p", "-S", "-120"]).stdout

    try:
        create_result = run_in_pane(["create", team, "--workspace", str(workspace)])
        assert create_result.returncode == 0, create_result.stdout

        pane_b = run_tmux(["split-window", "-t", pane_a, "-d", "-h", "-P", "-F", "#{pane_id}"]).stdout.strip()
        send_tmux_command(
            pane_b,
            f"cd {shlex.quote(str(workdir))} && {shlex.quote(str(droid_bin))} --fork source-sess",
        )
        wait_for(lambda: "for help" in capture(pane_b), timeout=10.0)

        register_result = run_in_pane(["register", pane_b, "--as", "droid-1", "--no-notify"])
        assert register_result.returncode == 0, register_result.stdout
        register_payload = json.loads(register_result.stdout)
        assert register_payload == {
            "registered": "droid-1",
            "role": "agent",
            "pane": pane_b,
            "team": team,
        }

        prompt = "run hive thread Veh9; then reply-to lulu"
        fork_result = run_in_pane(
            [
                "fork",
                "--pane",
                pane_b,
                "-s",
                "h",
                "--join-as",
                "droid-2",
                "--prompt",
                prompt,
            ],
            timeout=30.0,
        )
        assert fork_result.returncode == 0, fork_result.stdout

        fork_payload = json.loads(fork_result.stdout)
        new_pane = fork_payload["pane"]
        assert fork_payload == {
            "pane": new_pane,
            "registered": "droid-2",
            "team": team,
        }

        wait_for(
            lambda: "droid --fork source-sess" in capture(new_pane) and f"RECV:{prompt}" in capture(new_pane),
            timeout=10.0,
        )

        who_result = run_in_pane(["who"])
        assert who_result.returncode == 0, who_result.stdout
        members = {member["name"]: member for member in json.loads(who_result.stdout)["members"]}
        assert members["droid-1"]["pane"] == pane_b
        assert members["droid-2"]["pane"] == new_pane
        assert members["droid-2"]["role"] == "agent"

        tag_state = run_tmux(
            ["display-message", "-t", new_pane, "-p", "#{@hive-role}|#{@hive-agent}|#{@hive-team}|#{@hive-cli}"]
        ).stdout.strip()
        assert tag_state == f"agent|droid-2|{team}|droid"
    finally:
        subprocess.run(["tmux", "kill-session", "-t", session], capture_output=True, text=True)
        shutil.rmtree(workdir, ignore_errors=True)
