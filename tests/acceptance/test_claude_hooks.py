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
from pathlib import Path

import pytest

pytestmark = pytest.mark.acceptance

REPORT_WINDOW = int(os.environ.get("HIVE_ACCEPTANCE_HOOK_WINDOW", "150"))


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


def _spawn(rig, member: str, task: Path) -> dict:
    env = dict(os.environ)
    env.pop("TMUX", None)
    for key in list(env):
        if key.startswith("CLAUDE") or key.startswith("ANTHROPIC"):
            env.pop(key, None)
    out = subprocess.run(
        ["hive", "spawn", member, "-t", rig.team, "--cli", "claude", "--task", str(task)],
        capture_output=True, text=True, timeout=180, env=env,
    )
    assert out.returncode == 0, out.stdout + out.stderr
    payload = json.loads(out.stdout)
    assert payload.get("dispatched") is True, payload
    return payload


def _kill(rig, member: str) -> None:
    subprocess.run(["hive", "kill", member, "-t", rig.team], capture_output=True, timeout=60)


def _wait_for_complete(notify: Path, member: str, window: int) -> list[dict]:
    deadline = time.time() + window
    while time.time() < deadline:
        rows = _hook_rows(notify, member)
        if any(r.get("hook") == "turn.complete" for r in rows):
            return rows
        time.sleep(2)
    return _hook_rows(notify, member)


def test_claude_member_reports_its_turn_to_the_hived(rig):
    notify = rig.workspace / "run" / "notify.jsonl"
    endpoint = rig.workspace / "run" / "hooks-endpoint.json"
    assert endpoint.exists(), "the team's hived publishes its hooks endpoint"
    assert (endpoint.stat().st_mode & 0o777) == 0o600

    task = rig.root / "hooked-task.md"
    task.write_text("Reply with exactly the word `ready` and nothing else. Do not run any command.\n")

    members = []
    rows: list[dict] = []
    for attempt, member in enumerate(("hooked", "hooked-again")):
        members.append(member)
        _spawn(rig, member, task)
        rows = _wait_for_complete(notify, member, REPORT_WINDOW)
        if any(r.get("hook") == "turn.complete" for r in rows):
            break
        if attempt == 0:
            # the one tolerated case: a spare booted before the plugin
            # record carried the hooks module (see the module docstring)
            _kill(rig, member)
            print(f"{member}: no hook report within {REPORT_WINDOW}s; one more spawn for the spare lag")
    try:
        hooks = [r.get("hook") for r in rows]
        assert "turn.complete" in hooks, f"no turn.complete from {members[-1]}: rows={rows}"
        complete = next(r for r in rows if r.get("hook") == "turn.complete")
        assert complete.get("turnId"), complete
        assert complete.get("reason") == "answer", complete
        assert not any(r.get("event") == "claude.hook_refused" for r in rows)
        # the member's runtime row carries the hook as its busy source
        team = json.loads(subprocess.run(
            ["hive", "team", "-t", rig.team], capture_output=True, text=True, timeout=30,
        ).stdout)
        row = next((m for m in team.get("members", []) if m.get("name") == members[-1]), {})
        assert row.get("_busySource") == "hook", row
        assert row.get("busy") is False, row
    finally:
        for member in members:
            _kill(rig, member)
