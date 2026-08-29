"""hive.flow — deterministic orchestration over live members.

A flow script is plain Python the orch writes per task and runs with
``hive flow run workflow.py``. ``agent()`` spawns a real member pane, waits
until it is ready, dispatches the task as its first ``<HIVE>`` message,
then blocks until the member replies — the visible counterpart of a
headless subagent call. ``parallel()`` runs several of those at once.

The runner never owns a pane: it dispatches as the reserved ``flow``
address, whose delivery is the durable bus row itself (the hived's
mailbox branch), and reads replies straight off the bus. Members answer
with an ordinary ``hive send flow`` — auto-anchoring threads it back to
the dispatch, no new addressing concepts.

Deliberately not here: sandboxing (the script author is the orch),
schema validation, resume journals, token budgets, progress UI — the
panes are the progress display.
"""

from __future__ import annotations

import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

FLOW_SENDER = "flow.run"
_REPLY_POLL_SECONDS = 2.0

# tmux splits and team registration race each other; spawns serialize,
# waiting and reply-polling stay parallel.
_SPAWN_LOCK = threading.Lock()


class FlowError(RuntimeError):
    """A flow step failed loudly: spawn, ready gate, or dispatch."""


@dataclass
class _Ctx:
    team_name: str
    team: object
    workspace: str


_ctx: _Ctx | None = None
_ctx_lock = threading.Lock()


def _context() -> _Ctx:
    global _ctx
    with _ctx_lock:
        if _ctx is None:
            from . import cli as cli_mod

            team_name, team = cli_mod._resolve_scoped_team(None, required=True)
            workspace = cli_mod._resolve_workspace(team, required=True)
            _ctx = _Ctx(team_name=str(team_name), team=team, workspace=str(workspace))
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


_DISPATCH_ATTEMPTS = 3
_DISPATCH_RETRY_GAP = 3.0


def _dispatch(name: str, *, body: str, artifact: str) -> str:
    """Send with bounded retries: a cloud-backed transport (grok leader RPC,
    codex daemon) can refuse transiently under provider throttling, and a
    single blip must not kill a whole orchestration. Still loud on exhaustion.
    """
    from . import cli as cli_mod

    ctx = _context()
    last: RuntimeError | None = None
    for attempt in range(_DISPATCH_ATTEMPTS):
        try:
            payload = cli_mod._request_send_payload(
                workspace=ctx.workspace,
                team=ctx.team,
                sender_agent=FLOW_SENDER,
                target_agent=name,
                body=body,
                artifact=artifact,
                command_name="flow-dispatch",
                warn_on_long_body=False,
            )
            return str(payload.get("msgId") or "")
        except RuntimeError as exc:
            last = exc
            if attempt + 1 < _DISPATCH_ATTEMPTS:
                _log(f"{name} dispatch refused ({exc}); retry {attempt + 2}/{_DISPATCH_ATTEMPTS}")
                time.sleep(_DISPATCH_RETRY_GAP)
    raise FlowError(f"dispatch to '{name}' failed after {_DISPATCH_ATTEMPTS} attempts: {last}") from last


def _await_reply(name: str, msg_id: str) -> dict[str, object]:
    """Block until a reply from *name* anchored to *msg_id* lands on the bus.

    Scoped to *name*: a row anchored to the dispatch by anyone else — a
    bystander touching the thread — is not this member's deliverable.

    No timeout by design: the members are visible panes and the human is
    the supervisor — interrupt the flow run to stop waiting.
    """
    from . import bus

    ctx = _context()
    while True:
        row = bus.find_reply_to(ctx.workspace, msg_id=msg_id, from_agent=name)
        if row is not None:
            return row
        time.sleep(_REPLY_POLL_SECONDS)


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
            body = f"follow-up: see artifact"
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
        from . import layout as layout_mod

        ctx = _context()
        with _SPAWN_LOCK:
            agents = getattr(ctx.team, "agents", {})
            agent = agents.get(self.name)
            if agent is not None:
                agent.kill()
                del agents[self.name]
            window = getattr(ctx.team, "tmux_window", "") or ""
            if window:
                layout_mod.apply_adaptive(window)
        self._dead = True
        _log(f"{self.name} retired")


def agent(prompt: str, *, name: str, cli: str | None = None, model: str = "") -> Member:
    """Spawn a member, dispatch *prompt* as its task, block for its reply.

    The prompt is the whole contract — write it self-contained (scope,
    deliverable path, acceptance, material paths). It is written to
    ``<workspace>/artifacts/tasks/<name>.md`` and dispatched with the
    same atomic skeleton as ``hive spawn --task``.
    """
    from . import cli as cli_mod

    if name == "flow" or name.startswith("flow."):
        raise FlowError(f"'{name}' collides with the flow runner's mailbox address kind ({FLOW_SENDER}); pick another member name")
    ctx = _context()
    last: Exception | None = None
    spawned = None
    for attempt in range(_DISPATCH_ATTEMPTS):
        with _SPAWN_LOCK:
            try:
                spawned = cli_mod._spawn_team_agent(
                    ctx.team,
                    team_name=ctx.team_name,
                    agent_name=name,
                    model=model,
                    prompt="",
                    skill="hive:hive",
                    cli_name=cli,
                )
                break
            except (ValueError, RuntimeError) as exc:
                # A cloud transport (codex mint, grok leader) fails fast under
                # provider throttling; absorb blips here instead of widening
                # its RPC timeout — each retry is visible, the total bounded.
                last = exc
        if attempt + 1 < _DISPATCH_ATTEMPTS:
            _log(f"{name} spawn failed ({last}); retry {attempt + 2}/{_DISPATCH_ATTEMPTS}")
            time.sleep(_DISPATCH_RETRY_GAP)
    if spawned is None:
        raise FlowError(f"spawn '{name}' failed after {_DISPATCH_ATTEMPTS} attempts: {last}") from last
    _log(f"{name} spawned in {spawned.pane_id}")

    cli_mod._ensure_team_hived(ctx.team, Path(ctx.workspace))
    if spawned.cli != "claude":
        # claude inboxes queue; only TUI-injected CLIs need the ready gate.
        not_ready = cli_mod._wait_for_peer_ready(
            ctx.workspace,
            team_name=ctx.team_name,
            agents={name},
        )
        if not_ready:
            raise FlowError(f"member '{name}' did not reach ready within the gate; inspect its pane")

    artifact = _task_artifact(name, prompt)
    msg_id = _dispatch(name, body=f"flow-mailbox dispatch: {Path(artifact).name} (not a member; hive send flow.run, then stop)", artifact=artifact)
    _log(f"{name} dispatched ({msg_id}); waiting for reply…")
    member = Member(name=name, pane=spawned.pane_id)
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
