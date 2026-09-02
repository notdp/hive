"""Live acceptance rig: one member per CLI through the INSTALLED hive.

Gated behind HIVE_ACCEPTANCE=1 — these tests spawn real agents (real tmux
panes, real claude/codex/grok sessions) against the live install, so they
never run from a plain `pytest tests/`. Run after every install:

    HIVE_ACCEPTANCE=1 PYTHONPATH=src python -m pytest tests/acceptance -q
    HIVE_ACCEPTANCE=1 HIVE_ACCEPTANCE_CLIS=claude,codex,grok \
        PYTHONPATH=src python -m pytest tests/acceptance -q

The rig runs once per session (module fixture): scratch tmux session,
scratch team, one naturally-worded nonce task per CLI dispatched through a
real `hive flow run`. The task wording is deliberately NOT "mechanical, do
not improvise" — drift (acks, misaddressed replies, self-invented scope)
only shows itself when the member has room to move.
"""

from __future__ import annotations

import json
import os
import re
import sqlite3
import subprocess
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path

import pytest

pytestmark = pytest.mark.acceptance

SGR_RE = re.compile(r"\x1b\[[0-9;]*m")


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
    flow_stdout: str = ""
    flow_rc: int = 0
    bus_rows: list[tuple] = field(default_factory=list)
    member_panes: dict[str, str] = field(default_factory=dict)  # member -> pane id

    def member(self, cli: str) -> str:
        return f"probe-{cli}"

    def want(self, cli: str) -> str:
        return f"{self.nonce}-{cli}"

    def dispatches_for(self, member: str) -> list[tuple]:
        # A refused delivery leaves its bus row behind (the bus is a ledger,
        # not a queue) and the flow retries with a fresh msgId — several
        # same-body dispatch rows are legal. The one that reached the member
        # is whichever its reply anchors.
        return [r for r in self.bus_rows if r[1] == "flow.run" and r[2] == member]

    def replies_for(self, member: str) -> list[tuple]:
        ids = {d[0] for d in self.dispatches_for(member)}
        return [r for r in self.bus_rows if r[3] in ids]

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

        wf = r.root / "workflow.js"
        task = "请把这段口令写进 {path}：{nonce}。写完后把口令原样回报给派发人，顺便说一句你对这个任务的看法。"
        # agent() resolves to the reply ({body, artifact, msgId}); the
        # member's name is the script's own knowledge.
        thunk_lines = "".join(
            "  () => agent("
            + json.dumps(task.format(path=f"{r.root}/{c}.txt", nonce=r.want(c)), ensure_ascii=False)
            + ", { name: " + json.dumps(r.member(c))
            + ", cli: " + json.dumps(c) + " })"
            + ".then((reply) => ({ name: " + json.dumps(r.member(c)) + ", summary: reply.body })),\n"
            for c in r.clis
        )
        wf.write_text(
            "export const meta = { name: 'acceptance', description: 'one member per CLI, nonce causality' }\n"
            "const members = await parallel([\n" + thunk_lines + "])\n"
            # parallel() drops a failed branch to null instead of rejecting;
            # the rc-0 oracle needs the failure back.
            "if (members.some((m) => !m)) throw new Error('a member failed: ' + JSON.stringify(members))\n"
            "for (const m of members) log(`RESULT ${m.name} ${(m.summary || '').slice(0, 100)}`)\n"
            "return members.map((m) => ({ name: m.name, summary: m.summary }))\n"
        )
        # Reproduce the honest parentage: an orch's flow runner lives inside
        # a headless engine — no $TMUX. Only the pinned pane identity rides
        # in, exactly what a spawned daemon's tools get.
        env = dict(os.environ)
        env.pop("TMUX", None)
        env["TMUX_PANE"] = pane
        env.pop("CLAUDE_CODE_MESSAGING_SOCKET", None)
        env.pop("CODEX_THREAD_ID", None)
        proc = subprocess.run(
            ["hive", "flow", "run", str(wf)],
            capture_output=True, text=True,
            timeout=int(os.environ.get("HIVE_ACCEPTANCE_TIMEOUT", "420")),
            env=env,
        )
        # progress ([flow] lines, RESULT logs) rides stderr; stdout is the
        # script's return value
        r.flow_stdout, r.flow_rc = proc.stdout + proc.stderr[-3000:], proc.returncode

        db = r.workspace / "hive.db"
        if db.exists():
            r.bus_rows = sqlite3.connect(db).execute(
                "select msg_id, from_agent, to_agent, in_reply_to, body from messages"
            ).fetchall()
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
        # The registry keeps a headless team alive forever (hived won't
        # exit, member daemons won't reap) — release the name explicitly.
        for cli in r.clis:
            subprocess.run(["hive", "kill", f"{r.team}.{r.member(cli)}"],
                           capture_output=True, timeout=30)
        subprocess.run(["hive", "delete", r.team], capture_output=True, timeout=30)
        _tmux("kill-session", "-t", r.session, check=False)
        subprocess.run(["rm", "-rf", str(r.root)], timeout=15)


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
