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

import fcntl
import json
import os
import pty
import re
import struct
import subprocess
import termios
import threading
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
    name: str = ""  # the job's label, as the panel and ledger show it


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
        name=str(data.get("name") or ""),
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


# --------------------------------------------------------------------------
# keyboard: piping keystrokes into the engine over `claude attach <jobId>`
# --------------------------------------------------------------------------
# `claude attach` reads stdin even when it is a pipe, so a jobId addresses the
# engine's keyboard the same way it addresses everything else — no tmux, no
# pane, no viewer. A pane viewer stays attached and unflickered while this
# second client types (real-machine verified, 2.1.240), and the attach itself
# wakes a parked engine, so the keyboard path self-heals the ~1h park for free.
_CLEAR_LINE = "\x15"  # C-u: drop whatever is in the composer (claude keeps it
                      # on its own kill ring — Ctrl+Y pastes it back)
_RESTORE_KILL = "\x19"  # C-y: paste the kill ring back into the composer
_SUBMIT = "\r"
_ESCAPE = "\x1b"  # interrupts the running turn
# Only used when the job is on nobody's screen: claude's own pty host
# starts at this size, so it is the least surprising thing to wear.
_DEFAULT_PTY_COLS = 200
_DEFAULT_PTY_ROWS = 50

_ENGINE_READY_TIMEOUT = 20.0  # our own attach is the wake; the entry follows it
_CLIENT_READY_TIMEOUT = 15.0  # observed ~0.3s to the journal entry
_TYPE_READY_TIMEOUT = 25.0  # total budget for "the client is forwarding stdin"
_TYPE_RETRY_AFTER = 5.0  # re-type (C-u first, so it is idempotent) after this
_SUBMIT_CONFIRM_TIMEOUT = 20.0  # the user turn is written the moment it lands
# A slash command's `<command-name>` record is written when the command
# *finishes* (a /compact can take a minute), so waiting for it would block the
# caller on work it does not need to see. This window only has to be long
# enough for the failure shape — the command submitted as plain text, which
# writes its turn immediately.
_SLASH_CONFIRM_TIMEOUT = 5.0
_INTERRUPT_CONFIRM_TIMEOUT = 12.0
_LOGS_TIMEOUT = 15.0
_KEY_POLL_INTERVAL = 0.4
_CONTROL_KEY_GAP = 0.25  # a control byte must not ride in the text's chunk
_ATTACH_EXIT_TIMEOUT = 10.0

_LOGS_TAIL_CHARS = 4000  # the composer is at the very end of the pty stream
_ECHO_PREFIX_CHARS = 40  # head/tail slice: unique enough, short enough to survive a wrap
_PASTE_PLACEHOLDER = "[Pastedtext#"  # squashed `[Pasted text #N]`
_INTERRUPT_MARKER = "[Request interrupted by user]"

_ANSI_RE = re.compile(
    r"\x1b\[[0-9;:<=>?]*[ -/]*[@-~]"  # CSI
    r"|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)"  # OSC
    r"|\x1b[_PX^][^\x1b]*\x1b\\"  # APC/DCS/SOS/PM (claude emits `cc-daemon-hint`)
    r"|\x1b[()][0-9A-B]|\x1b[=>]"
)
# `claude logs` replays the raw pty stream: the layout lives in cursor moves,
# not in spaces, so whitespace and box drawing are noise for a substring test.
_CHROME_RE = re.compile(r"[\s─-▟]+")


@dataclass(frozen=True)
class KeyResult:
    """Outcome of a keystroke pipe. ``confirmed`` names the evidence:
    ``transcript`` (the engine recorded the turn/command/interrupt),
    ``status`` (the engine left ``busy``) or ``written`` (the bytes went into
    the pipe and nothing contradicted it)."""

    ok: bool
    confirmed: str = ""
    why: str = ""


def _strip_ansi(text: str) -> str:
    return _ANSI_RE.sub("", text)


def _squash(text: str) -> str:
    return _CHROME_RE.sub("", text)


def job_screen(job_id: str, *, claude_bin: str = "claude") -> str:
    """Tail of ``claude logs <jobId>`` with escape sequences stripped.

    The engine's own pty output — the composer's unsubmitted content included
    — read headlessly, with no pane or viewer involved. Empty when the CLI
    call failed.
    """
    if not job_id:
        return ""
    try:
        result = subprocess.run(
            [claude_bin, "logs", job_id],
            capture_output=True,
            text=True,
            errors="replace",
            timeout=_LOGS_TIMEOUT,
            env=bg_env(),
            stdin=subprocess.DEVNULL,
        )
    except (OSError, subprocess.SubprocessError):
        return ""
    if result.returncode != 0:
        return ""
    return _strip_ansi(result.stdout or "")[-_LOGS_TAIL_CHARS:]


def _composer_has_draft(job_id: str) -> bool:
    """Is there real, human-typed text sitting in the engine's composer?

    Read from the member's own tmux pane — the one place the composer is
    rendered by a real terminal emulator — through the draft guard's styled
    capture, whose dim tracking keeps autocomplete ghost text from counting
    as a draft. The ``claude logs`` replay cannot answer this: it is an
    incremental paint stream, and a partial repaint leaves the last ``❯`` on
    a history echo rather than the composer (real-machine observed).

    Only a pane that is certainly-or-likely showing this very job is read;
    anything else — viewer elsewhere, panel list, no pane — returns False,
    which just skips the restore. This gates the kill-ring paste: claude's
    kill ring survives a C-u on an *empty* composer unchanged, so pasting
    without the gate would resurrect whatever the ring happened to hold
    (real-machine verified).
    """
    from .. import draft_guard
    from .claude_view import view_for_pane

    pane = pane_for_job(job_id)
    if not pane:
        return False
    try:
        view = view_for_pane(pane)
        if view.job_id != job_id or view.certainty not in ("certain", "likely"):
            return False
        return draft_guard.suspected_draft(pane, "claude")
    except Exception:
        return False


def _echo_needles(text: str) -> tuple[str, ...]:
    """What "the composer is showing *text*" can look like on the pty screen.

    Three shapes, any of which counts: the head of the text, its tail (a long
    paste scrolls the composer viewport to the cursor, so the head is off
    screen), and the ``[Pasted text #N]`` placeholder the TUI folds a long
    paste into, which carries none of the text at all.
    """
    squashed = _squash(text)
    if not squashed:
        return ()
    return tuple(
        dict.fromkeys(
            (
                squashed[:_ECHO_PREFIX_CHARS],
                squashed[-_ECHO_PREFIX_CHARS:],
                _PASTE_PLACEHOLDER,
            )
        )
    )


def _echo_counts(job_id: str, needles: tuple[str, ...], *, claude_bin: str) -> dict[str, int]:
    """How often each needle is on the job's screen right now.

    Counted, not tested for presence: the screen is the tail of the whole pty
    replay, so a repeat delivery of the same text, a payload quoting what is
    already displayed, or a leftover placeholder from an earlier paste all sit
    there *before* anything is typed. Only a count that goes up was caused by
    this call.
    """
    screen = _squash(job_screen(job_id, claude_bin=claude_bin))
    return {needle: screen.count(needle) for needle in needles}


def _transcript_cursor(engine: EngineSession | None) -> tuple[Path | None, int]:
    """The job's transcript file and its current size — the offset new
    records are read from once the submit lands."""
    if engine is None or not engine.session_id:
        return None, 0
    from .claude import ClaudeAdapter

    path = ClaudeAdapter().find_session_file(engine.session_id, cwd=engine.cwd or None)
    if path is None:
        return None, 0
    try:
        return path, path.stat().st_size
    except OSError:
        return path, 0


def _transcript_since(path: Path | None, offset: int) -> str:
    """Whatever the transcript gained after *offset*."""
    if path is None:
        return ""
    try:
        with path.open(encoding="utf-8", errors="replace") as handle:
            handle.seek(offset)
            return handle.read()
    except OSError:
        return ""


def _user_text(record: dict[str, Any]) -> str | None:
    if record.get("type") != "user":
        return None
    content = (record.get("message") or {}).get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(
            str(block.get("text") or "")
            for block in content
            if isinstance(block, dict) and block.get("type") == "text"
        )
    return None


def _is_slash_command(text: str) -> bool:
    stripped = text.strip()
    return stripped.startswith("/") and "\n" not in stripped


def _submit_verdict(path: Path | None, offset: int, text: str) -> str:
    """What the transcript says about the submit: ``landed``, ``corrupted``
    or ``none`` (nothing yet — keep waiting).

    A slash command lands as a ``<command-name>`` entry: the engine ran the
    command instead of sending its literal text to the model. Anything else
    lands as a user turn whose content equals what was typed *exactly*.
    ``corrupted`` is the case exact matching exists for: a turn that ends
    with the typed text but carries something in front of it is a leftover
    composer draft that got submitted along with the delivery — the one
    thing a substring match would wave through.
    """
    chunk = _transcript_since(path, offset)
    if not chunk:
        return "none"
    turns: list[str] = []
    for line in chunk.splitlines():
        try:
            record = json.loads(line)
        except ValueError:
            continue  # a half-written tail line; the next poll sees it whole
        if isinstance(record, dict):
            turn = _user_text(record)
            if turn is not None:
                turns.append(turn)
    if _is_slash_command(text):
        if f"<command-name>{text.strip().split()[0]}</command-name>" in chunk:
            return "landed"
    elif any(turn == text for turn in turns):
        return "landed"
    if any(
        turn != text and turn.endswith(text) and "<command-name>" not in turn
        for turn in turns
    ):
        return "corrupted"
    return "none"


class _AttachClient:
    """A ``claude attach`` client on a pty, wearing the size already on screen.

    The engine's pty follows whatever client is attached, so a client with no
    tty drags it to a default the moment it connects and back when it leaves
    — measured on a real engine: 180 columns, 120 while a tty-less pipe was
    attached, 180 again after. The human watching that engine sees their
    session reflow twice per hive keystroke. Wearing the viewer's own size
    makes the connection invisible instead.

    Exposes the parts of :class:`subprocess.Popen` the keystroke path uses,
    with ``stdin`` writing into the pty master.
    """

    def __init__(self, proc: subprocess.Popen, master_fd: int) -> None:
        self._proc = proc
        self._fd: int | None = master_fd
        self.stdin = self
        self.pid = proc.pid
        # The attached TUI paints continuously; an undrained master fills its
        # buffer and blocks the engine's writes.
        self._drain = threading.Thread(target=self._drain_master, args=(master_fd,), daemon=True)
        self._drain.start()

    @staticmethod
    def _drain_master(fd: int) -> None:
        while True:
            try:
                if not os.read(fd, 65536):
                    return
            except OSError:
                return

    def write(self, payload: str) -> None:
        if self._fd is None:
            raise BrokenPipeError("attach client closed")
        os.write(self._fd, payload.encode("utf-8"))

    def flush(self) -> None:
        return None

    def close(self) -> None:
        fd, self._fd = self._fd, None
        if fd is not None:
            os.close(fd)

    def poll(self) -> int | None:
        return self._proc.poll()

    def wait(self, timeout: float | None = None) -> int:
        return self._proc.wait(timeout=timeout)

    def kill(self) -> None:
        self._proc.kill()


def _engine_screen_size(job_id: str) -> tuple[int, int]:
    """(cols, rows) the engine is rendering at — its viewer pane's size.

    The pane hive bound the job to is the client that set the current size,
    so matching it means the attach changes nothing. With no pane on record
    (or no tmux answer) the engine is not on anyone's screen and any size is
    harmless; the fallback is the size claude's own pty host starts at.
    """
    pane = pane_for_job(job_id)
    if pane:
        try:
            from .. import tmux

            raw = tmux.display_value(pane, "#{pane_width}\t#{pane_height}") or ""
            cols, _, rows = raw.partition("\t")
            if cols.isdigit() and rows.isdigit() and int(cols) > 0 and int(rows) > 0:
                return int(cols), int(rows)
        except Exception:
            pass
    return _DEFAULT_PTY_COLS, _DEFAULT_PTY_ROWS


def _attach_pipe(job_id: str, *, claude_bin: str) -> _AttachClient | None:
    cols, rows = _engine_screen_size(job_id)
    try:
        master, slave = pty.openpty()
    except OSError:
        return None
    try:
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        proc = subprocess.Popen(
            [claude_bin, "attach", job_id],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=bg_env(),
            start_new_session=True,  # the pty is the client's tty, not ours
        )
    except (OSError, subprocess.SubprocessError):
        os.close(master)
        os.close(slave)
        return None
    os.close(slave)
    return _AttachClient(proc, master)


def _feed(proc: _AttachClient, payload: str) -> bool:
    try:
        assert proc.stdin is not None
        proc.stdin.write(payload)
        proc.stdin.flush()
        return True
    except (BrokenPipeError, OSError, ValueError):
        return False


def _wait_engine_behind(job_id: str, proc: _AttachClient) -> EngineSession | None:
    """The engine the pipe is typing into — the attach itself wakes a parked
    one, so this is also the wake wait. A client that exits first says the
    job is gone; there is nothing left to wait for."""
    deadline = time.monotonic() + _ENGINE_READY_TIMEOUT
    while True:
        engine = engine_session_for_job(job_id)
        if engine is not None:
            return engine
        if proc.poll() is not None or time.monotonic() >= deadline:
            return None
        time.sleep(_ENTRY_POLL_INTERVAL)


def _wait_client_ready(proc: _AttachClient) -> bool:
    """Wait until the attach client has the session on screen.

    Its own attach-journal entry says so (~0.3s), and that matters for the
    control bytes: a ``\\x15`` written into a client that is not in raw key
    mode yet is inserted into the composer as a literal character instead of
    clearing it — observed once on 2.1.240, and silent when it happens.
    """
    from .claude_view import attach_entry_for_pid

    deadline = time.monotonic() + _CLIENT_READY_TIMEOUT
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            return False
        if attach_entry_for_pid(proc.pid) is not None:
            return True
        time.sleep(0.1)
    return False


def _clear_composer(proc: _AttachClient) -> bool:
    """C-u, in a chunk of its own — see :func:`_wait_client_ready`."""
    if not _feed(proc, _CLEAR_LINE):
        return False
    time.sleep(_CONTROL_KEY_GAP)
    return True


def _restore_draft(proc: _AttachClient) -> None:
    """C-y: paste the draft the C-u killed back into the (now empty) composer.

    Best-effort — a failed restore leaves what today's behavior always left,
    the draft on claude's kill ring with the TUI's own Ctrl+Y hint on screen.
    """
    time.sleep(_CONTROL_KEY_GAP)
    if _feed(proc, _RESTORE_KILL):
        time.sleep(_CONTROL_KEY_GAP)  # let the client forward it before EOF


def _close_pipe(proc: _AttachClient) -> None:
    """Let the attach client exit, and make sure it does.

    A wedged client would otherwise outlive the caller holding the pipe open;
    nothing downstream may block on it.
    """
    try:
        if proc.stdin is not None:
            proc.stdin.close()
    except (BrokenPipeError, OSError):
        pass
    try:
        proc.wait(timeout=_ATTACH_EXIT_TIMEOUT)
    except subprocess.TimeoutExpired:
        proc.kill()
        try:
            proc.wait(timeout=_ATTACH_EXIT_TIMEOUT)
        except subprocess.TimeoutExpired:
            pass


def type_into_job(job_id: str, text: str, *, claude_bin: str = "claude") -> KeyResult:
    """Type *text* into the engine's composer and press Enter.

    The composer is cleared first (C-u), so an unsent draft can never be
    concatenated onto the delivered text — and so a re-type after a lost
    keystroke is idempotent rather than doubled. Readiness is not a sleep:
    the text is typed, then ``claude logs`` is polled until the composer
    echoes it back, which is the proof that the attach client is forwarding
    stdin; a slice without an echo re-types.

    A real draft the C-u killed is pasted back (C-y) once the submit is
    confirmed: claude parks the killed text on its kill ring, so the engine
    itself restores the exact bytes. Gated by the dim-aware draft parser
    (autocomplete ghost text never counts, and a C-u that killed nothing
    must not paste whatever the ring held before) and forfeited on a re-type
    (the second C-u overwrites the single-slot ring with our own text).

    ponytail: two pipes typing into the same job at once (a cvim sendback and
    a hand-run ``hive inject``) interleave — one of them wins the composer and
    the other fails loudly on the transcript compare, never silently. Serialize
    with an flock under ``hive-control/<jobId>.lock`` if that ever bites.
    """
    if not job_id or not text:
        return KeyResult(False, why="no job id" if not job_id else "nothing to type")
    proc = _attach_pipe(job_id, claude_bin=claude_bin)
    if proc is None:
        return KeyResult(False, why=f"could not run `{claude_bin} attach {job_id}`")
    try:
        engine = _wait_engine_behind(job_id, proc)
        if engine is None:
            return KeyResult(False, why=f"job {job_id} has no engine (removed?)")
        transcript, offset = _transcript_cursor(engine)
        if not _wait_client_ready(proc):
            return KeyResult(False, why=f"`attach {job_id}` never came up")

        draft = _composer_has_draft(job_id)
        needles = _echo_needles(text)
        baseline = _echo_counts(job_id, needles, claude_bin=claude_bin)
        deadline = time.monotonic() + _TYPE_READY_TIMEOUT
        next_retype = 0.0
        clears = 0
        echoed = False
        while time.monotonic() < deadline:
            if time.monotonic() >= next_retype:
                if not _clear_composer(proc) or not _feed(proc, text):
                    return KeyResult(False, why="the attach client closed its stdin")
                clears += 1
                next_retype = time.monotonic() + _TYPE_RETRY_AFTER
            counts = _echo_counts(job_id, needles, claude_bin=claude_bin)
            if not needles or any(count > baseline[needle] for needle, count in counts.items()):
                echoed = True
                break
            time.sleep(_KEY_POLL_INTERVAL)
        restore = draft and clears == 1
        if not echoed:
            return KeyResult(
                False,
                why=f"job {job_id} never echoed the typed text back into its composer",
            )
        if not _feed(proc, _SUBMIT):
            return KeyResult(False, why="the attach client closed its stdin before Enter")
        if transcript is None:
            if restore:
                _restore_draft(proc)
            return KeyResult(True, "written", "no transcript to confirm against")
        slash = _is_slash_command(text)
        confirm_deadline = time.monotonic() + (
            _SLASH_CONFIRM_TIMEOUT if slash else _SUBMIT_CONFIRM_TIMEOUT
        )
        while time.monotonic() < confirm_deadline:
            verdict = _submit_verdict(transcript, offset, text)
            if verdict == "landed":
                if restore:
                    _restore_draft(proc)
                return KeyResult(True, "transcript")
            if verdict == "corrupted":
                return KeyResult(
                    False,
                    why=f"job {job_id} submitted the text with a leftover draft in front of it",
                )
            time.sleep(_KEY_POLL_INTERVAL)
        if slash:
            # ponytail: a slash command's record comes late (or never — /cost
            # and other UI-only commands write none), so silence here is not
            # evidence of failure; the composer echo already proved the client
            # was forwarding, and a command swallowed as text would have shown
            # up as a turn by now. If a lost `/compact` ever needs catching,
            # the missing signal is "the composer emptied after Enter".
            if restore:
                _restore_draft(proc)
            return KeyResult(True, "written", "a slash command with no transcript record yet")
        return KeyResult(
            False,
            why=f"job {job_id} took the text but no matching turn reached its transcript",
        )
    finally:
        _close_pipe(proc)


def ensure_job_named(job_id: str, name: str, *, claude_bin: str = "claude") -> bool:
    """Make the job's own label read *name*; True when it already did or now does.

    A job minted before hive knew whose pane it was on carries a placeholder
    (`hive-<pane>`): every path that adopts an existing pane into a team —
    duo, squad, resume — tags the pane after its CLI is already running, and
    the mint cannot see a tag that does not exist yet. `/rename` is the only
    way back, the same command the agents panel runs, and it updates the
    panel row, the ledger and the registry at once. A busy engine queues it
    and runs it when the turn ends.

    The name is not cosmetic: the view probe recognizes a session on screen
    by matching the panel title against member names, so a placeholder-named
    member reads as a stranger in its own pane.
    """
    if not job_id or not name:
        return False
    engine = engine_session_for_job(job_id)
    if engine is None:
        return False
    if engine.name == name:
        return True
    return type_into_job(job_id, f"/rename {name}", claude_bin=claude_bin).ok


def interrupt_job(job_id: str, *, claude_bin: str = "claude") -> KeyResult:
    """Send Escape to the engine — interrupt whatever turn is running.

    Escape leaves no composer echo, so the readiness gate the typing path
    uses does not apply, and Escape is never repeated: a second one lands on
    the engine's "edit previous message" chord. It is written once, then
    confirmed against the transcript's interrupt marker or the engine leaving
    ``busy``. An engine that was never busy has nothing to interrupt and
    nothing that could confirm one: that returns right away, a success with
    ``written`` — not a failure, and not a wait.
    """
    if not job_id:
        return KeyResult(False, why="no job id")
    proc = _attach_pipe(job_id, claude_bin=claude_bin)
    if proc is None:
        return KeyResult(False, why=f"could not run `{claude_bin} attach {job_id}`")
    try:
        engine = _wait_engine_behind(job_id, proc)
        if engine is None:
            return KeyResult(False, why=f"job {job_id} has no engine (removed?)")
        transcript, offset = _transcript_cursor(engine)
        was_busy = engine.status == "busy"
        if not _wait_client_ready(proc):
            return KeyResult(False, why=f"`attach {job_id}` never came up")
        if not _feed(proc, _ESCAPE):
            return KeyResult(False, why="the attach client closed its stdin")
        if not was_busy:
            # Nothing was running, so nothing can confirm: waiting out the
            # window could only relabel a success. cvim sends this before
            # every sendback, and the member is idle most of the time.
            time.sleep(_CONTROL_KEY_GAP)  # let the client forward it before EOF
            return KeyResult(True, "written", "the engine was not busy")
        deadline = time.monotonic() + _INTERRUPT_CONFIRM_TIMEOUT
        while time.monotonic() < deadline:
            if _INTERRUPT_MARKER in _transcript_since(transcript, offset):
                return KeyResult(True, "transcript")
            current = engine_session_for_job(job_id)
            if current is not None and current.status != "busy":
                return KeyResult(True, "status")
            time.sleep(_KEY_POLL_INTERVAL)
        return KeyResult(False, why=f"job {job_id} is still busy after Escape")
    finally:
        _close_pipe(proc)


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
