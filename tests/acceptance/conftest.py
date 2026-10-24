"""Live acceptance rig: one member per CLI through the INSTALLED hive.

Gated behind HIVE_ACCEPTANCE=1 — these tests spawn real agents (real tmux
panes, real claude/codex/grok sessions) against the live install, so they
never run from a plain `pytest tests/`. Run after every install:

    HIVE_ACCEPTANCE=1 python -m pytest tests/acceptance -q
    HIVE_ACCEPTANCE=1 HIVE_ACCEPTANCE_CLIS=codex,grok \
        python -m pytest tests/acceptance -q

The rig runs once per session (module fixture): scratch tmux session,
scratch team whose orch is a claude engine with a real inbox — the e2e
suite's stub `claude` (`tests/e2e/_stub_claude.py`) under the managed
launcher in the rig's pane, so a member's report has someone to land on —
and one naturally-worded nonce task per CLI, handed over the way an orch
hands one — `hive spawn --task` from the orch engine's own tool context,
every member spawned concurrently. A shell pane is no orch: nothing there
receives a report. The task wording is
deliberately NOT "mechanical, do not improvise" — drift (acks, stray
sends, self-invented scope) only shows itself when the member has room to
move.

The member does the task and reports back to its sender with `hive send`,
as the member protocol says. The oracle does not take any one artifact's
word for that: it reads the ledger (one dispatch in, one report out, no
ack), the frames the orch engine's inbox received (the delivery), the
nonce file the task names, and — the one transcript read — how often the
task envelope landed in the member's own conversation, by the engine
session resolved from its registry row (`member_transcripts`).
"""

from __future__ import annotations

import json
import os
import shutil
import signal
import sqlite3
import stat
import subprocess
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path

import pytest

from member_transcripts import count_dispatch_inputs, engine_session

pytestmark = pytest.mark.acceptance

STUB_CLAUDE = Path(__file__).resolve().parents[1] / "e2e" / "_stub_claude.py"


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
    pane: str = ""  # the rig's own pane, where the orch engine runs
    orch_cfg: Path = Path()  # the stub orch's CLAUDE_CONFIG_DIR
    orch_job: str = ""  # the stub orch's job id
    orch_env: dict[str, str] = field(default_factory=dict)  # the orch engine's tool context
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

    def inbox_frames(self, member: str) -> list[dict]:
        """Frames the orch engine's inbox received from *member*."""
        journal = self.orch_cfg / "stub" / f"inbox-{self.orch_job}.jsonl"
        if not journal.exists():
            return []
        frames = [json.loads(line) for line in journal.read_text().splitlines() if line.strip()]
        return [f for f in frames if f.get("type") == "user" and f.get("from") == f"{self.team}.{member}"]

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
        workspace=Path(),
    )
    r.root.mkdir(parents=True)
    (r.root / "tasks").mkdir()

    bindir = r.root / "bin"
    bindir.mkdir()
    stub = bindir / "claude"
    shutil.copy(STUB_CLAUDE, stub)
    stub.chmod(stub.stat().st_mode | stat.S_IXUSR)
    r.orch_cfg = r.root / "orch-claude"

    _tmux("new-session", "-d", "-s", r.session, "-x", "220", "-y", "50", "-c", str(r.root))
    pane = _tmux("display", "-t", f"{r.session}:", "-p", "#{pane_id}").strip()
    r.pane = pane
    try:
        # The orch: the managed launcher in the rig's pane, with the stub
        # answering as `claude` — a bg engine with an inbox socket.
        _tmux("send-keys", "-t", pane, "-l",
              f"export CLAUDE_CONFIG_DIR={r.orch_cfg} PATH={bindir}:$PATH && hive claude")
        _tmux("send-keys", "-t", pane, "Enter")
        r.orch_job = _wait_for(lambda: next(iter(_stub_jobs(r.orch_cfg)), {}).get("id", ""), 30,
                               "the stub orch never minted a job")
        engine = _wait_for(lambda: _stub_json(r.orch_cfg, f"engine-{r.orch_job}.json"), 30,
                           "the stub orch engine never registered")

        # Every orch verb runs the way the orch engine's tool subprocess
        # does: outside any tmux client, its inbox socket the identity. The
        # team's hived, started by create, inherits this context, so no
        # real claude member is spawned into this team.
        r.orch_env = {
            k: v for k, v in os.environ.items()
            if not k.startswith(("TMUX", "CLAUDE", "ANTHROPIC"))
        }
        if os.environ.get("TMUX_TMPDIR"):
            r.orch_env["TMUX_TMPDIR"] = os.environ["TMUX_TMPDIR"]
        r.orch_env.update({
            "CLAUDE_CONFIG_DIR": str(r.orch_cfg),
            "PATH": f"{bindir}:{os.environ['PATH']}",
            "CLAUDE_CODE_MESSAGING_SOCKET": engine["socket"],
        })
        _wait_for(lambda: _orch_pane(r) == pane, 30, "hive never bound the stub orch to its pane")
        # An orch's create keeps the workspace in the team's own directory.
        created = subprocess.run(
            ["hive", "create", r.team],
            capture_output=True, text=True, timeout=120, env=r.orch_env,
        )
        if created.returncode != 0:
            raise RuntimeError(f"hive create failed: {created.stdout[-500:]}{created.stderr[-500:]}")
        r.workspace = Path(_json_object(created.stdout)["workspace"])

        task = (
            "请把这段口令写进 {path}：{nonce}。写完后把口令 {nonce} 报给派你任务的人，"
            "顺便说一句你对这个任务的看法。"
        )
        procs = {}
        for c in r.clis:
            member = r.member(c)
            r.task_path(member).write_text(task.format(path=f"{r.root}/{c}.txt", nonce=r.want(c)) + "\n")
            procs[member] = subprocess.Popen(
                ["hive", "spawn", member, "-t", r.team, "--cli", c, "--task", str(r.task_path(member))],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=r.orch_env,
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
        # create moved the orch into the team's own session; the member
        # panes are found by their team tag wherever they sit.
        for line in _tmux("list-panes", "-a", "-F",
                          "#{pane_id} #{@hive-team} #{@hive-agent}", check=False).splitlines():
            pid, team, name = (line.strip().split(" ") + ["", ""])[:3]
            if team == r.team and name:
                r.member_panes[name] = pid

        yield r
    finally:
        # The registry keeps the team alive (hived won't exit, member
        # daemons won't reap) — retire every member and the team, from the
        # orch's context so its own job is stopped by the stub.
        subprocess.run(["hive", "delete", r.team, "--down"], capture_output=True, timeout=60,
                       env=r.orch_env or None)
        for pid in _stub_pids(r.orch_cfg):
            try:
                os.kill(pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        _tmux("kill-session", "-t", r.session, check=False)
        _tmux("kill-session", "-t", r.team, check=False)
        subprocess.run(["rm", "-rf", str(r.root)], timeout=15)


def _wait_for(probe, timeout: float, failure: str):
    """*probe*'s first truthy answer within *timeout* seconds."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        got = probe()
        if got:
            return got
        time.sleep(0.5)
    raise RuntimeError(failure)


def _stub_jobs(cfg: Path) -> list[dict]:
    ledger = cfg / "stub" / "jobs.json"
    try:
        return json.loads(ledger.read_text())
    except (OSError, ValueError):
        return []


def _stub_json(cfg: Path, name: str) -> dict | None:
    try:
        return json.loads((cfg / "stub" / name).read_text())
    except (OSError, ValueError):
        return None


def _stub_pids(cfg: Path) -> list[int]:
    """Every stub process: each invocation's own pid and each engine's."""
    pids: list[int] = []
    log = cfg / "stub" / "argv.jsonl"
    if log.exists():
        pids += [json.loads(line)["pid"] for line in log.read_text().splitlines() if line.strip()]
    for engine in (cfg / "stub").glob("engine-*.json"):
        try:
            pids.append(int(json.loads(engine.read_text())["pid"]))
        except (OSError, ValueError, KeyError):
            pass
    return [p for p in pids if p != os.getpid()]


def _orch_pane(r: Rig) -> str:
    """The pane `hive team` resolves the orch engine's tool context to."""
    probe = subprocess.run(["hive", "team"], capture_output=True, text=True, timeout=30, env=r.orch_env)
    if probe.returncode != 0:
        return ""
    return str(_json_object(probe.stdout).get("tmux", {}).get("currentPane", ""))


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
