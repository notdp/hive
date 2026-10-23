"""Live acceptance rig: one member per CLI through the INSTALLED hive.

Gated behind HIVE_ACCEPTANCE=1 — these tests spawn real agents (real tmux
panes, real claude/codex/grok sessions) against the live install, so they
never run from a plain `pytest tests/`. Run after every install:

    HIVE_ACCEPTANCE=1 python -m pytest tests/acceptance -q
    HIVE_ACCEPTANCE=1 HIVE_ACCEPTANCE_CLIS=codex,grok \
        python -m pytest tests/acceptance -q

The rig runs once per session (module fixture): scratch tmux session,
scratch team whose orch is the rig's own pane, and one naturally-worded
nonce task per CLI, handed over the way an orch hands one — `hive spawn
--task`, every member spawned concurrently. The task wording is
deliberately NOT "mechanical, do not improvise" — drift (acks, stray
sends, self-invented scope) only shows itself when the member has room to
move.

The member does the task and reports back to its sender with `hive send`,
as the member protocol says. The oracle does not take any one artifact's
word for that: it reads the ledger (one dispatch in, one report out, no
ack), the nonce file the task names, and — the one transcript read — how
often the task envelope landed in the member's own conversation, by the
engine session resolved from its registry row (`member_transcripts`).
"""

from __future__ import annotations

import json
import os
import sqlite3
import subprocess
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path

import pytest

from member_transcripts import count_dispatch_inputs, engine_session

pytestmark = pytest.mark.acceptance


def pytest_collection_modifyitems(config, items):
    if os.environ.get("HIVE_ACCEPTANCE") == "1":
        return
    skip = pytest.mark.skip(reason="live acceptance: set HIVE_ACCEPTANCE=1 (spawns real agents)")
    for item in items:
        if item.get_closest_marker("acceptance"):
            item.add_marker(skip)


def _tmux(*args: str, check: bool = True) -> str:
    r = subprocess.run(["tmux", *args], capture_output=True, text=True, timeout=20)
    if check and r.returncode != 0:
        raise RuntimeError(f"tmux {' '.join(args)}: {r.stderr.strip()}")
    return r.stdout


@dataclass
class Rig:
    clis: list[str]
    nonce: str
    root: Path
    session: str
    team: str
    workspace: Path
    pane: str = ""  # the rig's own pane: the orch identity a spawn is issued under
    spawn_log: str = ""  # every spawn's stdout/stderr tail
    spawn_rcs: dict[str, int] = field(default_factory=dict)  # member -> spawn exit code
    spawn_results: dict[str, dict] = field(default_factory=dict)  # member -> spawn JSON
    bus_rows: list[tuple] = field(default_factory=list)  # (seq, from, to, body, artifact)
    member_panes: dict[str, str] = field(default_factory=dict)  # member -> pane id
    roster: dict[str, dict] = field(default_factory=dict)  # member -> registry row
    dispatch_inputs: dict[str, int] = field(default_factory=dict)  # member -> transcript input records carrying the task path

    def member(self, cli: str) -> str:
        return f"probe-{cli}"

    def want(self, cli: str) -> str:
        return f"{self.nonce}-{cli}"

    def task_path(self, member: str) -> Path:
        # Resolved: spawn canonicalizes the task path before it rides the
        # ledger and the envelope (/tmp is /private/tmp on macOS).
        return (self.root / "tasks" / f"{member}.md").resolve()

    def dispatch_rows(self, member: str) -> list[tuple]:
        # The task's one ledger write: the member as recipient, its task
        # file as the artifact.
        task = str(self.task_path(member))
        return [r for r in self.bus_rows if _bare(r[2]) == member and str(r[4]) == task]

    def rows_from(self, member: str) -> list[tuple]:
        return [r for r in self.bus_rows if _bare(r[1]) == member]

    def report_text(self, member: str) -> str:
        """The member's report rows, body and artifact file content alike."""
        parts: list[str] = []
        for r in self.rows_from(member):
            parts.append(str(r[3]))
            if r[4]:
                try:
                    parts.append(Path(str(r[4])).read_text(errors="replace"))
                except OSError:
                    pass
        return "\n".join(parts)

    def capture(self, member: str, *, escapes: bool) -> str:
        pane = self.member_panes.get(member, "")
        if not pane:
            return ""
        args = ["capture-pane", "-t", pane, "-p"] + (["-e"] if escapes else [])
        return _tmux(*args, check=False)

    def capture_visible(self, member: str) -> str:
        """Pane text with the client's ghost predictions dropped.

        The claude TUI pre-renders a predicted next input in dim text (the
        human sees gray; a plain capture sees ordinary characters). Capture
        with escapes and drop dim cells so a screen-reading oracle never
        mistakes a prediction for typed input. (Inlined from the retired
        Python hive.draft_guard SGR parser; the Rust port carries the same
        logic in draft_guard.rs.)
        """
        raw = self.capture(member, escapes=True)
        return "\n".join(_drop_dim_cells(line) for line in raw.splitlines())


@pytest.fixture(scope="session")
def rig():
    clis = [c.strip() for c in os.environ.get("HIVE_ACCEPTANCE_CLIS", "codex,grok").split(",") if c.strip()]
    run_id = uuid.uuid4().hex[:6]
    r = Rig(
        clis=clis,
        nonce=f"acc-{run_id}",
        root=Path(f"/tmp/hacc-{run_id}"),  # short: the workspace carries a unix socket
        session=f"hive-acc-{run_id}",
        team=f"acc-{run_id}",
        workspace=Path(f"/tmp/hacc-{run_id}/ws"),
    )
    r.root.mkdir(parents=True)
    (r.root / "tasks").mkdir()
    jobs_before = {p.name for p in Path.home().glob(".claude/jobs/*")}

    _tmux("new-session", "-d", "-s", r.session, "-x", "220", "-y", "50", "-c", str(r.root))
    pane = _tmux("display", "-t", f"{r.session}:", "-p", "#{pane_id}").strip()
    r.pane = pane
    try:
        _tmux("send-keys", "-t", pane, "-l",
              f"hive create {r.team} --workspace {r.workspace} && touch {r.root}/created")
        _tmux("send-keys", "-t", pane, "Enter")
        deadline = time.time() + 30
        while not (r.root / "created").exists():
            if time.time() > deadline:
                raise RuntimeError("hive create never finished in the rig pane")
            time.sleep(1)

        task = (
            "请把这段口令写进 {path}：{nonce}。写完后把口令 {nonce} 报给派你任务的人，"
            "顺便说一句你对这个任务的看法。"
        )
        # The rig's pane is the team's orch: every spawn signs as it. No
        # $TMUX — the pinned pane identity is what addresses the server.
        env = dict(os.environ)
        env.pop("TMUX", None)
        env["TMUX_PANE"] = pane
        env.pop("CLAUDE_CODE_MESSAGING_SOCKET", None)
        env.pop("CODEX_THREAD_ID", None)
        procs = {}
        for c in r.clis:
            member = r.member(c)
            r.task_path(member).write_text(task.format(path=f"{r.root}/{c}.txt", nonce=r.want(c)) + "\n")
            procs[member] = subprocess.Popen(
                ["hive", "spawn", member, "-t", r.team, "--cli", c, "--task", str(r.task_path(member))],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env,
            )
        for member, proc in procs.items():
            out, err = proc.communicate(timeout=240)
            r.spawn_rcs[member] = proc.returncode
            r.spawn_results[member] = _json_object(out)
            r.spawn_log += f"--- {member} (rc {proc.returncode}) ---\n{out[-1500:]}\n{err[-1500:]}\n"

        # Wait for every member's report, then a grace period so a stray
        # second send after the report is seen too.
        db = r.workspace / "hive.db"
        deadline = time.time() + int(os.environ.get("HIVE_ACCEPTANCE_TIMEOUT", "420"))
        while time.time() < deadline:
            r.bus_rows = _bus_rows(db)
            if all(r.rows_from(m) for m in procs):
                time.sleep(int(os.environ.get("HIVE_ACCEPTANCE_GRACE", "20")))
                break
            time.sleep(2)
        r.bus_rows = _bus_rows(db)
        # The registry row names the member's engine session; the
        # transcript read counts how often the task envelope landed there.
        r.roster = _registry_members(r.team)
        for member in procs:
            row = r.roster.get(member, {})
            engine = engine_session(str(row.get("cli", "")), str(row.get("sessionId", "")))
            if row and engine:
                r.dispatch_inputs[member] = count_dispatch_inputs(
                    str(row.get("cli", "")), engine, str(row.get("cwd", "")), str(r.task_path(member))
                )
            else:
                r.dispatch_inputs[member] = 0
        for line in _tmux("list-panes", "-t", r.session, "-F",
                          "#{pane_id} #{@hive-agent}", check=False).splitlines():
            pid, _, name = line.strip().partition(" ")
            if name:
                r.member_panes[name] = pid

        yield r
    finally:
        for p in Path.home().glob(".claude/jobs/*"):
            if p.name in jobs_before:
                continue
            try:
                state = json.loads((p / "state.json").read_text())
            except (OSError, ValueError):
                continue
            if str(state.get("name", "")).startswith(f"{r.team}."):
                subprocess.run(["claude", "stop", p.name], capture_output=True, timeout=30)
                subprocess.run(["claude", "rm", p.name], capture_output=True, timeout=30)
        # The registry keeps the team alive (hived won't exit, member
        # daemons won't reap) — retire every member and the team.
        subprocess.run(["hive", "delete", r.team, "--down"], capture_output=True, timeout=60)
        _tmux("kill-session", "-t", r.session, check=False)
        subprocess.run(["rm", "-rf", str(r.root)], timeout=15)


def _bare(address: object) -> str:
    """A ledger address without its team prefix."""
    return str(address).split(".")[-1]


def _bus_rows(db: Path) -> list[tuple]:
    """Every ledger row, or [] before the ledger exists."""
    if not db.exists():
        return []
    return sqlite3.connect(db).execute(
        "select seq, from_agent, to_agent, body, artifact from messages"
    ).fetchall()


def _registry_members(team: str) -> dict[str, dict]:
    """`$HIVE_HOME/teams/<team>/team.json` members by name (cli, sessionId, cwd)."""
    hive_home = Path(os.environ.get("HIVE_HOME") or Path.home() / ".hive")
    try:
        entry = json.loads((hive_home / "teams" / team / "team.json").read_text())
    except (OSError, ValueError):
        return {}
    return {
        str(m.get("name")): m
        for m in entry.get("members", [])
        if isinstance(m, dict) and m.get("name")
    }


def _json_object(stdout: str) -> dict:
    """The JSON object a command printed (pretty or not); {} for anything else."""
    start = stdout.find("{")
    if start == -1:
        return {}
    try:
        parsed = json.loads(stdout[start:])
    except ValueError:
        return {}
    return parsed if isinstance(parsed, dict) else {}


def _drop_dim_cells(line: str) -> str:
    """Drop characters rendered dim (SGR 2) — claude's ghost predictions."""
    out: list[str] = []
    dim = False
    i = 0
    while i < len(line):
        if line[i] == "\x1b" and i + 1 < len(line) and line[i + 1] == "[":
            end = line.find("m", i + 2)
            if end != -1:
                raw = line[i + 2:end]
                params: list[int] = []
                for part in raw.split(";") if raw else ["0"]:
                    if not part:
                        params.append(0)
                        continue
                    try:
                        params.append(int(part))
                    except ValueError:
                        continue
                j = 0
                while j < len(params):
                    code = params[j]
                    if code == 0 or code == 22:
                        dim = False
                    elif code == 2:
                        dim = True
                    elif code in (38, 48) and j + 1 < len(params):
                        mode = params[j + 1]
                        if mode == 2:
                            j += 4
                        elif mode == 5:
                            j += 2
                    j += 1
                i = end + 1
                continue
        if not dim:
            out.append(line[i])
        i += 1
    return "".join(out)
