"""Team-scoped sidecar for pending send lifecycle tracking.

The sidecar owns runtime pending-send state in memory and exposes a tiny
workspace-local Unix socket for enqueue/status/shutdown. Durable facts still
land in the workspace database as observation events.
"""

from __future__ import annotations

import hashlib
import json
import os
import signal
import socket
import subprocess
import sys
import time
from collections import defaultdict
from pathlib import Path
from typing import Any

from . import bus
from . import devlog
from . import notify_ui
from .agent_cli import detect_profile_for_pane
from .runtime_state import (
    delivery_exception_body,
    delivery_guidance,
    format_hive_envelope,
    present_delivery_state,
    present_send_state,
    project_thread_event,
    send_guidance,
)
from .runtime_snapshot import RuntimeSnapshot, RuntimeSnapshotStore

ACTIVE_SLEEP = 0.5
IDLE_NOTIFY_TICK_SECONDS = 1.0
IDLE_NOTIFY_THRESHOLD_SECONDS = 5.0
IDLE_NOTIFY_MESSAGE = "Window idle 5s+ (all agents stopped). Return to review."
IDLE_NOTIFY_MISSING_PRUNE_TICKS = 5
NOTIFY_DEBUG_HEARTBEAT_SECONDS = 30.0
SIDECAR_CODE_CHECK_SECONDS = 5.0
SIDECAR_OWNER_CHECK_SECONDS = 5.0
_SIDECAR_REEXEC_LOCK_ENV = "HIVE_SIDECAR_REEXEC_LOCK_FD"
OBSERVATION_TIMEOUT = 60.0
POST_EXCEPTION_FOLLOWUP_TIMEOUT = 10.0
SOCKET_READY_TIMEOUT = 2.0
SOCKET_RETRY_INTERVAL = 0.1
SEND_GRACE_TIMEOUT = 3.0
SEND_REQUEST_TIMEOUT = SEND_GRACE_TIMEOUT + 2.0
SIDECAR_API_VERSION = 5
_FINALIZE_PENDING = "__finalize__"
BUSY_OUTPUT_THRESHOLD_SECONDS = 3.0
# Session snapshots stay event-invalidated for consumers. This low-rate
# maintenance probe only bounds sidecar drift without paying per-tick ps/fd
# cost on the steady fresh path.
RUNTIME_SESSION_STEADY_PROBE_SECONDS = 60.0
_TRANSCRIPT_PATH_CACHE_TTL = 60.0
_OUTPUT_BUSY_MONITOR = None
_TRANSCRIPT_PATH_CACHE: dict[str, tuple[str, float, str]] = {}
_RUNTIME_SESSION_LAST_STEADY_PROBE_AT: dict[str, float] = {}
_AGENT_NOTIFY_ROLES = {"agent", "lead", "orchestrator"}
_RUNTIME_SNAPSHOTS = RuntimeSnapshotStore()


def _compute_build_hash() -> str:
    try:
        root = Path(__file__).resolve().parent
        hasher = hashlib.sha256()
        for path in sorted(root.rglob("*.py")):
            if not path.is_file():
                continue
            rel = path.relative_to(root)
            hasher.update(str(rel).encode())
            hasher.update(path.read_bytes())
        return hasher.hexdigest()
    except OSError:
        return "unknown"


SIDECAR_BUILD_HASH = _compute_build_hash()


def _sidecar_reexec_argv(workspace: str, team: str, tmux_window: str, tmux_window_id: str) -> list[str]:
    return [
        sys.executable,
        "-m",
        "hive.sidecar",
        "--sidecar",
        workspace,
        team,
        tmux_window,
        tmux_window_id,
    ]


def _stale_disk_build_hash_for_reexec(
    state: dict[str, Any],
    *,
    now: float,
    pending_empty: bool,
) -> str | None:
    """Return a stable changed build hash that should trigger sidecar reexec."""
    if not pending_empty:
        return None

    last_check = float(state.get("last_code_check_at", 0.0))
    if now - last_check < SIDECAR_CODE_CHECK_SECONDS:
        return None
    state["last_code_check_at"] = now

    disk_hash = _compute_build_hash()
    if disk_hash == "unknown" or disk_hash == SIDECAR_BUILD_HASH:
        state.pop("candidate_hash", None)
        return None

    if state.get("candidate_hash") == disk_hash:
        return disk_hash
    state["candidate_hash"] = disk_hash
    return None


def _release_reexec_lock_fd(lock_fd: int | None) -> None:
    if lock_fd is None:
        return
    try:
        import fcntl

        fcntl.flock(lock_fd, fcntl.LOCK_UN)
    except OSError:
        pass
    try:
        os.close(lock_fd)
    except OSError:
        pass


def _try_acquire_reexec_lock(workspace: str) -> int | None:
    lock_path = _lock_path(workspace)
    try:
        lock_path.parent.mkdir(parents=True, exist_ok=True)
        lock_fd = os.open(str(lock_path), os.O_CREAT | os.O_RDWR)
    except OSError:
        return None

    try:
        import fcntl

        fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        os.set_inheritable(lock_fd, True)
    except OSError:
        _release_reexec_lock_fd(lock_fd)
        return None
    return lock_fd


def _take_reexec_lock_fd_from_env() -> int | None:
    raw_fd = os.environ.pop(_SIDECAR_REEXEC_LOCK_ENV, "")
    if not raw_fd:
        return None
    try:
        return int(raw_fd)
    except ValueError:
        return None


def _reexec_sidecar(
    *,
    workspace: str,
    team: str,
    tmux_window: str,
    tmux_window_id: str,
    server: socket.socket,
    busy_monitor: Any,
    on_reexec: Any = None,
) -> bool:
    lock_fd = _try_acquire_reexec_lock(workspace)
    if lock_fd is None:
        return False

    previous_lock_env = os.environ.get(_SIDECAR_REEXEC_LOCK_ENV)
    try:
        os.environ[_SIDECAR_REEXEC_LOCK_ENV] = str(lock_fd)
        if busy_monitor is not None:
            busy_monitor.stop()
        _set_output_busy_monitor(None)
        server.close()
        _cleanup_socket(workspace)
        if on_reexec is not None:
            on_reexec()
        argv = _sidecar_reexec_argv(workspace, team, tmux_window, tmux_window_id)
        os.execv(sys.executable, argv)
    finally:
        if previous_lock_env is None:
            os.environ.pop(_SIDECAR_REEXEC_LOCK_ENV, None)
        else:
            os.environ[_SIDECAR_REEXEC_LOCK_ENV] = previous_lock_env
        _release_reexec_lock_fd(lock_fd)
    return True



def _now_iso() -> str:
    from datetime import UTC, datetime

    return datetime.now(UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def _sidecar_metadata(started_at: str) -> dict[str, Any]:
    return {
        "pid": os.getpid(),
        "started_at": started_at,
        "code_hash": SIDECAR_BUILD_HASH,
    }


def _set_output_busy_monitor(monitor: Any) -> None:
    global _OUTPUT_BUSY_MONITOR
    _OUTPUT_BUSY_MONITOR = monitor


def _process_matches_cli(cli_name: str, command: str, argv: str) -> bool:
    from .agent_cli import detect_profile_from_pane_command, detect_profile_from_text

    profile = detect_profile_from_pane_command(command) or detect_profile_from_text(argv)
    return bool(profile and profile.name == cli_name)


def _requires_tick_session_capture(cli_name: str) -> bool:
    return cli_name == "claude"


def _pane_has_recent_output(pane_id: str) -> bool:
    monitor = _OUTPUT_BUSY_MONITOR
    if monitor is None:
        return False
    try:
        return bool(monitor.is_busy(pane_id, threshold_seconds=BUSY_OUTPUT_THRESHOLD_SECONDS))
    except Exception:
        return False


def _runtime_session_steady_probe_due(
    pane_id: str,
    snapshot: RuntimeSnapshot | None,
    *,
    now: float,
) -> bool:
    if snapshot is None or not snapshot.sessionId.is_fresh(now=now):
        return False
    last_checked_at = max(
        float(snapshot.sessionId.observed_at),
        _RUNTIME_SESSION_LAST_STEADY_PROBE_AT.get(pane_id, 0.0),
    )
    return (now - last_checked_at) >= RUNTIME_SESSION_STEADY_PROBE_SECONDS


def _probe_session_id_from_pidfile(pane_id: str, cli_name: str) -> str | None:
    if cli_name != "claude":
        return None
    from . import tmux as tmux_mod
    from .adapters.claude import resolve_session_id_from_pidfile

    tty = tmux_mod.get_pane_tty(pane_id) or ""
    if not tty:
        return None
    cwd_hint = tmux_mod.display_value(pane_id, "#{pane_current_path}")
    for process in tmux_mod.list_tty_processes(tty):
        if not _process_matches_cli(cli_name, process.command, process.argv):
            continue
        session_id = resolve_session_id_from_pidfile(process.pid, cwd=cwd_hint)
        if session_id:
            return session_id
    return None


def _runtime_snapshot_tick(
    team_name: str,
    *,
    store: RuntimeSnapshotStore | None = None,
    now: float | None = None,
    workspace: str = "",
    members: dict[str, dict[str, Any]] | None = None,
) -> None:
    snapshot_store = _RUNTIME_SNAPSHOTS if store is None else store
    observed_at = time.monotonic() if now is None else now
    bindings = members if members is not None else _team_member_bindings(team_name)
    for member in bindings.values():
        if member.get("role") not in _AGENT_NOTIFY_ROLES:
            continue
        pane_id = str(member.get("pane") or "")
        cli_name = str(member.get("cli") or "")
        if not pane_id or not cli_name:
            continue
        if not _requires_tick_session_capture(cli_name):
            continue
        snapshot = snapshot_store.get(pane_id)
        recent_output = _pane_has_recent_output(pane_id)
        needs_probe = cli_name == "claude" and (
            snapshot is None
            or not snapshot.sessionId.is_fresh(now=observed_at)
            or recent_output
        )
        if not needs_probe and not (
            cli_name == "claude"
            and _runtime_session_steady_probe_due(pane_id, snapshot, now=observed_at)
        ):
            continue
        if not needs_probe:
            _RUNTIME_SESSION_LAST_STEADY_PROBE_AT[pane_id] = observed_at
        session_id = _probe_session_id_from_pidfile(pane_id, cli_name)
        if session_id:
            snapshot_store.update_session_id(
                pane_id,
                session_id,
                source="pidfile",
                observed_at=observed_at,
            )
        elif snapshot is not None and recent_output:
            # PTY output is a conservative invalidation signal: a false positive
            # may blank cvim briefly, but reusing an old session would be worse.
            snapshot_store.mark_session_stale(pane_id, observed_at=observed_at)


def _fresh_snapshot_session_id(pane_id: str, *, now: float | None = None) -> str:
    snapshot = _RUNTIME_SNAPSHOTS.get(pane_id)
    if (
        snapshot is not None
        and snapshot.sessionId.value
        and snapshot.sessionId.is_fresh(now=now)
    ):
        return str(snapshot.sessionId.value)
    return ""


def _resolve_transcript_path_cached(pane_id: str, *, force: bool = False) -> str | None:
    """Resolve the agent transcript jsonl path for a pane, with TTL cache.

    Returns the absolute path string, or None when the pane has no
    associated transcript (non-agent pane, no resolved session, etc.).
    The cache is keyed by pane_id with a coarse TTL so the underlying
    rglob in ``adapter.find_session_file`` does not fire on every tick.

    When ``force=True`` the cache is bypassed and re-populated. Callers use
    this to recover from a session switch (e.g. ``/new``) where the cached
    path points at the previous session's jsonl that no longer advances.
    """
    now = time.monotonic()
    snapshot = _RUNTIME_SNAPSHOTS.get(pane_id)
    fresh_snapshot_session_id = _fresh_snapshot_session_id(pane_id, now=now)
    if not force:
        cached = _TRANSCRIPT_PATH_CACHE.get(pane_id)
        if (
            cached is not None
            and now < cached[1]
            and (
                snapshot is None
                or (fresh_snapshot_session_id and cached[2] == fresh_snapshot_session_id)
            )
        ):
            return cached[0] or None

    from . import adapters, tmux as tmux_mod

    path_str = ""
    sid = ""
    if pane_id and tmux_mod.is_pane_alive(pane_id):
        profile = detect_profile_for_pane(pane_id)
        if profile:
            adapter = adapters.get(profile.name)
            if adapter:
                sid = fresh_snapshot_session_id
                if not sid:
                    sid = adapter.resolve_current_session_id(pane_id) or ""
                if sid:
                    cwd_hint = tmux_mod.display_value(pane_id, "#{pane_current_path}")
                    transcript = adapter.find_session_file(sid, cwd=cwd_hint)
                    if transcript:
                        path_str = str(transcript)

    _TRANSCRIPT_PATH_CACHE[pane_id] = (path_str, now + _TRANSCRIPT_PATH_CACHE_TTL, sid)
    return path_str or None


def _check_mtime_within(path: str, threshold_seconds: float) -> bool | None:
    try:
        mtime = os.path.getmtime(path)
    except OSError:
        return None
    return (time.time() - mtime) <= threshold_seconds


def _transcript_progressed_recently(pane_id: str, threshold_seconds: float) -> bool | None:
    """Three-state phantom-redraw gate based on transcript jsonl mtime.

    Returns:
        True  — jsonl mtime advanced within ``threshold_seconds`` (real activity)
        False — jsonl mtime is older than threshold (phantom TUI redraw)
        None  — path could not be determined or stat failed; caller falls back
                to the underlying control-mode signal so notify never silently
                disappears for panes we can't introspect.

    On a stale cache hit the path is re-resolved once to recover from in-pane
    session switches (Claude ``/new``): if the new resolution yields a fresh
    path the gate returns True so real new-session output isn't suppressed.
    """
    path = _resolve_transcript_path_cached(pane_id)
    if not path:
        return None
    progressed = _check_mtime_within(path, threshold_seconds)
    if progressed is not False:
        return progressed
    # Stale: cached path may be from a previous session. Re-resolve once.
    fresh = _resolve_transcript_path_cached(pane_id, force=True)
    if not fresh or fresh == path:
        return False
    return _check_mtime_within(fresh, threshold_seconds)


def _pane_active_turn_phase(pane_id: str) -> str | None:
    """Probe transcript jsonl tail and return the current ``turnPhase`` token.

    Returns None when the transcript path can't be resolved or the probe
    fails — callers should treat that as "phase unknown" and not gate.
    """
    from .activity import probe_transcript_turn_phase

    path = _resolve_transcript_path_cached(pane_id)
    if not path:
        return None
    profile = detect_profile_for_pane(pane_id)
    if not profile:
        return None
    try:
        result = probe_transcript_turn_phase(profile.name, Path(path))
    except Exception:
        return None
    if not isinstance(result, dict):
        return None
    phase = result.get("turnPhase")
    return str(phase) if phase else None


def _pane_in_active_turn(pane_id: str) -> bool:
    """Helper: pane's transcript turnPhase is in :data:`ACTIVE_TURN_PHASES`."""
    from .activity import ACTIVE_TURN_PHASES

    return _pane_active_turn_phase(pane_id) in ACTIVE_TURN_PHASES


def _codex_app_server_busy(pane_id: str) -> bool | None:
    if not pane_id:
        return None
    try:
        from .adapters import codex_app_server

        rt = codex_app_server.runtime_for_pane(pane_id)
    except Exception:
        return None
    if rt is None:
        return None
    return bool(rt.busy)


def _pane_is_truly_busy(pane_id: str, monitor: Any) -> bool:
    """Public ``busy`` signal: true when the agent is in mid-turn.

    For daemon-backed Codex panes the app-server ``busy`` flag is
    authoritative and short-circuits the heuristic sources below.

    Heuristic sources (non-Codex or no daemon):
        A. tmux control-mode reports recent visible output (with phantom-redraw
           gate via transcript jsonl mtime).
        B. transcript turnPhase is in :data:`activity.ACTIVE_TURN_PHASES`.

    Source B catches the streaming-gap case where tmux visible-text payloads
    happen to space out beyond ``BUSY_OUTPUT_THRESHOLD_SECONDS`` mid-tool.
    """
    if not pane_id:
        return False

    app_busy = _codex_app_server_busy(pane_id)
    if app_busy is not None:
        return app_busy

    monitor_busy = (
        monitor is not None
        and monitor.is_busy(pane_id, threshold_seconds=BUSY_OUTPUT_THRESHOLD_SECONDS)
    )
    if monitor_busy:
        progressed = _transcript_progressed_recently(pane_id, BUSY_OUTPUT_THRESHOLD_SECONDS)
        if progressed is not False:
            return True

    return _pane_in_active_turn(pane_id)


def _busy_output_payload(pane_id: str) -> dict[str, Any]:
    return {"busy": _pane_is_truly_busy(pane_id, _OUTPUT_BUSY_MONITOR)}


def _is_output_busy(
    pane_id: str,
    monitor: Any,
    *,
    inactive_age: float | None = None,
) -> bool:
    """idle-notify variant of :func:`_pane_is_truly_busy`.

    For daemon-backed Codex panes the app-server ``busy`` flag is
    authoritative and short-circuits all heuristic sources.

    Heuristic sources (non-Codex or no daemon): same OR signal as
    ``_pane_is_truly_busy`` but the monitor source is additionally
    clamped by an ``inactive_age`` sub-gate: when the window has been
    inactive for ``inactive_age`` seconds, ignore monitor output that
    predates that transition (the user already saw it while the window
    was active — without the clamp, idle-notify rearms ~5s after every
    window switch).

    The active-turn (turnPhase) source intentionally **bypasses**
    ``inactive_age``: an agent mid-tool is busy regardless of when the
    user last looked at the window.
    """
    if not pane_id:
        return False

    app_busy = _codex_app_server_busy(pane_id)
    if app_busy is not None:
        return app_busy

    if monitor is not None and monitor.is_busy(pane_id, threshold_seconds=BUSY_OUTPUT_THRESHOLD_SECONDS):
        progressed = _transcript_progressed_recently(pane_id, BUSY_OUTPUT_THRESHOLD_SECONDS)
        if progressed is not False:
            if inactive_age is None:
                return True
            output_age = monitor.last_output_age(pane_id)
            if output_age is not None and output_age < inactive_age:
                return True

    return _pane_in_active_turn(pane_id)


def _most_recent_output_pane(panes: list[str], monitor: Any) -> str:
    if monitor is None:
        return ""
    candidates: list[tuple[float, str]] = []
    for pane_id in panes:
        try:
            age = monitor.last_output_age(pane_id)
        except AttributeError:
            age = None
        if age is None:
            continue
        candidates.append((float(age), pane_id))
    if not candidates:
        return ""
    return min(candidates)[1]


def _idle_notify_target_pane(panes: list[str], record: dict[str, Any], busy_monitor: Any) -> str:
    recorded = str(record.get("last_busy_pane") or "")
    if recorded in panes:
        return recorded
    recent = _most_recent_output_pane(panes, busy_monitor)
    if recent:
        return recent
    return panes[0]


def _saw_msg_id(pane_id: str, msg_id: str) -> bool:
    monitor = _OUTPUT_BUSY_MONITOR
    if monitor is None or not pane_id or not msg_id:
        return False
    return bool(monitor.saw_msg_id(pane_id, msg_id))


def _run_dir(workspace: str) -> Path:
    return devlog.run_dir(workspace)


def _socket_path(workspace: str) -> Path:
    return _run_dir(workspace) / "sidecar.sock"


def _lock_path(workspace: str) -> Path:
    return _run_dir(workspace) / "sidecar.lock"


def _owner_path(workspace: str) -> Path:
    return _run_dir(workspace) / "sidecar.owner.json"


def _write_sidecar_owner(
    workspace: str,
    *,
    pid: int,
    started_at: str,
    token: str,
) -> None:
    path = _owner_path(workspace)
    tmp = path.with_name(f"{path.name}.{pid}.tmp")
    payload = {
        "pid": pid,
        "startedAt": started_at,
        "token": token,
    }
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp.write_text(json.dumps(payload, ensure_ascii=False))
        os.replace(tmp, path)
    except OSError:
        try:
            tmp.unlink()
        except OSError:
            pass


def _read_sidecar_owner(workspace: str) -> dict[str, Any] | None:
    try:
        payload = json.loads(_owner_path(workspace).read_text())
    except (OSError, json.JSONDecodeError):
        return None
    return payload if isinstance(payload, dict) else None


def _owner_matches_current_process(owner: dict[str, Any] | None, owner_token: str) -> bool:
    if not owner:
        return True
    try:
        owner_pid = int(owner.get("pid", 0))
    except (TypeError, ValueError):
        return True
    return owner_pid == os.getpid() and owner.get("token") == owner_token


def _foreign_owner_pid(workspace: str, owner_token: str) -> int | None:
    owner = _read_sidecar_owner(workspace)
    if _owner_matches_current_process(owner, owner_token):
        return None
    try:
        return int(owner.get("pid", 0)) if owner else 0
    except (TypeError, ValueError):
        return 0


def _cleanup_owner_if_current(workspace: str, owner_token: str) -> None:
    owner = _read_sidecar_owner(workspace)
    if not owner or not _owner_matches_current_process(owner, owner_token):
        return
    try:
        _owner_path(workspace).unlink()
    except OSError:
        pass


def _cleanup_socket_if_owner(workspace: str, owner_token: str) -> None:
    owner = _read_sidecar_owner(workspace)
    if owner and not _owner_matches_current_process(owner, owner_token):
        return
    _cleanup_socket(workspace)
    _cleanup_owner_if_current(workspace, owner_token)


def _write_observation(
    workspace: str,
    message_id: str,
    result: str,
    *,
    metadata: dict[str, str] | None = None,
) -> None:
    ts = _now_iso()
    event_metadata: dict[str, str] = {
        "msgId": message_id,
        "result": result,
        "observedAt": ts,
    }
    if metadata:
        for key, value in metadata.items():
            if value in ("", None):
                continue
            event_metadata[key] = value
    bus.write_event(
        workspace,
        from_agent="_system",
        to_agent="",
        intent="observation",
        message_id=message_id,
        metadata=event_metadata,
    )


def _inject_exception(pane_id: str, message_id: str, target_agent: str, result: str) -> None:
    """Inject a HIVE-SYSTEM exception block into the sender's pane."""
    from .agent import _submit_interactive_text

    body = delivery_exception_body(
        result,
        message_id=message_id,
        target_agent=target_agent,
        timeout_seconds=OBSERVATION_TIMEOUT,
    )
    if body is None:
        return

    block = (
        f"<HIVE-SYSTEM type=delivery-exception msgId={message_id} "
        f"result={result} to={target_agent}>\n{body}\n</HIVE-SYSTEM>"
    )
    try:
        profile = detect_profile_for_pane(pane_id)
        cli_name = profile.name if profile else ""
        _submit_interactive_text(pane_id, block, cli_name)
    except Exception:
        pass


def _effective_deadline(record: dict[str, Any]) -> float:
    deadline = record.get("deadlineAt", 0)
    return float(deadline) if isinstance(deadline, (int, float)) else 0.0


def _pending_terminal_result(record: dict[str, Any]) -> str:
    result = str(record.get("terminalNotifiedResult", "") or "")
    if result == "failed":
        return result
    return ""


def _exception_followup_active(record: dict[str, Any], *, now: float) -> bool:
    followup_until = record.get("terminalFollowupUntil", 0)
    if not isinstance(followup_until, (int, float)):
        return False
    return now <= float(followup_until)


def _pending_delivery_state(record: dict[str, Any], observation: dict[str, Any] | None = None) -> dict[str, Any]:
    inject_status = "submitted"
    turn_observed = "pending"
    observation_result = _pending_terminal_result(record)
    observed_at = ""
    confirmation_source = str(record.get("confirmationSource", ""))

    if observation is not None:
        metadata = observation.get("metadata", {})
        if isinstance(metadata, dict):
            raw_result = metadata.get("result") or observation_result
            observation_result = "success" if raw_result == "success" else ("pending" if raw_result in ("", "pending") else "failed")
            observed_at = str(metadata.get("observedAt") or "")
            inject_status = (
                str(metadata.get("injectStatus", ""))
                or ("failed" if observation_result == "failed" else "submitted")
            )
            turn_observed = str(metadata.get("turnObserved", "")) or turn_observed
            confirmation_source = str(metadata.get("confirmationSource", "")) or confirmation_source

    if not turn_observed:
        if observation_result == "success":
            turn_observed = "confirmed"
        elif observation_result == "failed":
            turn_observed = "unconfirmed" if inject_status == "submitted" else "unavailable"
        else:
            turn_observed = "pending"

    payload: dict[str, Any] = {
        "delivery": present_delivery_state(
            inject_status=inject_status,
            turn_observed=turn_observed,
            observation_result=observation_result,
        ),
        "injectStatus": inject_status,
        "turnObserved": turn_observed,
    }
    if observed_at:
        payload["observedAt"] = observed_at
    if confirmation_source and payload["delivery"] == "success":
        payload["confirmationSource"] = confirmation_source
    guidance = delivery_guidance(payload["delivery"])
    if guidance is not None:
        payload.update(guidance)
    return payload


def _socket_alive(workspace: str) -> bool:
    response = request_ping(workspace)
    return bool(
        response
        and response.get("ok") is True
        and response.get("apiVersion") == SIDECAR_API_VERSION
    )


def request_ping(workspace: str) -> dict[str, Any] | None:
    return _request_sidecar(workspace, {"action": "ping"}, timeout=SOCKET_RETRY_INTERVAL)


def request_connect_codex(workspace: str, pane: str) -> dict[str, Any] | None:
    """Ask the sidecar to bring a per-pane codex 2nd client online now.

    Called at spawn time so the client is connected *before* codex creates its
    thread — it then receives the thread/started + status broadcast
    live, with no late-join resume. Best-effort: returns None when the sidecar
    is down, and the lazy connect on the next runtime tick covers that case.
    """
    return _request_sidecar(workspace, {"action": "connect-codex", "pane": pane}, timeout=3.0)


def _sidecar_identity_matches(
    response: dict[str, Any] | None,
    *,
    team: str,
    tmux_window_id: str,
) -> bool:
    return bool(
        response
        and response.get("ok") is True
        and response.get("apiVersion") == SIDECAR_API_VERSION
        and response.get("buildHash") == SIDECAR_BUILD_HASH
        and response.get("team") == team
        and response.get("tmuxWindowId") == tmux_window_id
    )


def _cleanup_socket(workspace: str) -> None:
    path = _socket_path(workspace)
    try:
        path.unlink()
    except OSError:
        pass


def _request_sidecar(workspace: str, payload: dict[str, Any], *, timeout: float) -> dict[str, Any] | None:
    path = _socket_path(workspace)
    if not path.exists():
        return None
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(timeout)
            client.connect(str(path))
            client.sendall((json.dumps(payload, ensure_ascii=False) + "\n").encode())
            client.shutdown(socket.SHUT_WR)
            chunks: list[bytes] = []
            while True:
                data = client.recv(65536)
                if not data:
                    break
                chunks.append(data)
    except OSError:
        return None
    if not chunks:
        return None
    try:
        response = json.loads(b"".join(chunks).decode())
    except json.JSONDecodeError:
        return None
    return response if isinstance(response, dict) else None


def request_send(
    workspace: str,
    *,
    team: str,
    sender_agent: str,
    sender_pane: str,
    target_agent: str,
    body: str,
    artifact: str = "",
    reply_to: str = "",
    wait: bool = False,
) -> dict[str, Any] | None:
    timeout = OBSERVATION_TIMEOUT if wait else SEND_REQUEST_TIMEOUT
    return _request_sidecar(
        workspace,
        {
            "action": "send",
            "team": team,
            "senderAgent": sender_agent,
            "senderPane": sender_pane,
            "targetAgent": target_agent,
            "body": body,
            "artifact": artifact,
            "replyTo": reply_to,
            "wait": wait,
        },
        timeout=timeout,
    )


def request_delivery(workspace: str, message_id: str) -> dict[str, Any] | None:
    return _request_sidecar(
        workspace,
        {"action": "delivery", "msgId": message_id},
        timeout=SOCKET_RETRY_INTERVAL,
    )


def request_doctor(
    workspace: str,
    *,
    team: str,
    target_agent: str,
    verbose: bool = False,
) -> dict[str, Any] | None:
    return _request_sidecar(
        workspace,
        {"action": "doctor", "team": team, "agent": target_agent, "verbose": verbose},
        timeout=SOCKET_READY_TIMEOUT,
    )


def request_team_runtime(
    workspace: str,
    *,
    team: str,
) -> dict[str, Any] | None:
    return _request_sidecar(
        workspace,
        {"action": "team-runtime", "team": team},
        timeout=SOCKET_READY_TIMEOUT,
    )


def request_runtime_snapshot(
    workspace: str,
    *,
    pane_id: str,
) -> dict[str, Any] | None:
    return _request_sidecar(
        workspace,
        {"action": "runtime-snapshot", "pane": pane_id},
        timeout=SOCKET_READY_TIMEOUT,
    )


def request_thread(workspace: str, message_id: str) -> dict[str, Any] | None:
    return _request_sidecar(
        workspace,
        {"action": "thread", "msgId": message_id},
        timeout=SOCKET_READY_TIMEOUT,
    )


def _resolve_live_agent(team_name: str, agent_name: str):
    from .team import Team

    team = Team.load(team_name)
    agent = team.get(agent_name)
    if not agent.is_alive():
        raise RuntimeError(f"agent '{agent_name}' is not alive")
    return team, agent


def _resolve_ack_baseline(target) -> tuple[Path, int]:
    from . import adapters
    from .adapters.base import get_transcript_baseline

    profile = detect_profile_for_pane(target.pane_id)
    if not profile:
        raise RuntimeError("cannot detect CLI profile for target pane")
    adapter = adapters.get(profile.name)
    if not adapter:
        raise RuntimeError(f"no adapter for CLI '{profile.name}'")
    session_id = _fresh_snapshot_session_id(target.pane_id)
    if not session_id:
        session_id = adapter.resolve_current_session_id(target.pane_id) or ""
    if not session_id:
        raise RuntimeError("cannot resolve session id for target pane")
    from . import tmux
    cwd_hint = tmux.display_value(target.pane_id, "#{pane_current_path}")
    transcript = adapter.find_session_file(session_id, cwd=cwd_hint)
    if not transcript:
        raise RuntimeError(f"transcript file not found for session {session_id}")
    return transcript, get_transcript_baseline(transcript)


def _check_send_gate(transcript_path: Path | None) -> str:
    if transcript_path is None:
        return "skipped"
    from .adapters.base import check_input_gate

    result = check_input_gate(transcript_path)
    if result.status == "waiting":
        raise RuntimeError(
            "target agent is waiting for a user answer; answer it in the target pane"
        )
    return result.status


def _wait_for_delivery_confirmation(
    *,
    pane_id: str,
    transcript_path: Path | None,
    message_id: str,
    baseline: int,
    timeout: float,
) -> str:
    """Block up to *timeout* seconds. Return 'transcript' / 'stream' on confirm, '' on timeout."""
    from .adapters.base import transcript_has_id_in_new_user_turn

    deadline = time.monotonic() + max(0.0, timeout)
    while time.monotonic() < deadline:
        if transcript_path is not None and transcript_has_id_in_new_user_turn(
            transcript_path, message_id, baseline
        ):
            return "transcript"
        if _saw_msg_id(pane_id, message_id):
            return "stream"
        time.sleep(0.2)
    return ""


def _observe_send_grace(
    *,
    pane_id: str,
    transcript_path: Path | None,
    message_id: str,
    baseline: int,
) -> tuple[str, str]:
    """Short synchronous grace loop. Returns (state, confirmation_source).

    state is "confirmed" or "pending". confirmation_source is "transcript",
    "stream", or "" for pending.
    """
    from .adapters.base import transcript_has_id_in_new_user_turn

    deadline = time.monotonic() + SEND_GRACE_TIMEOUT

    while True:
        if transcript_path is not None and transcript_has_id_in_new_user_turn(transcript_path, message_id, baseline):
            return "confirmed", "transcript"

        if _saw_msg_id(pane_id, message_id):
            return "confirmed", "stream"

        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return "pending", ""
        time.sleep(min(0.2, remaining))


def _pending_record(
    *,
    message_id: str,
    sender_agent: str,
    sender_pane: str,
    target_agent: str,
    target_pane: str,
    target_cli: str,
    transcript_path: str,
    baseline: int,
) -> dict[str, Any]:
    return {
        "msgId": message_id,
        "senderAgent": sender_agent,
        "senderPane": sender_pane,
        "targetAgent": target_agent,
        "targetPane": target_pane,
        "targetCli": target_cli,
        "targetTranscript": transcript_path,
        "baseline": baseline,
        "createdAt": _now_iso(),
        "deadlineAt": time.time() + OBSERVATION_TIMEOUT,
    }


def _target_cli_name(target: Any) -> str:
    profile = detect_profile_for_pane(getattr(target, "pane_id", "") or "")
    if profile and profile.name:
        return profile.name
    return str(getattr(target, "cli", "") or "")


def _send_payload(
    *,
    workspace: str,
    team_name: str,
    pending: dict[str, dict[str, Any]],
    sender_agent: str,
    sender_pane: str,
    target_agent: str,
    body: str,
    artifact: str,
    reply_to: str,
    wait: bool,
) -> dict[str, Any]:
    team, target = _resolve_live_agent(team_name, target_agent)
    normalized_body = body.strip()
    target_cli = _target_cli_name(target)

    message_id = ""
    transcript_path: Path | None = None
    baseline = 0
    try:
        transcript_path, baseline = _resolve_ack_baseline(target)
    except Exception:
        transcript_path = None

    # Side effect only: raises if target is waiting for a user answer. Return value unused.
    _check_send_gate(transcript_path)

    event = bus.write_send_event(
        workspace,
        from_agent=sender_agent,
        to_agent=target_agent,
        body=normalized_body,
        artifact=artifact,
        reply_to=reply_to,
    )
    message_id = event.msg_id
    envelope = format_hive_envelope(
        from_agent=sender_agent,
        to_agent=target_agent,
        body=body,
        artifact=artifact,
        message_id=message_id,
        reply_to=reply_to,
    )

    inject_status = "submitted"
    try:
        target.send(envelope)
    except Exception:
        inject_status = "failed"

    turn_observed = "pending"
    confirmation_source = ""
    profile = detect_profile_for_pane(target.pane_id)

    def _confirmed_metadata(source: str) -> dict[str, str]:
        meta = {"injectStatus": "submitted", "turnObserved": "confirmed"}
        if source:
            meta["confirmationSource"] = source
        return meta

    def _add_to_pending() -> None:
        pending[message_id] = _pending_record(
            message_id=message_id,
            sender_agent=sender_agent,
            sender_pane=sender_pane,
            target_agent=target_agent,
            target_pane=target.pane_id,
            target_cli=profile.name if profile else "",
            transcript_path=str(transcript_path) if transcript_path is not None else "",
            baseline=baseline,
        )

    if inject_status == "failed":
        turn_observed = "unavailable"
        _write_observation(
            workspace,
            message_id,
            "failed",
            metadata={
                "injectStatus": "failed",
                "turnObserved": "unavailable",
            },
        )
    elif wait:
        grace_state, grace_source = _observe_send_grace(
            pane_id=target.pane_id,
            transcript_path=transcript_path,
            message_id=message_id,
            baseline=baseline,
        )
        if grace_state == "confirmed":
            turn_observed = "confirmed"
            confirmation_source = grace_source
            _write_observation(workspace, message_id, "success", metadata=_confirmed_metadata(grace_source))
        else:
            wait_source = _wait_for_delivery_confirmation(
                pane_id=target.pane_id,
                transcript_path=transcript_path,
                message_id=message_id,
                baseline=baseline,
                timeout=OBSERVATION_TIMEOUT - SEND_GRACE_TIMEOUT,
            )
            if wait_source:
                turn_observed = "confirmed"
                confirmation_source = wait_source
                _write_observation(workspace, message_id, "success", metadata=_confirmed_metadata(wait_source))
            else:
                turn_observed = "unconfirmed"
                _write_observation(
                    workspace,
                    message_id,
                    "failed",
                    metadata={
                        "injectStatus": "submitted",
                        "turnObserved": "unconfirmed",
                    },
                )
    else:
        grace_state, grace_source = _observe_send_grace(
            pane_id=target.pane_id,
            transcript_path=transcript_path,
            message_id=message_id,
            baseline=baseline,
        )
        if grace_state == "confirmed":
            turn_observed = "confirmed"
            confirmation_source = grace_source
            _write_observation(workspace, message_id, "success", metadata=_confirmed_metadata(grace_source))
        else:
            _add_to_pending()
            turn_observed = "pending"

    payload: dict[str, Any] = {
        "ok": True,
        "to": target_agent,
        "msgId": message_id,
        "delivery": present_send_state(
            inject_status=inject_status,
            turn_observed=turn_observed,
        ),
    }
    if artifact:
        payload["artifact"] = artifact
    if confirmation_source and payload["delivery"] == "success":
        payload["confirmationSource"] = confirmation_source
    guidance = send_guidance(payload["delivery"])
    if guidance is not None:
        payload.update(guidance)
    return payload


def _delivery_payload(workspace: str, pending: dict[str, dict[str, Any]], message_id: str) -> dict[str, Any]:
    send_event = bus.find_send_event(workspace, message_id)
    if send_event is None:
        return {"ok": False, "error": f"no send event found with msgId '{message_id}'"}

    obs = bus.find_latest_observation(workspace, message_id)
    if obs is None and message_id not in pending:
        payload: dict[str, Any] = {
            "ok": True,
            "msgId": message_id,
            "to": send_event.get("to", ""),
            "delivery": "failed",
            "reason": "tracking_lost",
            "injectStatus": "submitted",
            "turnObserved": "unavailable",
        }
        guidance = delivery_guidance("failed")
        if guidance is not None:
            payload.update(guidance)
        return payload

    inject_status = "submitted"
    turn_observed = "pending"

    if message_id in pending:
        record = pending[message_id]
        delivery = _pending_delivery_state(record, obs)
        payload: dict[str, Any] = {
            "ok": True,
            "msgId": message_id,
            "to": send_event.get("to", ""),
        }
        payload.update(delivery)
        return payload

    if obs is not None:
        metadata = obs.get("metadata", {})
        result = metadata.get("result", "") if isinstance(metadata, dict) else ""
        observed_at = metadata.get("observedAt", "") if isinstance(metadata, dict) else ""
        inject_status = (
            str(metadata.get("injectStatus", ""))
            if isinstance(metadata, dict)
            else ""
        ) or ("failed" if result == "failed" else "submitted")
        turn_observed = (
            str(metadata.get("turnObserved", ""))
            if isinstance(metadata, dict)
            else ""
        )
        # Any non-success terminal result — including legacy values like
        # "unconfirmed"/"tracking_lost" left in hive.db — folds to "failed".
        normalized_result = "success" if result == "success" else ("pending" if result in ("", "pending") else "failed")
        if not turn_observed:
            if normalized_result == "success":
                turn_observed = "confirmed"
            elif normalized_result == "failed":
                turn_observed = "unconfirmed" if inject_status == "submitted" else "unavailable"
            else:
                turn_observed = "pending"
        confirmation_source = (
            str(metadata.get("confirmationSource", ""))
            if isinstance(metadata, dict)
            else ""
        )
        payload = {
            "ok": True,
            "msgId": message_id,
            "to": send_event.get("to", ""),
            "delivery": present_delivery_state(
                inject_status=inject_status,
                turn_observed=turn_observed,
                observation_result=normalized_result,
            ),
            "injectStatus": inject_status,
            "turnObserved": turn_observed,
        }
        if confirmation_source and payload["delivery"] == "success":
            payload["confirmationSource"] = confirmation_source
        if observed_at:
            payload["observedAt"] = observed_at
        guidance = delivery_guidance(payload["delivery"])
        if guidance is not None:
            payload.update(guidance)
        return payload

    payload = {
        "ok": True,
        "msgId": message_id,
        "to": send_event.get("to", ""),
        "delivery": present_delivery_state(
            inject_status=inject_status,
            turn_observed=turn_observed,
        ),
        "injectStatus": inject_status,
        "turnObserved": turn_observed,
    }
    guidance = delivery_guidance(payload["delivery"])
    if guidance is not None:
        payload.update(guidance)
    return payload


def _doctor_payload(
    workspace: str,
    team_name: str,
    target_agent: str,
    *,
    verbose: bool = False,
    sidecar: dict[str, Any] | None = None,
) -> dict[str, Any]:
    from .team import Team

    team = Team.load(team_name)
    try:
        target = team.get(target_agent)
    except KeyError as exc:
        raise RuntimeError(str(exc))

    alive = target.is_alive()
    diag: dict[str, object] = {
        "ok": True,
        "agent": target_agent,
        "team": team.name,
    }
    if sidecar:
        diag["sidecar"] = sidecar
    runtime = _member_runtime_payload(target.pane_id, role="agent")
    diag["alive"] = bool(runtime.get("alive", alive))
    if runtime.get("model"):
        diag["model"] = runtime["model"]
    if runtime.get("sessionId"):
        diag["sessionId"] = runtime["sessionId"]
    if runtime.get("inputState"):
        diag["inputState"] = runtime["inputState"]
    if "busy" in runtime:
        diag["busy"] = bool(runtime["busy"])
    if runtime.get("turnPhase"):
        diag["turnPhase"] = runtime["turnPhase"]
    if verbose:
        diag["pane"] = target.pane_id
        diag["teamMembers"] = len(list(team.agents.values()))
        if runtime.get("_cli"):
            diag["cli"] = runtime["_cli"]
        if "inputReason" in runtime:
            diag["inputReason"] = runtime["inputReason"]
        if "_transcript" in runtime:
            diag["transcript"] = runtime["_transcript"]
        if "_transcriptExists" in runtime:
            diag["transcriptExists"] = runtime["_transcriptExists"]
        if "_transcriptSize" in runtime:
            diag["transcriptSize"] = runtime["_transcriptSize"]
        if "_gateReason" in runtime:
            diag["gateReason"] = runtime["_gateReason"]
        if runtime.get("phaseObservedAt"):
            diag["phaseObservedAt"] = runtime["phaseObservedAt"]
        if "_safetyEvidence" in runtime:
            diag["safetyEvidence"] = runtime["_safetyEvidence"]
        diag["workspace"] = str(workspace)
        diag["runDir"] = str(devlog.run_dir(workspace))
        diag["logs"] = devlog.log_paths(workspace)
        diag["eventCount"] = bus.count_events(workspace)
    return diag


def _codex_app_server_runtime(pane_id: str) -> dict[str, Any] | None:
    """Native codex runtime from the per-pane daemon, or None if no daemon.

    hive-spawned codex panes run their own app-server daemon; reading
    busy/turn from it is both accurate and cheap versus tailing the
    transcript. Returns None for embedded (manual) codex so the caller falls
    back to the transcript path.
    """
    from .adapters import codex_app_server

    rt = codex_app_server.runtime_for_pane(pane_id)
    if rt is None:
        return None
    input_state = rt.input_state or "ready"
    fields: dict[str, Any] = {
        "busy": rt.busy,
        "turnPhase": rt.turn_phase,
        "inputState": input_state,
        "inputReason": "" if input_state != "waiting_user" else "app_server_active_flag",
        "_runtimeSource": "codex_app_server",
    }
    return fields


def _codex_session_id_best_effort(
    pane_id: str, *, runtime_snapshot: RuntimeSnapshot | None
) -> str:
    """Codex transcript session id without reading the transcript.

    Prefers a fresh snapshot, then the daemon's app-server thread metadata
    (``thread.sessionId`` from ``thread/resume``), falling back to lsof on the
    daemon pid. Returns 'unresolved' when nothing resolves.
    """
    from .adapters import codex_app_server

    if (
        runtime_snapshot is not None
        and runtime_snapshot.sessionId.value
        and runtime_snapshot.sessionId.is_fresh()
    ):
        return str(runtime_snapshot.sessionId.value)
    sid = codex_app_server.session_id_for_pane(pane_id)
    if sid:
        _RUNTIME_SNAPSHOTS.update_session_id(
            pane_id, sid, source="codex_app_server_session"
        )
        return sid
    return "unresolved"


def _agent_runtime_payload(
    pane_id: str,
    *,
    runtime_snapshot: RuntimeSnapshot | None = None,
) -> dict[str, Any]:
    from . import adapters, tmux
    from .adapters.base import check_input_gate
    from .activity import probe_transcript_turn_phase
    from .agent_cli import resolve_model_for_pane

    runtime: dict[str, Any] = {
        "alive": tmux.is_pane_alive(pane_id),
    }
    runtime.update(_busy_output_payload(pane_id))
    if not runtime["alive"]:
        runtime["inputState"] = "offline"
        runtime["inputReason"] = "pane_dead"
        return runtime

    profile = detect_profile_for_pane(pane_id)
    runtime["_cli"] = profile.name if profile else "unknown"

    resolved_model = resolve_model_for_pane(
        pane_id,
        cli_name=profile.name if profile else "",
        current_model="",
    )
    if resolved_model:
        runtime["model"] = resolved_model

    if not profile:
        runtime["inputState"] = "unknown"
        runtime["inputReason"] = "no_session"
        return runtime

    adapter = adapters.get(profile.name)
    if not adapter:
        runtime["inputState"] = "unknown"
        runtime["inputReason"] = "no_session"
        return runtime

    # hive-spawned codex runs a per-pane app-server daemon: read native runtime
    # signals (busy / turn) over the socket instead of reverse-engineering
    # them from the transcript. manual codex (embedded, no daemon socket)
    # falls through to the transcript path below.
    if profile.name == "codex":
        app_runtime = _codex_app_server_runtime(pane_id)
        if app_runtime is not None:
            runtime.update(app_runtime)
            runtime["sessionId"] = _codex_session_id_best_effort(
                pane_id, runtime_snapshot=runtime_snapshot
            )
            return runtime

    if (
        runtime_snapshot is not None
        and runtime_snapshot.sessionId.value
        and runtime_snapshot.sessionId.is_fresh()
    ):
        runtime.update(runtime_snapshot.to_runtime_fields())
        session_id = str(runtime_snapshot.sessionId.value)
    else:
        session_id = adapter.resolve_current_session_id(pane_id)
        source = "adapter" if session_id else ""
        if not session_id:
            session_id = _probe_session_id_from_pidfile(pane_id, profile.name)
            source = "pidfile"
        runtime["sessionId"] = session_id or "unresolved"
        if session_id:
            snapshot = _RUNTIME_SNAPSHOTS.update_session_id(
                pane_id,
                session_id,
                source=source,
            )
            runtime.update(snapshot.to_runtime_fields())
    if not session_id:
        runtime["inputState"] = "unknown"
        runtime["inputReason"] = "no_session"
        return runtime

    cwd_hint = tmux.display_value(pane_id, "#{pane_current_path}")
    transcript = adapter.find_session_file(session_id, cwd=cwd_hint)
    runtime["_transcript"] = str(transcript) if transcript else None
    if not transcript:
        runtime["inputState"] = "unknown"
        runtime["inputReason"] = "transcript_missing"
        return runtime

    runtime["_transcriptExists"] = transcript.exists()
    if not transcript.exists():
        runtime["inputState"] = "unknown"
        runtime["inputReason"] = "transcript_missing"
        return runtime

    runtime["_transcriptSize"] = transcript.stat().st_size
    safety = probe_transcript_turn_phase(profile.name, transcript)
    runtime["turnPhase"] = str(safety.get("turnPhase") or "unknown_evidence")
    if safety.get("phaseObservedAt"):
        runtime["phaseObservedAt"] = safety["phaseObservedAt"]
    if "evidence" in safety:
        runtime["_safetyEvidence"] = safety["evidence"]
    gate = check_input_gate(transcript)
    runtime["_gate"] = gate.status
    runtime["_gateReason"] = gate.reason
    if gate.status == "waiting":
        runtime["inputState"] = "waiting_user"
        runtime["inputReason"] = "ask_pending"
    elif gate.status == "clear":
        runtime["inputState"] = "ready"
        runtime["inputReason"] = ""
    else:
        runtime["inputState"] = "unknown"
        runtime["inputReason"] = gate.reason or "read_error"
    return runtime


def _member_runtime_payload(pane_id: str, *, role: str) -> dict[str, Any]:
    from . import tmux

    if role != "agent":
        payload = {"alive": tmux.is_pane_alive(pane_id)}
        payload.update(_busy_output_payload(pane_id))
        return payload
    return _agent_runtime_payload(
        pane_id,
        runtime_snapshot=_RUNTIME_SNAPSHOTS.get(pane_id),
    )


def _team_runtime_payload(team_name: str) -> dict[str, Any]:
    from .team import Team
    from .agent_cli import member_role_for_pane

    team = Team.load(team_name)
    members: dict[str, dict[str, Any]] = {}
    needs_answer: list[str] = []

    lead = team.lead_agent()
    if lead is not None:
        role = member_role_for_pane(lead.pane_id)
        runtime = _member_runtime_payload(lead.pane_id, role=role)
        members[lead.name] = runtime
        if runtime.get("inputState") == "waiting_user":
            needs_answer.append(lead.name)

    for name in sorted(team.agents):
        agent = team.agents[name]
        runtime = _member_runtime_payload(agent.pane_id, role="agent")
        members[name] = runtime
        if runtime.get("inputState") == "waiting_user":
            needs_answer.append(name)

    payload: dict[str, Any] = {
        "ok": True,
        "team": team_name,
        "members": members,
    }
    if needs_answer:
        payload["needsAnswer"] = needs_answer
    return payload


def _runtime_snapshot_payload(pane_id: str) -> dict[str, Any]:
    if not pane_id:
        return {"ok": False, "error": "pane required"}
    snapshot = _RUNTIME_SNAPSHOTS.get(pane_id)
    return {
        "ok": True,
        "pane": pane_id,
        "snapshot": snapshot.to_runtime_fields() if snapshot is not None else None,
    }


def _team_member_bindings(team_name: str) -> dict[str, dict[str, Any]]:
    from .team import Team
    from .agent_cli import member_role_for_pane

    team = Team.load(team_name)
    members: dict[str, dict[str, Any]] = {}

    lead = team.lead_agent()
    if lead is not None:
        members[lead.name] = {
            "name": lead.name,
            "role": member_role_for_pane(lead.pane_id),
            "pane": lead.pane_id,
            "cli": lead.cli,
        }

    for name in sorted(team.agents):
        agent = team.agents[name]
        members[name] = {
            "name": name,
            "role": "agent",
            "pane": agent.pane_id,
            "cli": agent.cli,
        }

    return members


def _idle_notify_agent_panes(team_name: str) -> list[str]:
    from . import tmux

    panes: list[str] = []
    for member in _team_member_bindings(team_name).values():
        if member.get("role") not in _AGENT_NOTIFY_ROLES:
            continue
        pane_id = str(member.get("pane") or "")
        if pane_id and pane_id not in panes and tmux.is_pane_alive(pane_id):
            panes.append(pane_id)
    return panes


def _idle_notify_tick(
    *,
    team_name: str,
    session_name: str,
    idle_notify: dict[str, dict[str, Any]],
    busy_monitor: Any,
    now: float,
    workspace: str = "",
    debug_state: dict[str, Any] | None = None,
    members: dict[str, dict[str, Any]] | None = None,
) -> None:
    from . import notify_debug
    from . import plugin_manager
    from . import tmux

    if debug_state is None:
        debug_state = {}
    debug_state["tick_seq"] = int(debug_state.get("tick_seq", 0)) + 1
    per_window = debug_state.setdefault("windows", {})

    active_window = tmux.get_most_recent_client_window(session_name) or ""

    windows: dict[str, list[str]] = {}
    if members is not None:
        agent_panes: list[str] = []
        for member in members.values():
            if member.get("role") not in _AGENT_NOTIFY_ROLES:
                continue
            pane_id = str(member.get("pane") or "")
            if pane_id and pane_id not in agent_panes and tmux.is_pane_alive(pane_id):
                agent_panes.append(pane_id)
    else:
        agent_panes = _idle_notify_agent_panes(team_name)
    for pane_id in agent_panes:
        window_target = tmux.get_pane_window_target(pane_id) or ""
        if not window_target:
            continue
        windows.setdefault(window_target, []).append(pane_id)

    prev_active = debug_state.get("active_window", "__init__")
    inactive_at: dict[str, float] = debug_state.setdefault("inactive_at", {})
    if prev_active != active_window:
        notify_debug.emit(
            workspace,
            "active.changed",
            team=team_name,
            old=prev_active if prev_active != "__init__" else None,
            new=active_window or None,
        )
        # Stamp the moment the previous active window became inactive so the
        # busy check can ignore output that the user already saw while it was
        # active. The newly-active window has no inactive boundary.
        if prev_active and prev_active != "__init__":
            inactive_at[prev_active] = now
        if active_window:
            inactive_at.pop(active_window, None)
        debug_state["active_window"] = active_window

    prev_keys = debug_state.get("windows_keys", "__init__")
    new_keys = sorted(windows)
    if prev_keys != new_keys:
        notify_debug.emit(
            workspace,
            "windows.changed",
            team=team_name,
            old=list(prev_keys) if prev_keys != "__init__" else None,
            new=new_keys,
        )
        debug_state["windows_keys"] = new_keys

    if active_window in windows:
        token = tmux.get_window_option(active_window, notify_ui.NOTIFY_TOKEN_OPTION.lstrip("@"))
        if token:
            notify_debug.emit(
                workspace,
                "active.clear_attempt",
                team=team_name,
                window=active_window,
                token=token,
                panes=sorted(windows[active_window]),
            )
            notify_ui.clear_stale_notify(
                active_window,
                sorted(windows[active_window]),
                token=token,
                remove_attention=False,
                source="sidecar.active_window",
                workspace=workspace,
            )

    if not plugin_manager.is_plugin_enabled("notify"):
        if idle_notify:
            notify_debug.emit(
                workspace,
                "plugin.disabled",
                team=team_name,
                records_cleared=len(idle_notify),
            )
        idle_notify.clear()
        return

    for window_target in list(idle_notify):
        if window_target in windows:
            idle_notify[window_target]["missing_ticks"] = 0
            continue
        record = idle_notify[window_target]
        record["missing_ticks"] = int(record.get("missing_ticks", 0)) + 1
        if record["missing_ticks"] >= IDLE_NOTIFY_MISSING_PRUNE_TICKS:
            notify_debug.emit(
                workspace,
                "record.prune",
                team=team_name,
                window=window_target,
                missing_ticks=record["missing_ticks"],
                last_state={
                    "notified": record.get("notified"),
                    "seen_since_fire": record.get("seen_since_fire"),
                    "last_busy_ts": record.get("last_busy_ts"),
                },
            )
            idle_notify.pop(window_target, None)
            per_window.pop(window_target, None)
            inactive_at.pop(window_target, None)

    for window_target in sorted(windows):
        panes = sorted(windows[window_target])
        record_existed = window_target in idle_notify
        record = idle_notify.setdefault(
            window_target,
            {"last_busy_ts": now, "notified": True, "seen_since_fire": True, "missing_ticks": 0},
        )
        win_dbg = per_window.setdefault(window_target, {"busy_observed": False, "observed_token": None})
        if not record_existed:
            notify_debug.emit(
                workspace,
                "record.create",
                team=team_name,
                window=window_target,
                panes=panes,
                initial={
                    "last_busy_ts": record["last_busy_ts"],
                    "notified": record["notified"],
                    "seen_since_fire": record["seen_since_fire"],
                },
            )
        record["missing_ticks"] = 0

        if window_target == active_window:
            state_before = {
                "notified": record.get("notified"),
                "seen_since_fire": record.get("seen_since_fire"),
                "last_busy_ts": record.get("last_busy_ts"),
            }
            record["last_busy_ts"] = now
            record["notified"] = True
            record["seen_since_fire"] = True
            if (
                state_before["seen_since_fire"] is not True
                or state_before["notified"] is not True
            ):
                notify_debug.emit(
                    workspace,
                    "active.block",
                    team=team_name,
                    window=window_target,
                    state_before=state_before,
                )
            continue

        token = tmux.get_window_option(window_target, notify_ui.NOTIFY_TOKEN_OPTION.lstrip("@"))
        if token:
            prev_token = win_dbg.get("observed_token")
            if prev_token != token:
                notify_debug.emit(
                    workspace,
                    "token.present",
                    team=team_name,
                    window=window_target,
                    token=token,
                    state_before={
                        "notified": record.get("notified"),
                        "seen_since_fire": record.get("seen_since_fire"),
                    },
                )
                win_dbg["observed_token"] = token
            record["notified"] = True
            record["seen_since_fire"] = False
            continue

        if win_dbg.get("observed_token"):
            notify_debug.emit(
                workspace,
                "token.cleared_externally",
                team=team_name,
                window=window_target,
                prev_token=win_dbg["observed_token"],
                state_before={
                    "notified": record.get("notified"),
                    "seen_since_fire": record.get("seen_since_fire"),
                    "last_busy_ts": record.get("last_busy_ts"),
                },
            )
            win_dbg["observed_token"] = None

        inactive_at_ts = inactive_at.get(window_target)
        inactive_age = (now - inactive_at_ts) if inactive_at_ts is not None else None
        busy_panes = [
            p for p in panes
            if _is_output_busy(p, busy_monitor, inactive_age=inactive_age)
        ]
        prev_busy = bool(win_dbg.get("busy_observed", False))
        is_busy = bool(busy_panes)
        if busy_panes:
            record["last_busy_ts"] = now
            record["last_busy_pane"] = _most_recent_output_pane(busy_panes, busy_monitor) or busy_panes[-1]
            if prev_busy != is_busy:
                notify_debug.emit(
                    workspace,
                    "busy.transition",
                    team=team_name,
                    window=window_target,
                    busy=True,
                    busy_panes=busy_panes,
                    last_busy_pane=record.get("last_busy_pane"),
                )
            seen_since_fire = record.get("seen_since_fire", True)
            if seen_since_fire:
                if record.get("notified") is True:
                    notify_debug.emit(
                        workspace,
                        "busy.rearm",
                        team=team_name,
                        window=window_target,
                        seen_since_fire=True,
                    )
                record["notified"] = False
            win_dbg["busy_observed"] = True
            continue

        if prev_busy != is_busy:
            notify_debug.emit(
                workspace,
                "busy.transition",
                team=team_name,
                window=window_target,
                busy=False,
                last_busy_ts=record.get("last_busy_ts"),
            )
        win_dbg["busy_observed"] = False

        last_busy_ts = float(record.get("last_busy_ts", now))
        if now - last_busy_ts >= IDLE_NOTIFY_THRESHOLD_SECONDS and not bool(record.get("notified", False)):
            target_pane = _idle_notify_target_pane(panes, record, busy_monitor)
            phase_snapshot = {p: _pane_active_turn_phase(p) for p in panes}
            notify_debug.emit(
                workspace,
                "fire.attempt",
                team=team_name,
                window=window_target,
                target_pane=target_pane,
                idle_seconds=now - last_busy_ts,
                pane_phases=phase_snapshot,
                state_before={
                    "notified": record.get("notified"),
                    "seen_since_fire": record.get("seen_since_fire"),
                },
            )
            payload = notify_ui.notify(IDLE_NOTIFY_MESSAGE, target_pane, workspace=workspace)
            suppressed = isinstance(payload, dict) and payload.get("suppressed") is True
            record["notified"] = True
            record["seen_since_fire"] = suppressed
            new_token = tmux.get_window_option(window_target, notify_ui.NOTIFY_TOKEN_OPTION.lstrip("@")) or ""
            win_dbg["observed_token"] = new_token or None
            notify_debug.emit(
                workspace,
                "fire.result",
                team=team_name,
                window=window_target,
                target_pane=target_pane,
                surface=(payload.get("surface") if isinstance(payload, dict) else None),
                suppressed=suppressed,
                token_after=new_token or None,
                state_after={
                    "notified": record["notified"],
                    "seen_since_fire": record["seen_since_fire"],
                },
            )

    last_heartbeat = float(debug_state.get("last_heartbeat", 0.0))
    if now - last_heartbeat >= NOTIFY_DEBUG_HEARTBEAT_SECONDS:
        notify_debug.emit(
            workspace,
            "tick.summary",
            team=team_name,
            tick_seq=debug_state["tick_seq"],
            active_window=active_window or None,
            windows=new_keys,
            records=len(idle_notify),
        )
        debug_state["last_heartbeat"] = now


def _thread_payload(workspace: str, pending: dict[str, dict[str, Any]], message_id: str) -> dict[str, Any]:
    events = bus.read_events_with_ns(workspace)
    send_events: dict[str, tuple[int, dict[str, object]]] = {}
    children: dict[str, list[str]] = defaultdict(list)
    latest_observations: dict[str, tuple[int, dict[str, object]]] = {}

    for seq, event in events:
        event_msg_id = str(event.get("msgId") or "")
        intent = str(event.get("intent") or "")
        if not event_msg_id:
            continue
        if intent == "send":
            send_events[event_msg_id] = (seq, event)
            parent = str(event.get("inReplyTo") or "")
            if parent:
                children[parent].append(event_msg_id)
        elif intent == "observation":
            latest_observations[event_msg_id] = (seq, event)

    if message_id not in send_events:
        return {"ok": False, "error": f"no send event found with msgId '{message_id}'"}

    root_id = message_id
    seen: set[str] = set()
    while True:
        _, event = send_events[root_id]
        parent = str(event.get("inReplyTo") or "")
        if not parent or parent not in send_events or parent in seen:
            break
        seen.add(root_id)
        root_id = parent

    depth_map: dict[str, int] = {}
    thread_ids: set[str] = set()

    def _walk(current_id: str, depth: int) -> None:
        if current_id in thread_ids:
            return
        thread_ids.add(current_id)
        depth_map[current_id] = depth
        for child_id in sorted(children.get(current_id, []), key=lambda item: send_events[item][0]):
            _walk(child_id, depth + 1)

    _walk(root_id, 0)

    items: list[dict[str, Any]] = []
    for thread_msg_id in sorted(thread_ids, key=lambda item: send_events[item][0]):
        _, event = send_events[thread_msg_id]
        item = project_thread_event(event)
        item["depth"] = depth_map.get(thread_msg_id, 0)
        if thread_msg_id == message_id:
            item["focus"] = True

        if thread_msg_id in pending:
            record = pending[thread_msg_id]
            observation = latest_observations.get(thread_msg_id, (None, None))[1]
            item["delivery"] = _pending_delivery_state(record, observation)
        elif thread_msg_id in latest_observations:
            _, observation = latest_observations[thread_msg_id]
            metadata = observation.get("metadata", {})
            if isinstance(metadata, dict):
                result = str(metadata.get("result") or "pending")
                delivery_value = result if result in ("pending", "success", "failed") else "pending"
                info: dict[str, Any] = {"delivery": delivery_value}
                if metadata.get("observedAt"):
                    info["observedAt"] = metadata["observedAt"]
                guidance = delivery_guidance(delivery_value)
                if guidance is not None:
                    info.update(guidance)
                item["delivery"] = info

        items.append(item)

    return {
        "ok": True,
        "rootMsgId": root_id,
        "focusMsgId": message_id,
        "messages": items,
    }


def _is_tmux_window_alive(tmux_window_id: str) -> bool:
    import subprocess

    if not tmux_window_id:
        return False
    try:
        result = subprocess.run(
            ["tmux", "display-message", "-t", tmux_window_id, "-p", "#{window_id}"],
            capture_output=True,
            text=True,
            check=False,
            timeout=5,
        )
        return result.returncode == 0 and result.stdout.strip() == tmux_window_id
    except Exception:
        return False


def ensure_sidecar(workspace: str, team: str, tmux_window: str, tmux_window_id: str) -> int | None:
    """Ensure the team sidecar socket is alive."""
    lock_path = _lock_path(workspace)
    lock_path.parent.mkdir(parents=True, exist_ok=True)

    import fcntl

    lock_fd = os.open(str(lock_path), os.O_CREAT | os.O_RDWR)
    try:
        fcntl.flock(lock_fd, fcntl.LOCK_EX)
        response = request_ping(workspace)
        if _sidecar_identity_matches(response, team=team, tmux_window_id=tmux_window_id):
            return None
        if response:
            stop_sidecar(workspace)
        _cleanup_socket(workspace)
        pid = _start_sidecar(workspace, team, tmux_window, tmux_window_id)
        deadline = time.monotonic() + SOCKET_READY_TIMEOUT
        while time.monotonic() < deadline:
            response = request_ping(workspace)
            if _sidecar_identity_matches(response, team=team, tmux_window_id=tmux_window_id):
                return pid
            time.sleep(SOCKET_RETRY_INTERVAL)
        return pid
    finally:
        fcntl.flock(lock_fd, fcntl.LOCK_UN)
        os.close(lock_fd)


def _start_sidecar(workspace: str, team: str, tmux_window: str, tmux_window_id: str) -> int:
    command = [
        sys.executable,
        "-m",
        "hive.sidecar",
        "--sidecar",
        workspace,
        team,
        tmux_window,
        tmux_window_id,
    ]
    stderr_path = devlog.sidecar_stderr_path(workspace)
    stderr_path.parent.mkdir(parents=True, exist_ok=True)
    with (
        open(os.devnull, "rb") as stdin_devnull,
        open(os.devnull, "ab") as stdout_devnull,
        open(stderr_path, "ab") as stderr_log,
    ):
        process = subprocess.Popen(
            command,
            stdin=stdin_devnull,
            stdout=stdout_devnull,
            stderr=stderr_log,
            start_new_session=True,
            close_fds=True,
        )
    return int(process.pid)


def _run_spawned_sidecar(argv: list[str]) -> int:
    if len(argv) != 5 or argv[0] != "--sidecar":
        raise SystemExit("usage: python -m hive.sidecar --sidecar <workspace> <team> <tmux_window> <tmux_window_id>")
    _, workspace, team, tmux_window, tmux_window_id = argv
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    _sidecar_loop(workspace, team, tmux_window, tmux_window_id)
    return 0


def _open_server_socket(workspace: str) -> socket.socket:
    _run_dir(workspace).mkdir(parents=True, exist_ok=True)
    path = _socket_path(workspace)
    _cleanup_socket(workspace)
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(path))
    server.listen()
    return server


def _live_state(record: dict[str, Any]) -> str:
    terminal_result = _pending_terminal_result(record)
    if terminal_result:
        return terminal_result
    return "pending"


def _handle_request(
    *,
    workspace: str,
    team: str,
    tmux_window: str,
    tmux_window_id: str,
    sidecar_started_at: str,
    pending: dict[str, dict[str, Any]],
    request: dict[str, Any],
) -> tuple[dict[str, Any], bool]:
    sidecar = _sidecar_metadata(sidecar_started_at)
    action = request.get("action")
    if action == "ping":
        return {
            "ok": True,
            "apiVersion": SIDECAR_API_VERSION,
            "buildHash": SIDECAR_BUILD_HASH,
            "team": team,
            "tmuxWindow": tmux_window,
            "tmuxWindowId": tmux_window_id,
            "sidecar": sidecar,
        }, True
    if action == "send":
        try:
            response = _send_payload(
                workspace=workspace,
                team_name=str(request.get("team") or team),
                pending=pending,
                sender_agent=str(request.get("senderAgent", "")),
                sender_pane=str(request.get("senderPane", "")),
                target_agent=str(request.get("targetAgent", "")),
                body=str(request.get("body", "")),
                artifact=str(request.get("artifact", "")),
                reply_to=str(request.get("replyTo", "")),
                wait=bool(request.get("wait", False)),
            )
        except Exception as exc:
            response = {"ok": False, "error": str(exc)}
        return response, True
    if action == "enqueue":
        record = request.get("record")
        if not isinstance(record, dict) or not record.get("msgId"):
            return {"ok": False, "error": "invalid record"}, True
        pending[str(record["msgId"])] = record
        return {"ok": True, "delivery": _live_state(record)}, True
    if action == "delivery":
        return _delivery_payload(workspace, pending, str(request.get("msgId", ""))), True
    if action == "doctor":
        try:
            response = _doctor_payload(
                workspace,
                str(request.get("team") or team),
                str(request.get("agent", "")),
                verbose=bool(request.get("verbose", False)),
                sidecar=sidecar,
            )
        except Exception as exc:
            response = {"ok": False, "error": str(exc)}
        return response, True
    if action == "team-runtime":
        try:
            response = _team_runtime_payload(str(request.get("team") or team))
        except Exception as exc:
            response = {"ok": False, "error": str(exc)}
        return response, True
    if action == "runtime-snapshot":
        try:
            response = _runtime_snapshot_payload(str(request.get("pane") or ""))
        except Exception as exc:
            response = {"ok": False, "error": str(exc)}
        return response, True
    if action == "thread":
        try:
            response = _thread_payload(workspace, pending, str(request.get("msgId", "")))
        except Exception as exc:
            response = {"ok": False, "error": str(exc)}
        return response, True
    if action == "connect-codex":
        try:
            from .adapters import codex_app_server
            pane = str(request.get("pane") or "")
            connected = bool(pane) and codex_app_server.connect_pane(pane)
            response = {"ok": True, "connected": connected}
        except Exception as exc:
            response = {"ok": False, "error": str(exc)}
        return response, True
    if action == "status":
        message_id = request.get("msgId", "")
        if message_id in pending:
            record = pending[message_id]
            return {
                "ok": True,
                "tracked": True,
                "delivery": _live_state(record),
            }, True
        obs = bus.find_latest_observation(workspace, str(message_id))
        if obs is not None:
            metadata = obs.get("metadata", {})
            result = metadata.get("result", "") if isinstance(metadata, dict) else ""
            return {"ok": True, "tracked": False, "result": result}, True
        return {"ok": True, "tracked": False}, True
    if action == "shutdown":
        return {"ok": True}, False
    return {"ok": False, "error": "unknown action"}, True


def _serve_requests(
    *,
    server: socket.socket,
    workspace: str,
    team: str,
    tmux_window: str,
    tmux_window_id: str,
    sidecar_started_at: str,
    pending: dict[str, dict[str, Any]],
    timeout: float,
) -> bool:
    end = time.monotonic() + timeout
    while True:
        remaining = end - time.monotonic()
        if remaining <= 0:
            return True
        server.settimeout(remaining)
        try:
            conn, _ = server.accept()
        except socket.timeout:
            return True
        except OSError:
            return True

        keep_running = True
        with conn:
            conn.settimeout(timeout)
            raw = b""
            try:
                while True:
                    chunk = conn.recv(65536)
                    if not chunk:
                        break
                    raw += chunk
            except OSError:
                raw = b""

            try:
                request = json.loads(raw.decode()) if raw else {}
            except json.JSONDecodeError:
                request = {}
            response, keep_running = _handle_request(
                workspace=workspace,
                team=team,
                tmux_window=tmux_window,
                tmux_window_id=tmux_window_id,
                sidecar_started_at=sidecar_started_at,
                pending=pending,
                request=request if isinstance(request, dict) else {},
            )
            try:
                conn.sendall((json.dumps(response, ensure_ascii=False) + "\n").encode())
            except OSError:
                pass
        if not keep_running:
            return False


def _cleanup_dead_codex_daemons(workspace: str) -> None:
    """Reap per-pane codex app-server daemons whose pane has died.

    The daemon is started at spawn time (agent.py) with ``start_new_session`` so
    it outlives the short-lived CLI process; the long-lived sidecar owns
    cleanup. Scan the on-disk sockets and, for any whose pane is gone, kill the
    daemon and remove its socket + pidfile. Independent of team bindings, so it
    also reaps orphans from crashed spawns.

    Killing a daemon takes its ``--remote`` TUI (and pane) down with it, so
    every reap is logged; ``is_pane_alive`` only reports dead panes from a
    successful tmux listing, never from a transient tmux failure.
    """
    from . import notify_debug, tmux
    from .adapters import codex_app_server

    for pane in codex_app_server.list_daemon_panes():
        if not tmux.is_pane_alive(pane):
            try:
                pid: int | None = int(
                    codex_app_server.pane_pidfile_path(pane).read_text().strip()
                )
            except (OSError, ValueError):
                pid = None
            notify_debug.emit(workspace, "daemon.reap", pane=pane, pid=pid)
            codex_app_server.kill_pane_daemon(pane)


def _write_resume_snapshot(workspace: str, team: str) -> None:
    """Persist the team's durable resume snapshot (`hive ls` / `hive resume`).

    Roster-merged: members observed live update their fields; a member whose
    pane died stays in the snapshot with its last known sessionId — that
    entry is exactly what `hive resume` brings back. The store only touches
    the file when the effective payload changed.
    """
    from . import resume as resume_store
    from . import tmux
    from .agent_cli import resolve_model_for_pane
    from .team import Team

    try:
        t = Team.load(team)
    except (FileNotFoundError, KeyError, ValueError):
        return
    if not t.name or not t.tmux_window or not t.agents:
        return
    if not any(t.member_groups.get(name) == "duo" for name in t.agents):
        return  # only duo snapshots are resumable today

    observed: list[dict[str, str]] = []
    for name, agent in sorted(t.agents.items()):
        session_id = _fresh_snapshot_session_id(agent.pane_id) or (agent.session_id or "")
        model = resolve_model_for_pane(agent.pane_id, cli_name=agent.cli, current_model="")
        observed.append({
            "name": name,
            "cli": agent.cli,
            "model": model or agent.model,
            "sessionId": session_id,
            "cwd": agent.cwd,
        })

    existing = resume_store.load_snapshot(t.name)
    prior: list[dict[str, str]] = []
    if existing is not None and str(existing.get("createdAt")) == str(t.created_at):
        # Same team instance: keep last-known fields for members whose pane
        # died. A *different* instance must never inherit the old sessions —
        # start from what we observe and let the store archive the predecessor.
        prior = list(existing.get("members", []))
    members = resume_store.merge_members(prior, observed)
    repo_cwd = next(
        (m.get("cwd", "") for m in members if m.get("name") == "worker" and m.get("cwd")),
        members[0].get("cwd", "") if members else "",
    )
    window_name = dict(tmux.list_window_names()).get(t.tmux_window, "") or str(
        (existing or {}).get("windowName", "")
    )
    pr = tmux.get_window_option(t.tmux_window, "hive-pr") or str((existing or {}).get("pr", ""))
    snap = resume_store.build_snapshot(
        handle=t.name,
        team=t.name,
        group="duo",
        window_name=window_name,
        workspace=workspace,
        repo_cwd=repo_cwd,
        repo=resume_store.repo_label(repo_cwd),
        branch=resume_store.git_branch(repo_cwd),
        pr=pr,
        created_at=str(t.created_at),
        members=members,
    )
    resume_store.save_snapshot(snap, now=_now_iso())


def _sidecar_loop(workspace: str, team: str, tmux_window: str, tmux_window_id: str) -> None:
    from . import tmux

    from . import notify_debug

    sidecar_started_at = _now_iso()
    pending: dict[str, dict[str, Any]] = {}
    idle_notify: dict[str, dict[str, Any]] = {}
    notify_debug_state: dict[str, Any] = {}
    code_reexec_state: dict[str, Any] = {}
    last_window_check = 0.0
    last_owner_check = 0.0
    last_daemon_cleanup = 0.0
    owner_token = f"{os.getpid()}:{time.monotonic_ns()}"
    notify_debug.emit(
        workspace,
        "sidecar.start",
        team=team,
        tmux_window=tmux_window,
        tmux_window_id=tmux_window_id,
        startedAt=sidecar_started_at,
    )
    inherited_reexec_lock_fd = _take_reexec_lock_fd_from_env()
    server = _open_server_socket(workspace)
    _write_sidecar_owner(
        workspace,
        pid=os.getpid(),
        started_at=sidecar_started_at,
        token=owner_token,
    )
    _release_reexec_lock_fd(inherited_reexec_lock_fd)
    inherited_reexec_lock_fd = None
    session_target = (tmux_window.split(":", 1)[0] if ":" in tmux_window else tmux_window).strip()
    busy_monitor = tmux.ControlModeOutputMonitor(session_target) if session_target else None
    _set_output_busy_monitor(busy_monitor)
    if busy_monitor is not None:
        busy_monitor.start()

    try:
        while True:
            if not Path(workspace).is_dir():
                return

            now = time.monotonic()
            if now - last_window_check >= 30.0:
                last_window_check = now
                if not _is_tmux_window_alive(tmux_window_id):
                    return

            if now - last_daemon_cleanup >= 30.0:
                last_daemon_cleanup = now
                _cleanup_dead_codex_daemons(workspace)
                try:
                    _write_resume_snapshot(workspace, team)
                except Exception:
                    # Snapshot persistence must never take the sidecar down.
                    pass

            if now - last_owner_check >= SIDECAR_OWNER_CHECK_SECONDS:
                last_owner_check = now
                foreign_pid = _foreign_owner_pid(workspace, owner_token)
                if foreign_pid is not None:
                    notify_debug.emit(
                        workspace,
                        "sidecar.retire_orphan",
                        team=team,
                        tmux_window=tmux_window,
                        tmux_window_id=tmux_window_id,
                        currentPid=os.getpid(),
                        socketPid=foreign_pid,
                    )
                    return

            stale_hash = _stale_disk_build_hash_for_reexec(
                code_reexec_state,
                now=now,
                pending_empty=not pending,
            )
            if stale_hash:
                def _emit_reexec() -> None:
                    notify_debug.emit(
                        workspace,
                        "sidecar.reexec",
                        team=team,
                        tmux_window=tmux_window,
                        tmux_window_id=tmux_window_id,
                        oldHash=SIDECAR_BUILD_HASH,
                        newHash=stale_hash,
                    )

                _reexec_sidecar(
                    workspace=workspace,
                    team=team,
                    tmux_window=tmux_window,
                    tmux_window_id=tmux_window_id,
                    server=server,
                    busy_monitor=busy_monitor,
                    on_reexec=_emit_reexec,
                )

            tick_members = _team_member_bindings(team)

            _runtime_snapshot_tick(team, now=now, workspace=workspace, members=tick_members)

            if not _serve_requests(
                server=server,
                workspace=workspace,
                team=team,
                tmux_window=tmux_window,
                tmux_window_id=tmux_window_id,
                sidecar_started_at=sidecar_started_at,
                pending=pending,
                timeout=ACTIVE_SLEEP if pending else IDLE_NOTIFY_TICK_SECONDS,
            ):
                return

            _idle_notify_tick(
                team_name=team,
                session_name=session_target,
                idle_notify=idle_notify,
                busy_monitor=busy_monitor,
                now=time.monotonic(),
                workspace=workspace,
                debug_state=notify_debug_state,
                members=tick_members,
            )

            for message_id, record in list(pending.items()):
                result = _check_pending(record)
                if result is None:
                    continue
                if result == _FINALIZE_PENDING:
                    pending.pop(message_id, None)
                    continue
                _write_observation(
                    workspace,
                    message_id,
                    result,
                    metadata=_observation_metadata_for_pending(record, result),
                )
                if result == "failed":
                    sender_pane = record.get("senderPane", "")
                    target_agent = record.get("targetAgent", "")
                    record["terminalNotifiedResult"] = result
                    record["terminalFollowupUntil"] = time.time() + POST_EXCEPTION_FOLLOWUP_TIMEOUT
                    if sender_pane:
                        _inject_exception(sender_pane, message_id, target_agent, result)
                    continue
                pending.pop(message_id, None)
    finally:
        _release_reexec_lock_fd(inherited_reexec_lock_fd)
        if busy_monitor is not None:
            busy_monitor.stop()
        _set_output_busy_monitor(None)
        try:
            server.close()
        except OSError:
            pass
        _cleanup_socket_if_owner(workspace, owner_token)


def _check_pending(record: dict[str, Any]) -> str | None:
    """Check a pending record. Returns 'success' / 'failed' / None (still pending)."""
    from .adapters.base import transcript_has_id_in_new_user_turn

    transcript_path = Path(record.get("targetTranscript", ""))
    pane_id = record.get("targetPane", "")
    message_id = record.get("msgId", "")
    baseline = record.get("baseline", 0)
    deadline = _effective_deadline(record)
    now = time.time()

    if transcript_path.exists() and transcript_has_id_in_new_user_turn(transcript_path, message_id, baseline):
        record["confirmationSource"] = "transcript"
        return "success"

    if _saw_msg_id(pane_id, message_id):
        record["confirmationSource"] = "stream"
        return "success"

    if _pending_terminal_result(record):
        if _exception_followup_active(record, now=now):
            return None
        return _FINALIZE_PENDING

    if now > deadline:
        return "failed"
    return None


def _observation_metadata_for_pending(record: dict[str, Any], result: str) -> dict[str, str]:
    metadata: dict[str, str] = {
        "injectStatus": "submitted",
    }
    if result == "success":
        metadata["turnObserved"] = "confirmed"
    elif result == "failed":
        metadata["turnObserved"] = "unconfirmed"
    source = str(record.get("confirmationSource", ""))
    if result == "success" and source:
        metadata["confirmationSource"] = source
    return metadata


def stop_sidecar(workspace: str) -> None:
    _request_sidecar(workspace, {"action": "shutdown"}, timeout=SOCKET_READY_TIMEOUT)
    deadline = time.monotonic() + SOCKET_READY_TIMEOUT
    while time.monotonic() < deadline:
        if not _socket_path(workspace).exists():
            return
        time.sleep(SOCKET_RETRY_INTERVAL)
    _cleanup_socket(workspace)


if __name__ == "__main__":
    raise SystemExit(_run_spawned_sidecar(sys.argv[1:]))
