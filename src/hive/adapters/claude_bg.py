"""Claude background jobs: the engine behind a hive claude member.

A claude member is a ``claude --bg`` job. The engine (a full Claude Code TUI
on a pty owned by claude's own supervisor daemon, argv ``claude bg-spare``)
runs outside tmux; the member's pane only shows it through a ``claude attach
<jobId>`` viewer, so the pane process table says nothing about the member's
life. Identity is the jobId — durable across engine restarts, wakes and
upgrades (the engine pid is not). Which job belongs to which tmux pane is
recorded in a per-pane ``.job`` file under the claude config tree, written by
whoever binds the pane to a job (spawn, managed launch, fork) — the same
shape as codex's pane ``.thread`` records.

Signal surfaces (2.1.240 real-machine verified):

- ``<claude-config>/sessions/<enginePid>.json``: the live engine's registry
  entry — ``kind:"bg"``, ``jobId``, ``status`` (idle|busy|waiting; not a
  documented enum), ``waitingFor`` (only while waiting), ``statusUpdatedAt``,
  ``sessionId``, ``messagingSocketPath``. The attach viewer never registers.
  Delivery and runtime read this entry, keyed by jobId scan.
- ``claude agents --json --all``: the durable job ledger. A sleeping engine
  (supervisor parks jobs idle ~1h) or a stopped one keeps its row but loses
  ``pid``/``status`` — that field absence is the asleep-vs-dead separator
  (``state`` lags reality and is never used for liveness). ~270ms per call,
  so it runs only on resolution misses, never per tick.
- ``claude attach <jobId>`` with no tty (stdin /dev/null) prints "Waking…"
  and exits 0 after reviving a parked/stopped engine — new pid, same
  jobId/sessionId. That is the wake primitive delivery self-heals with.

``jobs/<jobId>/state.json`` is deliberately not read: its fields are
undocumented and the registry entry already carries everything hive needs.

Hidden claude subcommands are only recognized at argv[1], so every
invocation here calls the binary directly with the subcommand first — never
behind a flag. Spawn env is washed of CLAUDE*/ANTHROPIC* vars: an inherited
``CLAUDE_CODE_CHILD_SESSION`` marker makes the engine skip registration
entirely (invisible to ``agents --json`` and undeliverable).
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .claude_sessions import _config_dir, _pid_alive, _registry_dir

_AGENTS_TIMEOUT = 10.0  # observed ~270ms; the cap only bounds a hung CLI
_SPAWN_TIMEOUT = 60.0
_WAKE_TIMEOUT = 20.0  # observed ~2-6s including a fresh supervisor start
_WAKE_ENTRY_TIMEOUT = 5.0  # the wake is synchronous; the entry follows fast
_ENTRY_POLL_INTERVAL = 0.3
# Worst-case extra submission budget when delivery must wake a parked engine
# first: one ledger read, the tty-less attach that revives it, and the short
# entry re-read. The sidecar folds this into its request budgets.
WAKE_SUBMIT_BUDGET = _AGENTS_TIMEOUT + _WAKE_TIMEOUT + _WAKE_ENTRY_TIMEOUT

# Job ids observed are 8 lowercase hex chars (the sessionId prefix); accept a
# small band around that so a format drift upstream does not break resolution.
_JOB_ID_RE = re.compile(r"^[0-9a-f]{6,12}$")
# `claude --bg` announces the job on stdout: `backgrounded · 7fcc705f · <name>`
_SPAWN_OUTPUT_RE = re.compile(r"backgrounded\s*·\s*(\S+)")

# An engine entry whose statusUpdatedAt stopped advancing this long ago is not
# trusted as busy/waiting truth (wedged engine, clock issues); liveness still
# holds — the pid check is what proves the process.
STATUS_STALE_AFTER_SECONDS = 30 * 60


def looks_like_job_id(value: str) -> bool:
    return bool(_JOB_ID_RE.match(value or ""))


# --------------------------------------------------------------------------
# pane <-> job records
# --------------------------------------------------------------------------
def _control_dir() -> Path:
    return _config_dir() / "hive-control"


def pane_job_path(pane: str) -> Path:
    """Per-pane record of the bg job hive bound to this pane."""
    slug = pane.replace("%", "") or "default"
    return _control_dir() / f"hive-pane-{slug}.job"


def write_pane_job(pane: str, job_id: str, session_id: str, cwd: str) -> None:
    path = pane_job_path(pane)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"jobId": job_id, "sessionId": session_id, "cwd": cwd}))


def read_pane_job(pane: str) -> tuple[str, str, str] | None:
    """(job_id, session_id, cwd) recorded for *pane*, or None."""
    try:
        data = json.loads(pane_job_path(pane).read_text())
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict):
        return None
    job_id = data.get("jobId")
    if not job_id:
        return None
    return str(job_id), str(data.get("sessionId") or ""), str(data.get("cwd") or "")


def clear_pane_job(pane: str) -> None:
    pane_job_path(pane).unlink(missing_ok=True)


def job_id_for_pane(pane: str) -> str | None:
    record = read_pane_job(pane)
    return record[0] if record else None


def _pane_from_record_name(name: str) -> str | None:
    """Inverse of :func:`pane_job_path`: ``hive-pane-19.job`` -> ``%19``."""
    if not name.startswith("hive-pane-") or not name.endswith(".job"):
        return None
    slug = name[len("hive-pane-"):-len(".job")]
    if not slug or slug == "default":
        return None
    return "%" + slug


def list_recorded_panes() -> list[str]:
    """Pane ids that currently have a job record on disk."""
    root = _control_dir()
    if not root.is_dir():
        return []
    panes: list[str] = []
    for entry in root.glob("hive-pane-*.job"):
        pane = _pane_from_record_name(entry.name)
        if pane:
            panes.append(pane)
    return panes


def pane_for_job(job_id: str) -> str | None:
    """Pane recorded for *job_id*, or None.

    The reverse lookup behind tool-side identity: a ``hive`` invocation inside
    a member's tool subprocess carries ``CLAUDE_CODE_MESSAGING_SOCKET``
    naming the engine's inbox, the engine's registry entry names the jobId,
    and this maps it back to the tmux pane hive bound the job to.
    """
    if not job_id:
        return None
    for pane in list_recorded_panes():
        record = read_pane_job(pane)
        if record and record[0] == job_id:
            return pane
    return None


# --------------------------------------------------------------------------
# engine registry entries (sessions/<enginePid>.json, kind == "bg")
# --------------------------------------------------------------------------
@dataclass(frozen=True)
class EngineSession:
    pid: int
    job_id: str
    session_id: str
    socket_path: str
    cwd: str
    status: str
    waiting_for: str
    status_updated_at: float  # epoch seconds, 0.0 when absent


def _entry_to_engine(data: dict[str, Any]) -> EngineSession | None:
    if data.get("kind") != "bg":
        return None
    pid = data.get("pid")
    job_id = str(data.get("jobId") or "")
    sock = str(data.get("messagingSocketPath") or "")
    if not job_id or not isinstance(pid, int) or isinstance(pid, bool) or not sock:
        return None
    if not _pid_alive(pid) or not os.path.exists(sock):
        return None
    raw_updated = data.get("statusUpdatedAt")
    updated = float(raw_updated) / 1000.0 if isinstance(raw_updated, (int, float)) else 0.0
    return EngineSession(
        pid=pid,
        job_id=job_id,
        session_id=str(data.get("sessionId") or ""),
        socket_path=sock,
        cwd=str(data.get("cwd") or ""),
        status=str(data.get("status") or ""),
        waiting_for=str(data.get("waitingFor") or ""),
        status_updated_at=updated,
    )


def engine_session_for_job(job_id: str) -> EngineSession | None:
    """The live engine's registry entry for *job_id*, or None.

    The engine registers under its own (unstable) pid, so the jobId is found
    by scanning the registry for the ``kind:"bg"`` entry naming it. None
    means no live engine — asleep or dead; :func:`job_row` tells them apart.
    """
    if not job_id:
        return None
    root = _registry_dir()
    if not root.is_dir():
        return None
    for entry in root.glob("*.json"):
        try:
            data = json.loads(entry.read_text())
        except (OSError, ValueError):
            continue
        if not isinstance(data, dict):
            continue
        engine = _entry_to_engine(data)
        if engine is not None and engine.job_id == job_id:
            return engine
    return None


def engine_session_for_pid(pid: int) -> EngineSession | None:
    """The bg engine entry registered under *pid*, or None (viewer pids and
    interactive sessions have no bg entry)."""
    try:
        data = json.loads((_registry_dir() / f"{pid}.json").read_text())
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict):
        return None
    return _entry_to_engine(data)


def pane_engine_alive(pane: str) -> bool:
    """True when *pane* records a job whose engine is live right now.

    False also covers a parked (asleep) engine — asleep is not dead, but the
    cheap per-tick probes must not pay the ``agents --all`` cost; callers
    that need the third state use :func:`job_row`.
    """
    job_id = job_id_for_pane(pane)
    return bool(job_id) and engine_session_for_job(job_id) is not None


def session_id_for_pane(pane: str) -> str | None:
    """Transcript session id of the pane's recorded job.

    The live engine's registry entry is current truth (an in-session
    ``/clear`` rotates it); the record's spawn-time snapshot answers for a
    parked engine — wake preserves the sessionId, so the snapshot stays
    valid.
    """
    record = read_pane_job(pane)
    if record is None:
        return None
    engine = engine_session_for_job(record[0])
    if engine is not None and engine.session_id:
        return engine.session_id
    return record[1] or None


# --------------------------------------------------------------------------
# job ledger (claude agents --json --all) and lifecycle
# --------------------------------------------------------------------------
def bg_env(extra: dict[str, str] | None = None) -> dict[str, str]:
    """Environment for claude bg invocations.

    CLAUDE*/ANTHROPIC* vars are washed: an inherited
    ``CLAUDE_CODE_CHILD_SESSION`` makes the engine skip registration —
    invisible and undeliverable. The config-tree override survives as
    ``CLAUDE_CONFIG_DIR`` so a sandboxed lane's engine registers in the same
    tree hive reads.
    """
    env = {
        k: v for k, v in os.environ.items()
        if not (k.startswith("CLAUDE") or k.startswith("ANTHROPIC"))
    }
    config = _config_dir()
    if config != Path(os.path.expanduser("~/.claude")):
        env["CLAUDE_CONFIG_DIR"] = str(config)
    if extra:
        env.update(extra)
    return env


def list_jobs(*, claude_bin: str = "claude") -> list[dict[str, Any]] | None:
    """All job rows from ``claude agents --json --all``; None when the CLI
    call itself failed (distinct from an empty ledger)."""
    try:
        result = subprocess.run(
            [claude_bin, "agents", "--json", "--all"],
            capture_output=True,
            text=True,
            timeout=_AGENTS_TIMEOUT,
            env=bg_env(),
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    try:
        rows = json.loads(result.stdout)
    except ValueError:
        return None
    return [row for row in rows if isinstance(row, dict)] if isinstance(rows, list) else None


def job_row(job_id: str, *, claude_bin: str = "claude") -> dict[str, Any] | None:
    """The ledger row for *job_id*, or None (unknown job, or CLI failure).

    A row without ``pid``/``status`` is a parked or stopped engine — asleep,
    not dead: ``claude attach`` wakes it with the same jobId/sessionId.
    """
    if not job_id:
        return None
    rows = list_jobs(claude_bin=claude_bin)
    if rows is None:
        return None
    for row in rows:
        if str(row.get("id") or "") == job_id:
            return row
    return None


def job_exists(job_id: str, *, claude_bin: str = "claude") -> bool:
    return job_row(job_id, claude_bin=claude_bin) is not None


def spawn_job(
    *,
    cwd: str,
    name: str,
    prompt: str = "",
    extra_args: list[str] | None = None,
    extra_env: dict[str, str] | None = None,
    claude_bin: str = "claude",
) -> str | None:
    """Start a ``claude --bg`` job; return its jobId, or None on failure.

    *extra_args* are forwarded verbatim (``--model``, ``-r <sid>
    --fork-session``, ``--settings`` …) and become the job's durable
    ``respawnFlags``, so any path-valued flag must be absolute. The prompt is
    the positional argument (never ``-p``, which ``--bg`` rejects). An empty
    *name* adds no ``--name`` (the caller passed its own in *extra_args*).
    """
    argv = [claude_bin, "--bg"]
    if name:
        argv.extend(["--name", name])
    argv.extend(extra_args or [])
    if prompt:
        argv.append(prompt)
    try:
        result = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=_SPAWN_TIMEOUT,
            cwd=cwd or None,
            env=bg_env(extra_env),
            stdin=subprocess.DEVNULL,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    match = _SPAWN_OUTPUT_RE.search(result.stdout or "")
    return match.group(1) if match else None


def wake_job(job_id: str, *, claude_bin: str = "claude") -> bool:
    """Revive a parked/stopped engine without a terminal.

    ``claude attach <jobId>`` with stdin at /dev/null prints "Waking…", spins
    the engine back up (new pid, same jobId/sessionId) and exits 0. On a
    removed job it fails; the caller reads the registry to see the result.
    """
    if not job_id:
        return False
    try:
        result = subprocess.run(
            [claude_bin, "attach", job_id],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=_WAKE_TIMEOUT,
            env=bg_env(),
        )
    except (OSError, subprocess.SubprocessError):
        return False
    return result.returncode == 0


def wait_engine_entry(job_id: str, *, timeout: float) -> EngineSession | None:
    """Poll for the engine's registry entry (spawn readiness)."""
    deadline = time.monotonic() + timeout
    while True:
        engine = engine_session_for_job(job_id)
        if engine is not None:
            return engine
        if time.monotonic() >= deadline:
            return None
        time.sleep(_ENTRY_POLL_INTERVAL)


def ensure_engine(
    job_id: str,
    *,
    timeout: float = _WAKE_ENTRY_TIMEOUT,
    claude_bin: str = "claude",
) -> EngineSession | None:
    """The job's live engine entry, waking a parked engine when needed.

    Returns None when no engine came up — the job is gone (removed) or the
    wake failed; the caller decides whether that is a delivery error.
    """
    engine = engine_session_for_job(job_id)
    if engine is not None:
        return engine
    if not wake_job(job_id, claude_bin=claude_bin):
        return None
    return wait_engine_entry(job_id, timeout=timeout)


def stop_job(job_id: str, *, claude_bin: str = "claude") -> None:
    """Best-effort ``claude stop`` — parks the job (still in ``--all``, still
    wakeable); never raises."""
    if not job_id:
        return
    try:
        subprocess.run(
            [claude_bin, "stop", job_id],
            capture_output=True,
            timeout=_AGENTS_TIMEOUT,
            env=bg_env(),
            stdin=subprocess.DEVNULL,
        )
    except (OSError, subprocess.SubprocessError):
        pass


# --------------------------------------------------------------------------
# runtime signal mapping (engine status -> hive runtime fields)
# --------------------------------------------------------------------------
def runtime_from_engine(engine: EngineSession, *, now: float | None = None) -> dict[str, Any]:
    """Fold an engine entry's status into hive runtime fields.

    ``status`` is the live truth (``state`` in the ledger lags); ``waiting``
    carries ``waitingFor``. A stale ``statusUpdatedAt`` demotes the status to
    unknown instead of trusting a wedged engine's last word.
    """
    fields: dict[str, Any] = {"_runtimeSource": "claude_bg"}
    current = time.time() if now is None else now
    if (
        engine.status_updated_at
        and current - engine.status_updated_at > STATUS_STALE_AFTER_SECONDS
    ):
        fields.update(busy=False, inputState="unknown", inputReason="stale_status")
        return fields
    if engine.status == "busy":
        fields.update(busy=True, inputState="ready", inputReason="")
    elif engine.status == "waiting":
        fields.update(
            busy=False,
            inputState="waiting_user",
            inputReason=f"registry:{engine.waiting_for or 'unknown'}",
        )
    elif engine.status == "idle":
        fields.update(busy=False, inputState="ready", inputReason="")
    else:
        fields.update(busy=False, inputState="unknown", inputReason="no_registry_status")
    return fields
