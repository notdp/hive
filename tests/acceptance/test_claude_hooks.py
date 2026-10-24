"""The claude hooks lane against the live install: a hive-spawned claude
member's own engine reports its turn boundaries to the team's hived.

The oracle is the hived's notify log, `<workspace>/run/notify.jsonl`: a
`claude.hook` row naming the member with `hook: turn.complete` can only be
written after the member's engine posted it through the plugin's hooks
module and the hived admitted it (token, instance, roster). Nothing else
writes that row.

One known timing fact is tolerated once, and only once: Claude Code boots
a bg job's engine ahead of its claim, and that spare loaded the plugin
record in force at its boot — right after `hive plugin setup` refreshed
the record, the first spawn can still claim a spare without the module
(`docs/runtime-model.md`, "The engine's own turn reports"). A member that
reports nothing within the window is retired and one more is spawned;
the second must report.
"""

from __future__ import annotations

import json
import os
import subprocess
import time
import uuid
from dataclasses import dataclass
from pathlib import Path

import pytest

pytestmark = pytest.mark.acceptance

REPORT_WINDOW = int(os.environ.get("HIVE_ACCEPTANCE_HOOK_WINDOW", "150"))


@dataclass
class HookTeam:
    team: str
    pane: str
    root: Path
    workspace: Path


def _tmux(*args: str) -> str:
    return subprocess.run(["tmux", *args], capture_output=True, text=True, timeout=20).stdout


@pytest.fixture(scope="module")
def team():
    """A team of its own, created from a plain shell pane: its hived runs in
    the human's real context, where a real claude member is reached. (The
    shared rig's hived runs in its stub orch's context instead.)"""
    run_id = uuid.uuid4().hex[:6]
    t = HookTeam(team=f"hook-{run_id}", pane="", root=Path(f"/tmp/hhook-{run_id}"),
                 workspace=Path(f"/tmp/hhook-{run_id}/ws"))
    t.root.mkdir(parents=True)
    session = f"hive-hook-{run_id}"
    jobs_before = {p.name for p in Path.home().glob(".claude/jobs/*")}
    _tmux("new-session", "-d", "-s", session, "-x", "220", "-y", "50", "-c", str(t.root))
    t.pane = _tmux("display", "-t", f"{session}:", "-p", "#{pane_id}").strip()
    try:
        _tmux("send-keys", "-t", t.pane, "-l",
              f"hive create {t.team} --workspace {t.workspace} && touch {t.root}/created")
        _tmux("send-keys", "-t", t.pane, "Enter")
        deadline = time.time() + 30
        while not (t.root / "created").exists():
            if time.time() > deadline:
                raise RuntimeError("hive create never finished in the hooks team's pane")
            time.sleep(1)
        yield t
    finally:
        for p in Path.home().glob(".claude/jobs/*"):
            if p.name in jobs_before:
                continue
            try:
                state = json.loads((p / "state.json").read_text())
            except (OSError, ValueError):
                continue
            if str(state.get("name", "")).startswith(f"{t.team}."):
                subprocess.run(["claude", "stop", p.name], capture_output=True, timeout=30)
                subprocess.run(["claude", "rm", p.name], capture_output=True, timeout=30)
        subprocess.run(["hive", "delete", t.team, "--down"], capture_output=True, timeout=60)
        _tmux("kill-session", "-t", session)
        subprocess.run(["rm", "-rf", str(t.root)], timeout=15)


def _hook_rows(notify: Path, member: str) -> list[dict]:
    if not notify.exists():
        return []
    rows = []
    for line in notify.read_text().splitlines():
        try:
            rec = json.loads(line)
        except ValueError:
            continue
        if rec.get("event") == "claude.hook" and rec.get("member") == member:
            rows.append(rec)
    return rows


def _env(team) -> dict[str, str]:
    """The creating shell pane's context: no $TMUX, that pane's identity
    pinned, so every verb is issued from it."""
    env = dict(os.environ)
    env.pop("TMUX", None)
    env["TMUX_PANE"] = team.pane
    for key in list(env):
        if key.startswith("CLAUDE") or key.startswith("ANTHROPIC"):
            env.pop(key, None)
    return env


def _spawn(team, member: str, task: Path) -> dict:
    out = subprocess.run(
        ["hive", "spawn", member, "-t", team.team, "--cli", "claude", "--task", str(task)],
        capture_output=True, text=True, timeout=180, env=_env(team),
    )
    assert out.returncode == 0, out.stdout + out.stderr
    payload = json.loads(out.stdout)
    assert payload.get("dispatched") is True, payload
    return payload


def _kill(team, member: str) -> None:
    subprocess.run(["hive", "kill", member, "-t", team.team], capture_output=True, timeout=60, env=_env(team))


def _wait_for_complete(notify: Path, member: str, window: int) -> list[dict]:
    deadline = time.time() + window
    while time.time() < deadline:
        rows = _hook_rows(notify, member)
        if any(r.get("hook") == "turn.complete" for r in rows):
            return rows
        time.sleep(2)
    return _hook_rows(notify, member)


def test_claude_member_reports_its_turn_to_the_hived(team):
    notify = team.workspace / "run" / "notify.jsonl"
    endpoint = team.workspace / "run" / "hooks-endpoint.json"

    task = team.root / "hooked-task.md"
    task.write_text("Reply with exactly the word `ready` and nothing else. Do not run any command.\n")

    members = []
    rows: list[dict] = []
    for attempt, member in enumerate(("hooked", "hooked-again")):
        members.append(member)
        _spawn(team, member, task)
        # the first spawn brings the team's hived up
        assert endpoint.exists(), "the team's hived publishes its hooks endpoint"
        assert (endpoint.stat().st_mode & 0o777) == 0o600
        rows = _wait_for_complete(notify, member, REPORT_WINDOW)
        if any(r.get("hook") == "turn.complete" for r in rows):
            break
        if attempt == 0:
            # the one tolerated case: a spare booted before the plugin
            # record carried the hooks module (see the module docstring)
            _kill(team, member)
            print(f"{member}: no hook report within {REPORT_WINDOW}s; one more spawn for the spare lag")
    try:
        hooks = [r.get("hook") for r in rows]
        assert "turn.complete" in hooks, f"no turn.complete from {members[-1]}: rows={rows}"
        complete = next(r for r in rows if r.get("hook") == "turn.complete")
        assert complete.get("turnId"), complete
        assert complete.get("reason") == "answer", complete
        assert not any(r.get("event") == "claude.hook_refused" for r in rows)
        # the member's row says the busy verdict is the engine's own report
        team = json.loads(subprocess.run(
            ["hive", "team", "-t", team.team], capture_output=True, text=True, timeout=30, env=_env(team),
        ).stdout)
        row = next((m for m in team.get("members", []) if m.get("name") == members[-1]), {})
        assert row.get("busySource") == "hook", row
        assert row.get("hookEvent") == "turn.complete", row
        assert row.get("busy") is False, row
        # the engine reports its own end when hive retires it
        _kill(team, members[-1])
        deadline = time.time() + 30
        end = None
        while time.time() < deadline and end is None:
            end = next((r for r in _hook_rows(notify, members[-1]) if r.get("hook") == "session.end"), None)
            time.sleep(1)
        assert end is not None, f"no session.end from {members[-1]} within 30s of hive kill"
        assert end.get("reason"), end
    finally:
        for member in members:
            _kill(team, member)
