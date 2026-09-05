"""Member lifecycle end to end, with a stub `claude` first on PATH.

An orch pane running the managed launcher (`hive claude`), then — from that
orch engine's own tool context — create -> spawn -> the member's own
`hive send orch <nonce>` -> kill -> delete, through the real binary, a real
tmux session of the test's own and the real hived. Only the agent CLI is a
stand-in (`_stub_claude.py`), so no LLM is in the loop. The oracles are
causal, never screen text: the stub's recorded argv (what `claude --bg` was
asked to run), the bus row the member's send left behind, the frame that
landed in the orch engine's inbox (nonce and msgId round trip), the
registry roster, and the `claude stop` the kill issued.
"""

import json
import os
import shutil
import signal
import sqlite3
import stat
import subprocess
import tempfile
import uuid
from pathlib import Path

import pytest

from tests.e2e._helpers import (
    base_env,
    hive_binary_argv,
    run_tmux,
    send_tmux_command,
    wait_for,
)

STUB = Path(__file__).with_name("_stub_claude.py")


def _stub_calls(config_dir: Path) -> list[list[str]]:
    log = config_dir / "stub" / "argv.jsonl"
    if not log.exists():
        return []
    return [json.loads(line)["argv"] for line in log.read_text().splitlines() if line.strip()]


def _stub_jobs(config_dir: Path) -> list[dict]:
    ledger = config_dir / "stub" / "jobs.json"
    return json.loads(ledger.read_text()) if ledger.exists() else []


def _stub_json(config_dir: Path, name: str) -> dict | None:
    path = config_dir / "stub" / name
    return json.loads(path.read_text()) if path.exists() else None


def _inbox_frames(config_dir: Path, job_id: str) -> list[dict]:
    journal = config_dir / "stub" / f"inbox-{job_id}.jsonl"
    if not journal.exists():
        return []
    return [json.loads(line) for line in journal.read_text().splitlines() if line.strip()]


def _bus_rows(db: Path) -> list[tuple]:
    if not db.exists():
        return []
    return sqlite3.connect(db).execute(
        "select msg_id, from_agent, to_agent, in_reply_to, body from messages"
    ).fetchall()


def _stub_pids(config_dir: Path) -> list[int]:
    log = config_dir / "stub" / "argv.jsonl"
    if not log.exists():
        return []
    return [json.loads(line)["pid"] for line in log.read_text().splitlines() if line.strip()]


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


@pytest.mark.skipif(shutil.which("tmux") is None, reason="tmux is required for e2e tests")
def test_e2e_spawn_send_kill_with_a_stub_cli():
    # Short path: the stub engines' unix sockets live under it.
    workdir = Path(tempfile.mkdtemp(prefix="hive-e2e-", dir="/tmp"))
    config_dir = workdir / ".claude"
    bindir = workdir / "bin"
    session = f"hive-e2e-{uuid.uuid4().hex[:8]}"
    try:
        _run_stub_flow(workdir, config_dir, bindir, session)
    finally:
        subprocess.run(["tmux", "kill-session", "-t", session], capture_output=True, text=True)
        for pid in _stub_pids(config_dir):
            if pid != os.getpid() and _pid_alive(pid):
                os.kill(pid, signal.SIGKILL)
        shutil.rmtree(workdir, ignore_errors=True)


def _run_stub_flow(workdir: Path, config_dir: Path, bindir: Path, session: str) -> None:
    bindir.mkdir()
    # The stub answers as `claude`; bare `hive` (what the panes' launch lines
    # and the engines' own tool calls run) is this build.
    stub = bindir / "claude"
    shutil.copy(STUB, stub)
    stub.chmod(stub.stat().st_mode | stat.S_IXUSR)
    os.symlink(hive_binary_argv()[0], bindir / "hive")

    pane_env = {
        **base_env(workdir),
        "CLAUDE_CONFIG_DIR": str(config_dir),
        "PATH": f"{bindir}:{os.environ['PATH']}",
    }
    team = f"e2e-{uuid.uuid4().hex[:8]}"
    nonce = f"nonce-{uuid.uuid4().hex}"
    auto_workspace: Path | None = None

    # Every pane's shell must resolve `hive` and `claude` to bindir first —
    # the initial orch pane, and the split pane `hive spawn` opens for the
    # member (whose launch line runs bare `hive claude`). The non-PATH vars
    # ride `-e` at session creation (inherited by the split too); PATH does
    # not, though — on macOS the pane shell's PATH is the tmux server's, not
    # the session `-e` value (login shells rebuild it via path_helper, and
    # tmux does not let a session PATH override the global one) — so PATH is
    # prepended by the pane command itself: `default-command` wraps a no-rc
    # shell that exports it, and every split inherits that command.
    env_flags = [
        flag for key, value in pane_env.items() if key != "PATH"
        for flag in ("-e", f"{key}={value}")
    ]
    shell = f"/bin/sh -c 'export PATH={bindir}:$PATH; exec /bin/sh'"
    pane_a = run_tmux([
        "new-session", "-d", "-s", session, "-x", "160", "-y", "48", "-c", str(workdir),
        *env_flags, "-P", "-F", "#{pane_id}", shell,
    ]).stdout.strip()
    run_tmux(["set-option", "-t", session, "default-command", shell])

    try:
        # The orch: a human's `hive claude` in the pane. The managed launcher
        # spawns the bg job (stub engine) and attaches the pane to it.
        send_tmux_command(pane_a, "hive claude")
        wait_for(lambda: len(_stub_jobs(config_dir)) == 1, timeout=30.0)
        orch_job = _stub_jobs(config_dir)[0]["id"]
        wait_for(lambda: _stub_json(config_dir, f"engine-{orch_job}.json") is not None, timeout=30.0)
        orch_engine = _stub_json(config_dir, f"engine-{orch_job}.json")

        # Everything below runs the way an orch's tool subprocess does:
        # outside any tmux client, carrying only the engine's inbox socket
        # as identity — hive maps it to the pane the launcher bound the job
        # to.
        orch_env = {
            "HOME": os.environ["HOME"],
            **pane_env,
            "CLAUDE_CODE_MESSAGING_SOCKET": orch_engine["socket"],
        }

        def orch(args: list[str], *, timeout: float = 60.0) -> subprocess.CompletedProcess[str]:
            return subprocess.run(
                ["hive", *args], env=orch_env, cwd=workdir, capture_output=True, text=True, timeout=timeout
            )

        def orch_identity_bound() -> bool:
            probe = orch(["team"])
            if probe.returncode != 0:
                return False
            return json.loads(probe.stdout).get("tmux", {}).get("currentPane") == pane_a

        wait_for(orch_identity_bound, timeout=30.0)

        create_result = orch(["create", team])
        assert create_result.returncode == 0, create_result.stderr
        created = json.loads(create_result.stdout)
        assert created["team"] == team
        assert created["orch"] == {"pane": pane_a, "name": "orch", "cli": "claude"}
        auto_workspace = Path(created["workspace"])
        bus_db = auto_workspace / "hive.db"
        registry_entry = workdir / ".hive" / "state" / "teams" / f"{team}.json"
        assert registry_entry.is_file()

        def roster() -> dict[str, dict]:
            return {m["name"]: m for m in json.loads(registry_entry.read_text())["members"]}

        # A claude member's roster identity is its bg job id (what the pane
        # record carries), not the engine's transcript sessionId.
        assert roster()["orch"]["cli"] == "claude"
        assert roster()["orch"]["sessionId"] == orch_job

        team_result = orch(["team"])
        assert team_result.returncode == 0, team_result.stderr
        team_payload = json.loads(team_result.stdout)
        assert team_payload["self"] == "orch"
        assert [m["name"] for m in team_payload["members"]] == ["orch"]

        spawn_result = orch(["spawn", "worker", "--cli", "claude", "--prompt", nonce])
        assert spawn_result.returncode == 0, spawn_result.stderr
        # The member got a pane of its own, tagged with its name — so the
        # single-pane check after kill below is about a pane that existed.
        tagged = run_tmux(["list-panes", "-t", session, "-F", "#{pane_id} #{@hive-agent}"]).stdout.split("\n")
        tagged = [line for line in tagged if line.strip()]
        assert len(tagged) == 2, tagged
        assert tagged[0].startswith(pane_a), tagged
        assert tagged[1].endswith(" worker"), tagged

        # What `claude --bg` was asked to run for the member: its label and
        # the nonce riding the prompt (last line, after the skill
        # activation).
        member_bg = [c for c in _stub_calls(config_dir) if c[:1] == ["--bg"] and "--name" in c
                     and c[c.index("--name") + 1] == f"{team}.worker"]
        assert len(member_bg) == 1, _stub_calls(config_dir)
        assert member_bg[0][-1].splitlines()[-1] == nonce

        jobs = {row["name"]: row for row in _stub_jobs(config_dir)}
        worker_job = jobs[f"{team}.worker"]["id"]
        worker_engine_pid = jobs[f"{team}.worker"]["pid"]

        # The roster row is keyed by the engine identity the spawn minted.
        assert roster()["worker"]["cli"] == "claude"
        assert roster()["worker"]["sessionId"] == worker_job

        # The member found itself and sent the nonce to orch: the bus row is
        # the ledger receipt, the frame in the orch engine's inbox is the
        # delivery.
        def nonce_rows() -> list[tuple]:
            return [r for r in _bus_rows(bus_db) if r[4] == nonce]

        try:
            wait_for(lambda: len(nonce_rows()) == 1, timeout=60.0)
        except AssertionError as exc:
            raise AssertionError(
                f"no bus row carried the nonce; member outcome: "
                f"{_stub_json(config_dir, f'send-{worker_job}.json')}"
            ) from exc
        (msg_id, from_agent, to_agent, in_reply_to, _body), = nonce_rows()
        assert (from_agent, to_agent, in_reply_to) == ("worker", "orch", "")
        assert msg_id

        wait_for(lambda: _stub_json(config_dir, f"send-{worker_job}.json") is not None, timeout=30.0)
        outcome = _stub_json(config_dir, f"send-{worker_job}.json")
        assert outcome["identity"] == {"self": "worker", "members": ["orch", "worker"]}
        assert outcome["send"]["rc"] == 0, outcome["send"]

        frames = [f for f in _inbox_frames(config_dir, orch_job) if f.get("type") == "user"]
        assert len(frames) == 1, _inbox_frames(config_dir, orch_job)
        frame = frames[0]
        assert frame["from"] == f"{team}.worker"
        assert frame["session_id"] == orch_engine["sessionId"]
        assert nonce in frame["message"]["content"]
        assert f"msgId={msg_id}" in frame["message"]["content"]

        kill_result = orch(["kill", "worker"])
        assert kill_result.returncode == 0, kill_result.stderr
        kill_payload = json.loads(kill_result.stdout)
        assert kill_payload["success"] is True
        assert kill_payload["removedFromTeam"] is True
        assert ["stop", worker_job] in _stub_calls(config_dir)
        wait_for(lambda: not _pid_alive(worker_engine_pid), timeout=10.0)
        assert "worker" not in roster()
        panes = run_tmux(["list-panes", "-t", session, "-F", "#{pane_id}"]).stdout.split()
        assert panes == [pane_a]

        delete_result = orch(["delete", team, "--delete-workspace"])
        assert delete_result.returncode == 0, delete_result.stderr
        assert not registry_entry.exists()
        assert not auto_workspace.exists()
    finally:
        # A failure above may leave the team (and its hived) behind:
        # release the name from the registry side, no identity needed.
        subprocess.run(
            ["hive", "delete", team, "--delete-workspace"],
            env={"HOME": os.environ["HOME"], **pane_env}, cwd=workdir,
            capture_output=True, text=True, timeout=60,
        )
        # Engines from the ledger plus every stub invocation that is still
        # around (an `attach` that never saw EOF would sit here forever).
        leftovers = [row.get("pid") for row in _stub_jobs(config_dir)] + _stub_pids(config_dir)
        for pid in leftovers:
            if pid and pid != os.getpid() and _pid_alive(pid):
                os.kill(pid, signal.SIGKILL)
        subprocess.run(["tmux", "kill-session", "-t", session], capture_output=True, text=True)
        if auto_workspace is not None:
            shutil.rmtree(auto_workspace, ignore_errors=True)
        shutil.rmtree(workdir, ignore_errors=True)
