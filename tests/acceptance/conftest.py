"""Live acceptance rig: one member per CLI through the INSTALLED hive.

Gated behind HIVE_ACCEPTANCE=1 — these tests spawn real agents (real tmux
panes, real claude/codex/grok sessions) against the live install, so they
never run from a plain `pytest tests/`. Run after every install:

    HIVE_ACCEPTANCE=1 python -m pytest tests/acceptance -q
    HIVE_ACCEPTANCE=1 HIVE_ACCEPTANCE_CLIS=claude,codex,grok \
        python -m pytest tests/acceptance -q

The rig runs once per session (module fixture): scratch tmux session,
scratch team, one naturally-worded nonce task per CLI, each driven the way
a Claude Code Workflow drives a node — one concurrent `hive node run`
subprocess per member, task on stdin, one JSON line back on stdout. The
task wording is deliberately NOT "mechanical, do not improvise" — drift
(acks, stray replies, self-invented scope) only shows itself when the
member has room to move.

The oracle does not take the node's word for it: after the nodes return,
the rig resolves each member's engine session from its registry row
(`member_transcripts.engine_session` — a claude row holds the bg job id,
and the engine session behind it comes from the job's own state file, not
from the node's answer), reads the member's own transcript by that id, and
the tests compare the node JSON against what the engine wrote. The task
carries two decoys for that oracle: the nonce itself appears in the task
text (a reader that grabbed the input record instead of the final message
would still "find" it) and a bait string the member is told never to
repeat.
"""

from __future__ import annotations

import json
import os
import sqlite3
import subprocess
import threading
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path

import pytest

from member_transcripts import BoundTurn, engine_session, read_member_turn

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
    flow_stdout: str = ""  # every node's stderr tail + one RESULT line per member
    flow_rc: int = 0  # the first non-zero node exit code, 0 when every node exited 0
    node_rcs: dict[str, int] = field(default_factory=dict)  # member -> raw exit code
    node_results: dict[str, dict] = field(default_factory=dict)  # member -> node run JSON
    bus_rows: list[tuple] = field(default_factory=list)  # (seq, from, to, body, artifact)
    member_panes: dict[str, str] = field(default_factory=dict)  # member -> pane id
    roster: dict[str, dict] = field(default_factory=dict)  # member -> registry row
    engine_sessions: dict[str, str] = field(default_factory=dict)  # member -> engine session behind the row
    turns: dict[str, BoundTurn] = field(default_factory=dict)  # member -> what its transcript says

    def member(self, cli: str) -> str:
        return f"probe-{cli}"

    def want(self, cli: str) -> str:
        return f"{self.nonce}-{cli}"

    def bait(self, cli: str) -> str:
        return f"bait-{self.nonce[4:]}-{cli}"

    def dispatch_id(self, member: str) -> str:
        return str(self.node_results.get(member, {}).get("dispatchId", ""))

    def dispatch_rows(self, member: str) -> list[tuple]:
        # The node's one ledger write: no sender, the member as recipient,
        # the task artifact named after the dispatch id.
        did = self.dispatch_id(member)
        return [
            r for r in self.bus_rows
            if r[1] == "" and r[2] == member and did and did in str(r[4])
        ]

    def rows_from(self, member: str) -> list[tuple]:
        return [r for r in self.bus_rows if r[1] == member]

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
    clis = [c.strip() for c in os.environ.get("HIVE_ACCEPTANCE_CLIS", "claude").split(",") if c.strip()]
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
    jobs_before = {p.name for p in Path.home().glob(".claude/jobs/*")}

    _tmux("new-session", "-d", "-s", r.session, "-x", "220", "-y", "50", "-c", str(r.root))
    pane = _tmux("display", "-t", f"{r.session}:", "-p", "#{pane_id}").strip()
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
            "请把这段口令写进 {path}：{nonce}。写完后，在你这一轮的最后一条消息里原样写出口令 {nonce}，"
            "顺便说一句你对这个任务的看法。另外有一个干扰词 {bait}——它不是口令，任何地方都不要复述它。"
        )
        # Reproduce the honest parentage: a Workflow's node runner lives
        # inside an engine's tool subprocess — no $TMUX. Only the pinned pane
        # identity rides in, exactly what a spawned daemon's tools get.
        env = dict(os.environ)
        env.pop("TMUX", None)
        env["TMUX_PANE"] = pane
        env.pop("CLAUDE_CODE_MESSAGING_SOCKET", None)
        env.pop("CODEX_THREAD_ID", None)
        # One node per CLI, all started before any is awaited. The task is
        # handed over by communicate(input=…) on a thread per node: writing
        # then closing stdin by hand and calling communicate() afterwards
        # raises "I/O operation on closed file" on Python 3.12.
        procs = {
            r.member(c): subprocess.Popen(
                ["hive", "node", "run", "--team", r.team, "--name", r.member(c), "--cli", c],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                text=True, env=env,
            )
            for c in r.clis
        }
        deadline = time.time() + int(os.environ.get("HIVE_ACCEPTANCE_TIMEOUT", "420"))
        waits = {
            r.member(c): _NodeWait(
                procs[r.member(c)],
                task.format(path=f"{r.root}/{c}.txt", nonce=r.want(c), bait=r.bait(c)),
                deadline,
            )
            for c in r.clis
        }
        for member, wait in waits.items():
            out, err, rc = wait.result()
            # progress rides stderr; stdout is the one JSON result line
            result = _last_json_object(out)
            r.node_results[member] = result
            r.node_rcs[member] = rc
            r.flow_stdout += f"--- {member} (rc {rc}) ---\n{err[-3000:]}\n"
            r.flow_stdout += f"RESULT {member} {str(result.get('body', ''))[:100]}\n"
        # A signal kill is a negative code; max() with 0 would hide it.
        r.flow_rc = next((rc for rc in r.node_rcs.values() if rc != 0), 0)

        db = r.workspace / "hive.db"
        if db.exists():
            r.bus_rows = sqlite3.connect(db).execute(
                "select seq, from_agent, to_agent, body, artifact from messages"
            ).fetchall()
        # The registry row is where the runner learned the member's engine
        # session; the oracle starts from the same row, resolves the engine
        # session on its own (a claude row is a job id — its engine session
        # comes from the job's state file, never from the node's answer),
        # and reads the transcript before teardown retires the members.
        r.roster = _registry_members(r.team)
        for member in procs:
            row = r.roster.get(member, {})
            did = r.dispatch_id(member)
            engine = engine_session(str(row.get("cli", "")), str(row.get("sessionId", "")))
            r.engine_sessions[member] = engine
            if row and did and engine:
                r.turns[member] = read_member_turn(str(row.get("cli", "")), engine, str(row.get("cwd", "")), did)
            else:
                r.turns[member] = BoundTurn()
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
        # daemons won't reap) — retire the run the way a Workflow does.
        subprocess.run(["hive", "delete", r.team, "--down"], capture_output=True, timeout=60)
        _tmux("kill-session", "-t", r.session, check=False)
        subprocess.run(["rm", "-rf", str(r.root)], timeout=15)


class _NodeWait:
    """Feed one node its task and drain it on a thread, so every node is
    fed and running concurrently; a node still running at the deadline is
    killed and reported with what it wrote so far."""

    def __init__(self, proc: subprocess.Popen, task: str, deadline: float):
        self.proc = proc
        self.out = self.err = ""
        self.thread = threading.Thread(target=self._run, args=(task, deadline), daemon=True)
        self.thread.start()

    def _run(self, task: str, deadline: float) -> None:
        try:
            self.out, self.err = self.proc.communicate(task, timeout=max(1.0, deadline - time.time()))
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.out, self.err = self.proc.communicate()
            self.err += f"\n[rig] node run killed at the {os.environ.get('HIVE_ACCEPTANCE_TIMEOUT', '420')}s deadline\n"

    def result(self) -> tuple[str, str, int]:
        self.thread.join()
        return self.out, self.err, self.proc.returncode


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


def _last_json_object(stdout: str) -> dict:
    """The node's result is its last stdout line; anything else is {}."""
    for line in reversed(stdout.splitlines()):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            parsed = json.loads(line)
        except ValueError:
            continue
        if isinstance(parsed, dict):
            return parsed
    return {}


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
