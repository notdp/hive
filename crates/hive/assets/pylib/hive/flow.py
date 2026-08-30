"""hive.flow — deterministic orchestration over live members.

The script-facing API of the in-process ``hive.flow`` library
(``agent()``, ``parallel()``, ``Member``, ``FlowError``), rebuilt as a
thin client: ``hive flow run`` materializes this file and every hive
interaction — team resolution, spawn with retry and ready gate, dispatch,
bus reply polling, kill — is a hidden ``hive flow-op`` subprocess call
into the binary at ``$HIVE_BIN``. Orchestration state stays in this
process: the spawn lock, member liveness, ``parallel()`` threading.

Op protocol: ``hive flow-op <op> <json-args>``; stdout lines starting
with ``[flow] `` are progress (streamed through verbatim), the final
line is one JSON object — ``{"ok": true, ...}`` on success,
``{"ok": false, "error": ...}`` (exit 1) on failure.
"""

from __future__ import annotations

import json
import os
import subprocess
import threading
from dataclasses import dataclass, field
from pathlib import Path

FLOW_SENDER = "flow.run"

# tmux splits and team registration race each other; spawns serialize,
# waiting and reply-polling stay parallel.
_SPAWN_LOCK = threading.Lock()


class FlowError(RuntimeError):
    """A flow step failed loudly: spawn, ready gate, or dispatch."""


@dataclass
class _Ctx:
    team_name: str
    workspace: str


_ctx: _Ctx | None = None
_ctx_lock = threading.Lock()


def _op(op: str, args: dict[str, object]) -> dict[str, object]:
    """One hidden `hive flow-op` call. `wait-reply` blocks in the child
    (2s bus poll) by design — interrupt the flow run to stop waiting.
    """
    hive_bin = os.environ.get("HIVE_BIN") or "hive"
    proc = subprocess.Popen(
        [hive_bin, "flow-op", op, json.dumps(args)],
        stdout=subprocess.PIPE,
        text=True,
    )
    result_line = ""
    assert proc.stdout is not None
    for line in proc.stdout:
        line = line.rstrip("\n")
        if line.startswith("[flow] "):
            print(line, flush=True)
        elif line:
            result_line = line
    code = proc.wait()
    try:
        result = json.loads(result_line) if result_line else {}
    except json.JSONDecodeError:
        result = {}
    if code != 0 or not result.get("ok"):
        raise FlowError(str(result.get("error") or f"hive flow-op {op} failed (exit {code})"))
    return result


def _context() -> _Ctx:
    global _ctx
    with _ctx_lock:
        if _ctx is None:
            resolved = _op("context", {})
            _ctx = _Ctx(
                team_name=str(resolved.get("teamName") or ""),
                workspace=str(resolved.get("workspace") or ""),
            )
        return _ctx


def _log(message: str) -> None:
    print(f"[flow] {message}", flush=True)


def _task_artifact(name: str, text: str) -> str:
    ctx = _context()
    tasks_dir = Path(ctx.workspace) / "artifacts" / "tasks"
    tasks_dir.mkdir(parents=True, exist_ok=True)
    path = tasks_dir / f"{name}.md"
    counter = 1
    while path.exists():
        counter += 1
        path = tasks_dir / f"{name}-{counter}.md"
    path.write_text(text, encoding="utf-8")
    return str(path)


def _dispatch(name: str, *, body: str, artifact: str) -> str:
    # Bounded retries live in the binary; refusal logs stream through _op.
    payload = _op("dispatch", {"name": name, "body": body, "artifact": artifact})
    return str(payload.get("msgId") or "")


def _await_reply(name: str, msg_id: str) -> dict[str, object]:
    # Scoped to *name*: a row anchored to the dispatch by anyone else — a
    # bystander touching the thread — is not this member's deliverable.
    return _op("wait-reply", {"name": name, "msgId": msg_id})


@dataclass
class Member:
    """A live member the flow dispatched to. Fields hold its latest reply."""

    name: str
    pane: str
    summary: str = ""
    artifact: str = ""
    msg_id: str = ""
    _dead: bool = field(default=False, repr=False)

    def _absorb(self, reply: dict[str, object]) -> None:
        self.summary = str(reply.get("body") or "")
        self.artifact = str(reply.get("artifact") or "")
        self.msg_id = str(reply.get("msgId") or "")

    def ask(self, prompt: str) -> "Member":
        """Send a follow-up (question, rework order) and block for the answer.

        The member keeps its full context — this is what a dead headless
        subagent cannot do.
        """
        if self._dead:
            raise FlowError(f"member '{self.name}' was killed; spawn a new one")
        if "\n" in prompt or len(prompt) > 200:
            artifact = _task_artifact(f"{self.name}-ask", prompt)
            body = "follow-up: see artifact"
        else:
            artifact = ""
            body = prompt
        msg_id = _dispatch(self.name, body=body, artifact=artifact)
        _log(f"{self.name} asked ({msg_id}); waiting…")
        self._absorb(_await_reply(self.name, msg_id))
        _log(f"{self.name} answered ({self.msg_id})")
        return self

    def kill(self) -> None:
        """Retire the member's pane; the window re-tiles."""
        with _SPAWN_LOCK:
            _op("kill", {"name": self.name})
        self._dead = True
        _log(f"{self.name} retired")


def agent(prompt: str, *, name: str, cli: str | None = None, model: str = "") -> Member:
    """Spawn a member, dispatch *prompt* as its task, block for its reply.

    The prompt is the whole contract — write it self-contained (scope,
    deliverable path, acceptance, material paths). It is written to
    ``<workspace>/artifacts/tasks/<name>.md`` and dispatched with the
    same atomic skeleton as ``hive spawn --task``.
    """
    # The binary owns the flow/flow.* name guard, the retry loop, and the
    # retry logs; the lock here is the cross-thread spawn serialization.
    # ponytail: the lock spans the in-op retry sleeps too (the in-process
    # library released it between attempts); spawns only over-serialize.
    with _SPAWN_LOCK:
        spawned = _op("spawn", {"name": name, "cli": cli, "model": model})
    pane = str(spawned.get("pane") or "")
    _log(f"{name} spawned in {pane}")
    _op("ready", {"name": name, "cli": str(spawned.get("cli") or "")})

    artifact = _task_artifact(name, prompt)
    msg_id = _dispatch(name, body=f"flow-mailbox dispatch: {Path(artifact).name} (not a member; hive send flow.run, then stop)", artifact=artifact)
    _log(f"{name} dispatched ({msg_id}); waiting for reply…")
    member = Member(name=name, pane=pane)
    member._absorb(_await_reply(name, msg_id))
    _log(f"{name} replied ({member.msg_id})")
    return member


def parallel(*thunks):
    """Run callables concurrently; return their results in call order.

    The first exception propagates after every thread finishes — no
    silent partial results.
    """
    from concurrent.futures import ThreadPoolExecutor

    if not thunks:
        return []
    with ThreadPoolExecutor(max_workers=len(thunks)) as pool:
        futures = [pool.submit(t) for t in thunks]
        results = []
        first_error: BaseException | None = None
        for future in futures:
            try:
                results.append(future.result())
            except BaseException as exc:  # noqa: BLE001 — re-raised below
                if first_error is None:
                    first_error = exc
                results.append(None)
        if first_error is not None:
            raise first_error
    return results
