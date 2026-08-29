"""Team-scoped sidecar: message transport, runtime signals, notify watcher.

Delivery has exactly one state: the native transport (claude inbox /
codex daemon / grok leader) either accepted the message or refused it.
There is no tracked in-between and no confirmation oracle — acceptance means the
target's own runtime owns it from there.
"""

from __future__ import annotations

import hashlib
import json
import os
import signal
import socket
import subprocess
import sys
import threading
import time
from collections import defaultdict
from pathlib import Path
from typing import Any

from . import bus
from . import devlog
from . import notify_ui
from .agent_cli import detect_cli_process_for_pane, detect_profile_for_pane
from .runtime_state import (
    format_hive_envelope,
    project_thread_event,
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
SOCKET_READY_TIMEOUT = 2.0
SOCKET_RETRY_INTERVAL = 0.1
# The CLI's socket budget must be strictly longer than the work it asks the
# sidecar to perform: worst-case native transport submission (claude inbox
# connect+write / codex daemon RPC / grok leader prompt+ack) plus slack for
# scheduling and payload plumbing.
# A send blocks on nothing else — it returns queued the moment the transport
# accepts; confirmation is asynchronous (background tracker / query-time).
REQUEST_SLACK = 5.0


def _native_submit_timeout() -> float:
    from .adapters import claude_bg, claude_sessions, codex_app_server, grok_leader

    # claude's worst case is a delivery that has to wake a parked engine
    # first (ledger check + tty-less attach + entry poll) before the inbox
    # write itself.
    return max(
        claude_sessions.SUBMIT_TIMEOUT + claude_bg.WAKE_SUBMIT_BUDGET,
        codex_app_server.SUBMIT_TIMEOUT,
        grok_leader.SUBMIT_TIMEOUT,
    )


def _send_request_timeout() -> float:
    return _native_submit_timeout() + REQUEST_SLACK
SIDECAR_API_VERSION = 5
BUSY_OUTPUT_THRESHOLD_SECONDS = 3.0
# A probed session id only speaks for the session it saw: nothing tells the
# sidecar that the human typed `/new` in an unmanaged pane, so the snapshot
# ages out and the adapter re-probes instead of pinning a dead id forever.
_SESSION_SNAPSHOT_FRESHNESS_S = 600.0
_TRANSCRIPT_PATH_CACHE_TTL = 60.0
_OUTPUT_BUSY_MONITOR = None
_TRANSCRIPT_PATH_CACHE: dict[str, tuple[str, float, str]] = {}
_AGENT_NOTIFY_ROLES = {"agent"}
_RUNTIME_SNAPSHOTS = RuntimeSnapshotStore()
# Requests run on their own threads (see _serve_requests). This guards the
# sidecar's own short in-memory mutations — never held across transport,
# subprocess or socket work, which is the starvation this threading exists to
# end. The read caches (_TRANSCRIPT_PATH_CACHE, _CLAUDE_JOBS_CACHE) stay
# unguarded: a lost race there costs a duplicate probe, not correctness.
_STATE_LOCK = threading.Lock()
_SHUTDOWN = threading.Event()
_INFLIGHT_REQUESTS = 0
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
) -> str | None:
    """Return a stable changed build hash that should trigger sidecar reexec."""
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
) -> socket.socket | None:
    """Replace this process with the on-disk build.

    Returns None when nothing was torn down (another sidecar holds the reexec
    lock) — the caller keeps serving on its own socket. When ``execv`` itself
    fails, the old build has to keep serving rather than leave the window with
    a dead sidecar and no socket: the listener is rebound, the output monitor
    restarted, and the replacement socket returned for the caller to serve on.
    """
    lock_fd = _try_acquire_reexec_lock(workspace)
    if lock_fd is None:
        return None

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
        try:
            os.execv(sys.executable, argv)
        except OSError as exc:
            print(
                f"hive sidecar: reexec failed ({exc}); staying on build "
                f"{SIDECAR_BUILD_HASH[:12]}",
                file=sys.stderr,
                flush=True,
            )
    finally:
        if previous_lock_env is None:
            os.environ.pop(_SIDECAR_REEXEC_LOCK_ENV, None)
        else:
            os.environ[_SIDECAR_REEXEC_LOCK_ENV] = previous_lock_env
        _release_reexec_lock_fd(lock_fd)

    # Only reached when execv failed. Rebinding is the recovery; if it too
    # fails the raised OSError takes the loop through its own teardown.
    replacement = _open_server_socket(workspace)
    if busy_monitor is not None:
        busy_monitor.start()
        _set_output_busy_monitor(busy_monitor)
    return replacement



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


def _claude_registry_busy(pane_id: str) -> bool | None:
    """Busy flag from claude's own session registry, or None.

    A bg member pane answers from its job's engine entry; an interactive
    claude on the pane tty answers from its own registry entry (real TUI
    sessions report ``status``; headless/desktop ones do not and stay None).
    """
    from .adapters import claude_bg
    from .agent_cli import claude_pid_for_pane

    job_id = claude_bg.job_id_for_pane(pane_id)
    if job_id:
        engine = claude_bg.engine_session_for_job(job_id)
        if engine is None:
            return None  # parked or gone — no live status either way
        return engine.status == "busy"
    from .adapters import claude_sessions

    reported = claude_sessions.session_status(claude_pid_for_pane(pane_id))
    if reported is None:
        return None
    return reported[0] == "busy"


def _native_daemon_busy(pane_id: str) -> bool | None:
    """Busy flag from the pane's native runtime source (codex shared
    app-server via the pane's thread record, grok per-pane leader, claude's
    own session registry).

    None when no native source holds live state for the pane, which is the
    signal to fall back to the heuristic monitor source.
    """
    if not pane_id:
        return None
    try:
        from .adapters import codex_app_server, grok_leader

        rt = codex_app_server.runtime_for_pane(pane_id)
        if rt is None:
            rt = grok_leader.runtime_for_pane(pane_id)
        if rt is not None:
            return bool(rt.busy)
        return _claude_registry_busy(pane_id)
    except Exception:
        return None


def _pane_is_truly_busy(pane_id: str, monitor: Any) -> bool:
    """Public ``busy`` signal: true when the agent is in mid-turn.

    For panes with a native runtime source (codex daemon, grok leader,
    claude session registry) that source's ``busy`` flag is authoritative
    and short-circuits the heuristic below.

    Heuristic source (no native state): tmux control-mode reports recent
    visible output, with the phantom-redraw gate via transcript jsonl mtime.
    """
    if not pane_id:
        return False

    app_busy = _native_daemon_busy(pane_id)
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

    return False


def _busy_output_payload(pane_id: str) -> dict[str, Any]:
    return {"busy": _pane_is_truly_busy(pane_id, _OUTPUT_BUSY_MONITOR)}


def _is_output_busy(
    pane_id: str,
    monitor: Any,
    *,
    inactive_age: float | None = None,
) -> bool:
    """idle-notify variant of :func:`_pane_is_truly_busy`.

    For panes with a native runtime source (codex daemon, grok leader,
    claude session registry) that source's ``busy`` flag is authoritative
    and short-circuits the heuristic source.

    Heuristic source (no native state): the monitor signal from
    ``_pane_is_truly_busy``, additionally clamped by an ``inactive_age``
    sub-gate: when the window has been inactive for ``inactive_age``
    seconds, ignore monitor output that predates that transition (the user
    already saw it while the window was active — without the clamp,
    idle-notify rearms ~5s after every window switch).
    """
    if not pane_id:
        return False

    app_busy = _native_daemon_busy(pane_id)
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

    return False


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


def _socket_alive(workspace: str) -> bool:
    response = request_ping(workspace)
    return bool(
        response
        and response.get("ok") is True
        and response.get("apiVersion") == SIDECAR_API_VERSION
    )


def request_ping(workspace: str) -> dict[str, Any] | None:
    return _request_sidecar(workspace, {"action": "ping"}, timeout=SOCKET_RETRY_INTERVAL)


def request_connect_codex(workspace: str) -> dict[str, Any] | None:
    """Ask the sidecar to bring its shared-daemon codex client online now.

    Called at spawn time so the client holds the broadcast stream before the
    member's first turn. Best-effort: returns None when the sidecar is down,
    and the lazy connect on the next runtime tick covers that case.
    """
    return _request_sidecar(workspace, {"action": "connect-codex"}, timeout=3.0)


def request_connect_grok(workspace: str, pane: str) -> dict[str, Any] | None:
    """Ask the sidecar to bring a per-pane grok 2nd client online now.

    Called at spawn time so the stdio client has loaded the pane's session
    before its first turn: ``session/load`` replays past updates, and a replay
    is not evidence — only a live-attached client sees the first real turn.
    Best-effort: returns None when the sidecar is down, and the lazy connect on
    the next runtime tick covers that case.
    """
    return _request_sidecar(workspace, {"action": "connect-grok", "pane": pane}, timeout=3.0)


def _sidecar_identity_matches(
    response: dict[str, Any] | None,
    *,
    team: str,
) -> bool:
    """Sidecar identity is (workspace socket, team) — never the window.

    The window is display: it can die, move, or be recreated by attach
    without the team changing, so a window mismatch must not bounce a
    healthy sidecar (and with it every live delivery client it holds).
    """
    return bool(
        response
        and response.get("ok") is True
        and response.get("apiVersion") == SIDECAR_API_VERSION
        and response.get("buildHash") == SIDECAR_BUILD_HASH
        and response.get("team") == team
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
) -> dict[str, Any] | None:
    timeout = _send_request_timeout()
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
        },
        timeout=timeout,
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


# waitingFor values that do not gate a send: a /status-style dialog open in
# an attached viewer parks the status on "waiting", but the inbox still
# queues normally and the message shows the moment the dialog closes.
_SEND_GATE_WAIVED_REASONS = frozenset({"registry:dialog open"})


def _check_send_gate(target) -> None:
    """Raise when the target agent is waiting on its human.

    Reads the member's runtime (native daemon / registry state for
    codex, grok and claude; transcript gate for unmanaged panes) instead of
    re-deriving it — one judgement for every CLI, and no silent skip when a
    transcript cannot be resolved.
    """
    if target.pane_id:
        runtime = _member_runtime_payload(target.pane_id, role="agent")
    else:
        runtime = _headless_member_runtime(target)
    if runtime.get("inputState") != "waiting_user":
        return
    if str(runtime.get("inputReason") or "") in _SEND_GATE_WAIVED_REASONS:
        return
    raise RuntimeError(
        "target agent is waiting for a user answer; answer it in the target pane"
    )


FLOW_MAILBOX_AGENT = "flow"


def _send_payload(
    *,
    workspace: str,
    team_name: str,
    sender_agent: str,
    sender_pane: str,
    target_agent: str,
    body: str,
    artifact: str,
    reply_to: str,
) -> dict[str, Any]:
    if target_agent == FLOW_MAILBOX_AGENT:
        # The flow runner's mailbox: it owns no pane and no transport —
        # the durable bus row IS the delivery, and the runner polls for
        # it. Members answer a flow dispatch with an ordinary
        # `hive send flow`, which lands here.
        event = bus.write_send_event(
            workspace,
            from_agent=sender_agent,
            to_agent=target_agent,
            body=body.strip(),
            artifact=artifact,
            reply_to=reply_to,
        )
        return {"ok": True, "to": target_agent, "msgId": event.msg_id, "mailbox": True}

    team, target = _resolve_live_agent(team_name, target_agent)
    normalized_body = body.strip()

    # Side effect only: raises if target is waiting for a user answer.
    _check_send_gate(target)

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

    payload: dict[str, Any] = {
        "ok": True,
        "to": target_agent,
        "msgId": message_id,
    }
    # Fire-and-forget past this point: the transport verdict is the only
    # delivery state. The daemon/channel either accepted the message (its
    # own contract queues and processes it) or refused it — there is no
    # tracked in-between, no confirmation oracle, and nothing to poll. A
    # claude member mid-turn queues the message itself (`priority: next`
    # folds it in at the next tool boundary) — no sidecar hold on top.
    try:
        target.send(envelope)
    except Exception as exc:
        return {"ok": False, "error": f"transport refused {target_agent}: {exc}", "msgId": message_id}

    if artifact:
        payload["artifact"] = artifact
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
    if "cliAlive" in runtime:
        diag["cliAlive"] = bool(runtime["cliAlive"])
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
        if runtime.get("_cli") == "codex":
            from .adapters import codex_app_server

            diag["codexDaemon"] = {
                "socket": str(codex_app_server.shared_socket_path()),
                "alive": codex_app_server.daemon_alive(),
                "threadId": codex_app_server.thread_id_for_pane(target.pane_id),
            }
        if runtime.get("_cli") == "claude":
            from .adapters import claude_bg

            job_id = claude_bg.job_id_for_pane(target.pane_id)
            if job_id:
                diag["claudeJob"] = {
                    "jobId": job_id,
                    "engineAlive": claude_bg.engine_session_for_job(job_id) is not None,
                }
            if "_viewKind" in runtime:
                # What the pane's viewer is showing right now — the member's
                # own job, another session, or the panel list.
                diag["claudeView"] = {
                    "kind": runtime["_viewKind"],
                    "certainty": runtime.get("_viewCertainty", ""),
                    "jobId": runtime.get("_viewedJob", ""),
                    "member": runtime.get("_viewedMember", ""),
                    "onMember": bool(job_id) and runtime.get("_viewedJob") == job_id,
                }
        if "_engineState" in runtime:
            diag["engineState"] = runtime["_engineState"]
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
    """Native codex runtime from the shared daemon, or None if unmanaged.

    A hive-managed codex pane has a recorded thread on the shared app-server
    daemon; reading busy/turn from its status stream is both accurate and
    cheap versus tailing the transcript. Returns None for an unmanaged codex
    (no thread record / no daemon) so the caller falls back to the transcript
    path.
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


def _grok_leader_runtime(pane_id: str) -> dict[str, Any] | None:
    """Native grok runtime from the pane's leader, or None if no daemon.

    hive-spawned grok panes run their own leader daemon; hive's second client
    folds its ACP notifications into busy/turn state. Returns None for a grok
    hive never spawned — it has no socket and no session record, so there is
    nothing to read (grok has no transcript probe to fall back to).
    """
    from .adapters import grok_leader

    rt = grok_leader.runtime_for_pane(pane_id)
    if rt is None:
        return None
    input_state = rt.input_state or "ready"
    fields: dict[str, Any] = {
        "busy": rt.busy,
        "turnPhase": rt.turn_phase,
        "inputState": input_state,
        "inputReason": "" if input_state != "waiting_user" else "leader_permission_request",
        "_runtimeSource": "grok-leader",
    }
    return fields


def _claude_bg_runtime(pane_id: str) -> dict[str, Any] | None:
    """Native claude runtime from the pane's bg job, or None if unmanaged."""
    from .adapters import claude_bg

    record = claude_bg.read_pane_job(pane_id)
    if record is None:
        return None
    job_id, record_session, _cwd = record
    return _claude_job_runtime(job_id, record_session)


def _claude_job_runtime(job_id: str, record_session: str = "") -> dict[str, Any]:
    """Native claude runtime keyed by the job itself (pane optional).

    Liveness is three-tier: a live engine entry (alive — its ``status`` is
    the truth); a ledger row without a live engine (asleep — the supervisor
    parks idle jobs after ~1h, delivery wakes them, so asleep is not dead
    and is never reaped); no ledger row (gone). The ledger costs a CLI call
    (~270ms), so it is consulted only when the engine entry is missing,
    behind a short cache.
    """
    from .adapters import claude_bg

    engine = claude_bg.engine_session_for_job(job_id)
    if engine is not None:
        fields = claude_bg.runtime_from_engine(engine)
        fields["cliAlive"] = True
        fields["sessionId"] = engine.session_id or record_session or "unresolved"
        return fields
    fields = {"_runtimeSource": "claude_bg", "busy": False}
    rows = _claude_jobs_cached()
    if rows is None:
        fields.update(
            cliAlive=True,
            inputState="unknown",
            inputReason="ledger_unavailable",
            sessionId=record_session or "unresolved",
        )
        return fields
    row = rows.get(job_id)
    if row is None:
        fields.update(
            cliAlive=False,
            inputState="offline",
            inputReason="engine_gone",
            sessionId=record_session or "unresolved",
        )
        return fields
    # Asleep: parked engine. It still accepts input — delivery wakes it — so
    # it reads as an idle, reachable member, never as a dead one.
    fields.update(
        cliAlive=True,
        _engineState="asleep",
        inputState="ready",
        inputReason="",
        sessionId=str(row.get("sessionId") or "") or record_session or "unresolved",
    )
    return fields


def _claude_view_fields(pane_id: str) -> dict[str, Any]:
    """What the pane's attach viewer is actually showing (the human can
    switch it to any other bg session)."""
    from .adapters import claude_view

    try:
        view = claude_view.view_for_pane(pane_id)
    except Exception:
        return {}  # a diagnostic field must never break the runtime payload
    return {
        "_viewKind": view.kind,
        "_viewCertainty": view.certainty,
        "_viewedJob": view.job_id,
        "_viewedMember": view.member,
    }


_CLAUDE_JOBS_CACHE_TTL = 30.0
_CLAUDE_JOBS_CACHE: tuple[float, dict[str, dict[str, Any]] | None] | None = None


def _claude_jobs_cached() -> dict[str, dict[str, Any]] | None:
    """Job ledger rows keyed by jobId, or None when the CLI call failed.

    Cached briefly: the ledger is only read when an engine entry is missing
    (rare state), and a ~270ms node start must not run per tick per pane.
    """
    global _CLAUDE_JOBS_CACHE
    now = time.monotonic()
    if _CLAUDE_JOBS_CACHE is not None and now < _CLAUDE_JOBS_CACHE[0]:
        return _CLAUDE_JOBS_CACHE[1]
    from .adapters import claude_bg

    rows = claude_bg.list_jobs()
    indexed = (
        {str(row.get("id") or ""): row for row in rows if row.get("id")}
        if rows is not None
        else None
    )
    _CLAUDE_JOBS_CACHE = (now + _CLAUDE_JOBS_CACHE_TTL, indexed)
    return indexed


def _agent_runtime_payload(
    pane_id: str,
    *,
    runtime_snapshot: RuntimeSnapshot | None = None,
) -> dict[str, Any]:
    from . import adapters, tmux
    from .adapters.base import check_input_gate
    from .agent_cli import resolve_model_for_pane

    runtime: dict[str, Any] = {
        "alive": tmux.is_pane_alive(pane_id),
    }
    runtime.update(_busy_output_payload(pane_id))
    if not runtime["alive"]:
        runtime["cliAlive"] = False
        runtime["busy"] = False
        runtime["inputState"] = "offline"
        runtime["inputReason"] = "pane_dead"
        return runtime

    # Liveness is runtime evidence only: a retained shell keeps the pane, a
    # stale title, the @hive-cli tag and a surviving thread/job record alive,
    # and none of that alone makes it an agent runtime. For claude the
    # evidence is the bg job's registry/ledger state — the engine never
    # lives on the pane tty, so the process table only proves the viewer.
    profile = detect_cli_process_for_pane(pane_id)
    runtime["cliAlive"] = profile is not None
    runtime["_cli"] = profile.name if profile else "unknown"
    if profile is None or profile.name == "claude":
        bg_runtime = _claude_bg_runtime(pane_id)
        if bg_runtime is not None:
            runtime["_cli"] = "claude"
            resolved_model = resolve_model_for_pane(pane_id, cli_name="claude", current_model="")
            if resolved_model:
                runtime["model"] = resolved_model
            runtime.update(bg_runtime)
            runtime.update(_claude_view_fields(pane_id))
            return runtime
    if not profile:
        runtime["busy"] = False  # shell output is not agent activity
        runtime["inputState"] = "offline"
        runtime["inputReason"] = "cli_exited"
        return runtime

    resolved_model = resolve_model_for_pane(
        pane_id,
        cli_name=profile.name if profile else "",
        current_model="",
    )
    if resolved_model:
        runtime["model"] = resolved_model

    adapter = adapters.get(profile.name)
    if not adapter:
        runtime["inputState"] = "unknown"
        runtime["inputReason"] = "no_session"
        return runtime

    # A hive-managed codex has a recorded thread on the shared app-server
    # daemon: read native runtime signals (busy / turn) over the socket
    # instead of reverse-engineering them from the transcript, and its
    # session id IS the recorded threadId — no probing. An unmanaged codex
    # (no record) falls through to the transcript path below.
    if profile.name == "codex":
        app_runtime = _codex_app_server_runtime(pane_id)
        if app_runtime is not None:
            from .adapters import codex_app_server

            runtime.update(app_runtime)
            runtime["sessionId"] = (
                codex_app_server.session_id_for_pane(pane_id) or "unresolved"
            )
            return runtime

    # hive-spawned grok is the same shape over its per-pane leader daemon, and
    # its session id needs no probing: hive minted it at spawn time and wrote
    # it beside the socket. Unlike codex it never falls through to the
    # transcript path — that gate only knows claude/codex record shapes and
    # reads a pending grok permission request as clear — so with no leader
    # state the honest answer is unknown.
    if profile.name == "grok":
        from .adapters import grok_leader

        leader_runtime = _grok_leader_runtime(pane_id)
        runtime["sessionId"] = grok_leader.session_id_for_pane(pane_id) or "unresolved"
        if leader_runtime is not None:
            runtime.update(leader_runtime)
        else:
            runtime["inputState"] = "unknown"
            runtime["inputReason"] = "no_leader_runtime"
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
        runtime["sessionId"] = session_id or "unresolved"
        if session_id:
            with _STATE_LOCK:
                snapshot = _RUNTIME_SNAPSHOTS.update_session_id(
                    pane_id,
                    session_id,
                    source=source,
                    freshness_s=_SESSION_SNAPSHOT_FRESHNESS_S,
                )
            runtime.update(snapshot.to_runtime_fields())

    # An interactive claude reports its own state in the session registry —
    # the same fields the bg engine path maps. It is the authority when it
    # speaks: the transcript gate can only see an AskUserQuestion record, so
    # it reads every other wait (and a stale ask) wrong, and the send gate
    # refuses on that verdict.
    if profile.name == "claude":
        from .adapters import claude_sessions
        from .agent_cli import claude_pid_for_pane

        reported = claude_sessions.session_status(claude_pid_for_pane(pane_id))
        if reported is not None:
            runtime.update(claude_sessions.runtime_from_status(*reported))
            runtime["_runtimeSource"] = "claude_registry"
            return runtime

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


def _headless_member_runtime(agent) -> dict[str, Any]:
    """Runtime for a registry member with no pane: the engine IS the member.

    ``alive`` mirrors engine liveness (there is no pane to be alive), and
    ``headless`` marks the row so consumers can tell a closed display from a
    dead member.
    """
    runtime: dict[str, Any] = {"alive": False, "headless": True, "busy": False}
    sid = str(getattr(agent, "session_id", "") or "")
    cli = getattr(agent, "cli", "") or ""
    if cli == "claude" and sid:
        runtime.update(_claude_job_runtime(sid))
    elif cli == "codex" and sid:
        from .adapters import codex_app_server

        rt = codex_app_server.runtime_for_thread(sid)
        if rt is None:
            runtime.update(cliAlive=False, inputState="unknown", inputReason="no_daemon_runtime")
        else:
            input_state = rt.input_state or "ready"
            runtime.update(
                cliAlive=True,
                busy=rt.busy,
                turnPhase=rt.turn_phase,
                inputState=input_state,
                inputReason="" if input_state != "waiting_user" else "app_server_active_flag",
                _runtimeSource="codex_app_server",
            )
        runtime["sessionId"] = sid
    elif cli == "grok":
        from .adapters import grok_leader

        key = grok_leader.member_key(getattr(agent, "team_name", "") or "", getattr(agent, "name", "") or "")
        rt = grok_leader.runtime_for_key(key)
        if rt is None:
            runtime.update(cliAlive=False, inputState="unknown", inputReason="no_leader_runtime")
        else:
            input_state = rt.input_state or "ready"
            runtime.update(
                cliAlive=True,
                busy=rt.busy,
                turnPhase=rt.turn_phase,
                inputState=input_state,
                inputReason="" if input_state != "waiting_user" else "leader_permission_request",
                _runtimeSource="grok-leader",
            )
        record = grok_leader.read_session_key(key)
        runtime["sessionId"] = (record[0] if record else "") or sid or "unresolved"
    else:
        runtime.update(cliAlive=False, inputState="unknown", inputReason="no_engine_identity")
    runtime["alive"] = bool(runtime.get("cliAlive"))
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
        if agent.pane_id:
            runtime = _member_runtime_payload(agent.pane_id, role="agent")
        else:
            runtime = _headless_member_runtime(agent)
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
        if (
            pane_id
            and pane_id not in panes
            and tmux.is_pane_alive(pane_id)
            and detect_cli_process_for_pane(pane_id) is not None
        ):
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
            if (
                pane_id
                and pane_id not in agent_panes
                and tmux.is_pane_alive(pane_id)
                and detect_cli_process_for_pane(pane_id) is not None
            ):
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
            notify_debug.emit(
                workspace,
                "fire.attempt",
                team=team_name,
                window=window_target,
                target_pane=target_pane,
                idle_seconds=now - last_busy_ts,
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


def _thread_payload(workspace: str, message_id: str) -> dict[str, Any]:
    events = bus.read_events_with_ns(workspace)
    send_events: dict[str, tuple[int, dict[str, object]]] = {}
    children: dict[str, list[str]] = defaultdict(list)

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
        items.append(item)

    return {
        "ok": True,
        "rootMsgId": root_id,
        "focusMsgId": message_id,
        "messages": items,
    }


def _is_tmux_window_alive(tmux_window_id: str) -> bool:
    from . import tmux

    return tmux.window_exists(tmux_window_id)


def ensure_sidecar(workspace: str, team: str, tmux_window: str, tmux_window_id: str) -> int | None:
    """Ensure the team sidecar socket is alive."""
    lock_path = _lock_path(workspace)
    lock_path.parent.mkdir(parents=True, exist_ok=True)

    import fcntl

    lock_fd = os.open(str(lock_path), os.O_CREAT | os.O_RDWR)
    try:
        fcntl.flock(lock_fd, fcntl.LOCK_EX)
        response = request_ping(workspace)
        if _sidecar_identity_matches(response, team=team):
            return None
        if response:
            stop_sidecar(workspace)
        _cleanup_socket(workspace)
        pid = _start_sidecar(workspace, team, tmux_window, tmux_window_id)
        deadline = time.monotonic() + SOCKET_READY_TIMEOUT
        while time.monotonic() < deadline:
            response = request_ping(workspace)
            if _sidecar_identity_matches(response, team=team):
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


def _handle_request(
    *,
    workspace: str,
    team: str,
    tmux_window: str,
    tmux_window_id: str,
    sidecar_started_at: str,
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
                sender_agent=str(request.get("senderAgent", "")),
                sender_pane=str(request.get("senderPane", "")),
                target_agent=str(request.get("targetAgent", "")),
                body=str(request.get("body", "")),
                artifact=str(request.get("artifact", "")),
                reply_to=str(request.get("replyTo", "")),
            )
        except Exception as exc:
            response = {"ok": False, "error": str(exc)}
        return response, True
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
            response = _thread_payload(workspace, str(request.get("msgId", "")))
        except Exception as exc:
            response = {"ok": False, "error": str(exc)}
        return response, True
    if action == "connect-codex":
        try:
            from .adapters import codex_app_server
            response = {"ok": True, "connected": codex_app_server.connect()}
        except Exception as exc:
            response = {"ok": False, "error": str(exc)}
        return response, True
    if action == "connect-grok":
        try:
            from .adapters import grok_leader
            pane = str(request.get("pane") or "")
            connected = bool(pane) and grok_leader.connect_pane(pane)
            response = {"ok": True, "connected": connected}
        except Exception as exc:
            response = {"ok": False, "error": str(exc)}
        return response, True
    if action == "shutdown":
        return {"ok": True}, False
    return {"ok": False, "error": "unknown action"}, True


def _requests_in_flight() -> bool:
    with _STATE_LOCK:
        return _INFLIGHT_REQUESTS > 0


def _serve_connection(
    conn: socket.socket,
    *,
    workspace: str,
    team: str,
    tmux_window: str,
    tmux_window_id: str,
    sidecar_started_at: str,
    read_timeout: float,
) -> None:
    global _INFLIGHT_REQUESTS

    with _STATE_LOCK:
        _INFLIGHT_REQUESTS += 1
    try:
        with conn:
            conn.settimeout(read_timeout)
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
                request=request if isinstance(request, dict) else {},
            )
            try:
                conn.sendall((json.dumps(response, ensure_ascii=False) + "\n").encode())
            except OSError:
                pass
            # Answer first, then retire: the reply must be on the wire before
            # the loop tears the socket down.
            if not keep_running:
                _SHUTDOWN.set()
    finally:
        with _STATE_LOCK:
            _INFLIGHT_REQUESTS -= 1


def _serve_requests(
    *,
    server: socket.socket,
    workspace: str,
    team: str,
    tmux_window: str,
    tmux_window_id: str,
    sidecar_started_at: str,
    timeout: float,
) -> bool:
    """Accept for up to ``timeout`` seconds, handling each request off-loop.

    Handlers run on their own thread because their budgets differ by an order
    of magnitude: a delivery may hold the native transport for
    ``_send_request_timeout()`` while ``hive team`` / ``hive doctor`` give up
    after ``SOCKET_READY_TIMEOUT`` and report a missing sidecar. Serving them
    in accept order made one slow send fake the sidecar's death for every
    short read behind it.
    """
    end = time.monotonic() + timeout
    while not _SHUTDOWN.is_set():
        remaining = end - time.monotonic()
        if remaining <= 0:
            break
        server.settimeout(remaining)
        try:
            conn, _ = server.accept()
        except socket.timeout:
            break
        except OSError:
            break

        threading.Thread(
            target=_serve_connection,
            args=(conn,),
            kwargs={
                "workspace": workspace,
                "team": team,
                "tmux_window": tmux_window,
                "tmux_window_id": tmux_window_id,
                "sidecar_started_at": sidecar_started_at,
                "read_timeout": timeout,
            },
            name="hive-sidecar-request",
            daemon=True,
        ).start()
    return not _SHUTDOWN.is_set()


_GROK_REAP_GRACE_SECONDS = 120.0


def _cleanup_dead_daemons(workspace: str) -> None:
    """Reap grok leader daemons that nothing owns any more.

    Two lifecycles, told apart by key shape:

    - ``m-<team>.<member>`` — registry-driven: the engine belongs to a team
      member, so a dead pane means nothing (the display closed). Reap only
      when the team's registry file is *valid and lists no such member*
      (kill/delete removed it), or the file is *missing entirely* (the team
      was deleted/archived). An unreadable entry is never grounds to kill a
      daemon, and a young pidfile gets a grace window so a spawn's
      registration in flight cannot be raced.
    - ``p<slug>`` — a raw ``hive grok`` pane outside any team keeps the old
      pane lifecycle: pane gone, daemon reaped.

    Killing a leader takes its attached TUI down with it, so every reap is
    logged; ``is_pane_alive`` only reports dead panes from a successful tmux
    listing, never from a transient tmux failure.
    """
    from . import notify_debug, registry, tmux
    from .adapters import grok_leader

    for key in grok_leader.list_daemon_keys():
        binding = grok_leader.member_from_key(key)
        if binding is None:
            slug = key[1:]
            if not slug.isdigit():
                continue
            pane = f"%{slug}"
            if tmux.is_pane_alive(pane):
                continue
        else:
            team, member = binding
            path = registry.entry_path(team)
            if path is None:
                continue
            if path.is_file():
                entry = registry.load(team)
                if entry is None:
                    continue  # unreadable is not proof of absence
                if any(m.get("name") == member for m in entry.get("members", [])):
                    continue
            # Missing registry file, or a valid roster without this member:
            # the engine is an orphan — but never a newborn one.
            try:
                pidfile = grok_leader.socket_path_for_key(key).with_suffix(".pid")
                age = time.time() - pidfile.stat().st_mtime
            except OSError:
                continue  # no pidfile yet: daemon mid-start
            if age < _GROK_REAP_GRACE_SECONDS:
                continue
        notify_debug.emit(workspace, "daemon.reap", key=key)
        # Drop the pool's client BEFORE killing the daemon: a grok stdio
        # client that outlives its leader auto-spawns a replacement on the
        # same socket, resurrecting an orphan mid-reap.
        grok_leader.pool().drop_key(key)
        grok_leader.kill_daemon_key(key)


# One send_keys attempt per pane per cooldown window, so a slow-starting codex
# is not typed at twice while the process check cannot see it yet.
_CODEX_REATTACH_COOLDOWN_SECONDS = 60.0
_CODEX_REATTACH_AT: dict[str, float] = {}


def _codex_supervisor_tick(workspace: str, team: str) -> None:
    """Keep this team's codex members riding the shared daemon.

    1. Prune pane thread records whose pane died (machine-level records, so
       staleness never rebinds a recycled pane id to a foreign thread).
    2. If any of this team's codex members is alive but the shared daemon is
       not answering, respawn it and log the event. A workspace with no live
       codex member leaves the daemon alone — it is machine-level shared
       state, other teams (or the human) may be using it, and hive never
       kills it.
    3. A member pane whose CLI exited (retained shell) but whose thread is
       recorded gets one `hive codex resume <threadId>` typed into its shell
       — the daemon surviving means the thread is still live, so the member
       is re-attached instead of left dead. Guarded by a live-process check,
       a shell-prompt check, and a per-pane cooldown.
    """
    from . import notify_debug, tmux
    from .adapters import codex_app_server
    from .agent_cli import is_shell_command
    from .team import Team

    live_panes: set[str] = set()
    try:
        panes = tmux.list_panes_all()
    except Exception:
        panes = []
    for p in panes:
        live_panes.add(p.pane_id)
    if panes:
        for pane in codex_app_server.list_recorded_panes():
            if pane not in live_panes:
                codex_app_server.clear_pane_thread(pane)
                _CODEX_REATTACH_AT.pop(pane, None)

    try:
        t = Team.load(team)
    except Exception:
        return
    members = [
        agent for agent in t.agents.values()
        if agent.cli == "codex" and agent.pane_id in live_panes
    ]
    if not members:
        return

    if not codex_app_server.daemon_alive():
        codex_app_server.drop_client()
        respawned = codex_app_server.spawn_daemon()
        notify_debug.emit(workspace, "codex.daemon.respawn", ok=respawned)
        if not respawned:
            return

    now = time.monotonic()
    for agent in members:
        thread_id = codex_app_server.thread_id_for_pane(agent.pane_id)
        if not thread_id:
            continue
        if detect_cli_process_for_pane(agent.pane_id) is not None:
            continue  # CLI (codex or another agent) is on the TTY — leave it
        if now - _CODEX_REATTACH_AT.get(agent.pane_id, 0.0) < _CODEX_REATTACH_COOLDOWN_SECONDS:
            continue
        command = tmux.display_value(agent.pane_id, "#{pane_current_command}") or ""
        if not is_shell_command(command):
            continue  # not at a shell prompt (vim, ssh, …): never type into it
        _CODEX_REATTACH_AT[agent.pane_id] = now
        notify_debug.emit(
            workspace, "codex.member.reattach",
            pane=agent.pane_id, agent=agent.name, thread=thread_id,
        )
        tmux.send_keys(agent.pane_id, f"hive codex resume {thread_id}")


def _claude_supervisor_tick(workspace: str) -> None:
    """Prune claude pane job records whose pane died; park the orphans.

    Records are machine-level (like codex's thread records), so staleness
    must never rebind a recycled pane id to a foreign job. A record whose
    pane is gone also means nobody is watching that engine any more:
    ``claude stop`` parks it — the job stays in the ledger and ``hive
    resume`` can still wake it, so nothing is lost, but no orphan engine
    keeps burning in the background.

    No respawn/reattach half: the engine's life is claude's own supervisor's
    business (wake happens on demand at delivery), and the pane viewer
    self-heals through the managed launcher's attach loop — a user who
    deliberately left the loop must not be typed at.
    """
    from . import notify_debug, tmux
    from .adapters import claude_bg

    try:
        panes = tmux.list_panes_all()
    except Exception:
        return
    if not panes:
        return  # an empty listing is a tmux failure, not an empty server
    live_panes = {p.pane_id for p in panes}
    for pane in claude_bg.list_recorded_panes():
        if pane in live_panes:
            continue
        record = claude_bg.read_pane_job(pane)
        claude_bg.clear_pane_job(pane)
        if record:
            notify_debug.emit(workspace, "claude.job.park", pane=pane, job=record[0])
            claude_bg.stop_job(record[0])


def _claude_name_tick(*, members: dict[str, dict[str, Any]], team: str, state: dict[str, Any]) -> None:
    """Keep each claude member's job labelled `<team>.<member>`.

    A member spawned by hive is minted under that name already; one adopted
    from a pane that was running claude first (init, spawn, resume) was minted
    before the pane carried any tag, so its job keeps a `hive-<pane>`
    placeholder. The engine's registry entry — read anyway on every tick —
    carries the current label, so the comparison is free and the rename fires
    at most once per job.

    The rename is one control frame, but its confirmation polls the registry
    for up to a few seconds, so it goes to a thread: identity repair must not
    stall delivery.
    """
    import threading

    from .adapters import claude_bg

    done: set[str] = state.setdefault("named", set())
    for member, binding in sorted(members.items()):
        if binding.get("cli") != "claude":
            continue
        job_id = claude_bg.job_id_for_pane(str(binding.get("pane") or "")) or ""
        want = f"{team}.{member}"
        if not job_id or job_id in done:
            continue
        engine = claude_bg.engine_session_for_job(job_id)
        if engine is None:
            continue  # asleep or gone: retry on a later tick
        done.add(job_id)
        if engine.name == want:
            continue
        threading.Thread(
            target=claude_bg.ensure_job_named, args=(job_id, want), daemon=True
        ).start()


def _claude_view_tick(
    *,
    workspace: str,
    team: str,
    members: dict[str, dict[str, Any]],
    state: dict[str, Any],
) -> None:
    """Follow the human's attach-panel switches on this team's claude panes.

    A member pane is an attach viewer: pressing the panel key inside it opens
    any other bg session, and the pane keeps its member tags while the screen
    shows something else. Each pane's ``@hive-view`` tag carries what is
    really on screen (empty while it shows its own member) and the border
    renders it; a switch onto *another* hive member is also logged, which is
    what a whole-window follow would key on later.

    Two cheap signals gate the work: the attach journal's entry set (an entry
    appears/disappears on every attach, switch and detach) and the panes'
    titles (the panel writes the viewed session's name). Probing costs a ps
    per pane, so it only runs when one of those changed.
    """
    from . import notify_debug, tmux
    from .adapters import claude_bg, claude_view

    panes = tmux.list_panes_all()
    if not panes:
        return  # an empty listing is a tmux failure, not an empty server
    titles = {pane.pane_id: pane.title for pane in panes if pane.cli == "claude"}
    signature = (claude_view.journal_signature(), tuple(sorted(titles.items())))
    if signature == state.get("signature"):
        return
    state["signature"] = signature
    labels: dict[str, str] = state.setdefault("labels", {})

    for name, binding in sorted(members.items()):
        pane_id = str(binding.get("pane") or "")
        if binding.get("cli") != "claude" or pane_id not in titles:
            continue
        own_job = claude_bg.job_id_for_pane(pane_id) or ""
        view = claude_view.view_for_pane(pane_id, panes=panes)
        label = claude_view.view_label(view, own_job)
        if labels.get(pane_id) == label:
            continue
        labels[pane_id] = label
        tmux.set_pane_option(pane_id, "hive-view", label)
        if view.kind == "member_view" and view.job_id != own_job:
            notify_debug.emit(
                workspace,
                "claude.view.foreign_member",
                team=team,
                member=name,
                pane=pane_id,
                viewing=view.member,
                viewingJob=view.job_id,
                otherTeam=view.member.split(".", 1)[0] != team,
                certainty=view.certainty,
            )


def _write_registry_backfill(workspace: str, team: str) -> None:
    """Backfill the team's registry entry from live observation.

    Refreshes fields of members the registry already knows (model switch,
    cwd change, a sessionId learned late) and the display cache. It never
    adds or removes a roster name — membership belongs to the CLI writers,
    and the whole read-merge-write runs under the store lock so an
    observation racing a `hive kill` cannot resurrect the killed member.
    """
    from . import registry
    from .agent_cli import resolve_model_for_pane
    from .team import Team

    try:
        t = Team.load(team)
    except (FileNotFoundError, KeyError, ValueError):
        return
    if not t.name or not t.agents:
        return
    observed: list[dict[str, str]] = []
    for name, agent in sorted(t.agents.items()):
        if not agent.pane_id:
            continue  # registry-only member: nothing on screen to observe
        session_id = _fresh_snapshot_session_id(agent.pane_id) or (agent.session_id or "")
        if not session_id and agent.cli == "grok":
            # Daemon-family runtimes never reach the transcript-probe path
            # that feeds runtime snapshots, so a grok member's session id
            # must come straight from its leader record.
            from .adapters import grok_leader
            session_id = grok_leader.session_id_for_pane(agent.pane_id) or ""
        model = resolve_model_for_pane(agent.pane_id, cli_name=agent.cli, current_model="")
        observed.append({
            "name": name,
            "cli": agent.cli,
            "model": model or agent.model,
            "sessionId": session_id,
            "cwd": agent.cwd,
        })

    registry.backfill(
        t.name,
        observed,
        created_at=str(t.created_at),
        display=t.tmux_window_id,
        workspace=workspace,
    )


def _sidecar_loop(workspace: str, team: str, tmux_window: str, tmux_window_id: str) -> None:
    from . import tmux

    from . import notify_debug

    _SHUTDOWN.clear()
    sidecar_started_at = _now_iso()
    idle_notify: dict[str, dict[str, Any]] = {}
    notify_debug_state: dict[str, Any] = {}
    code_reexec_state: dict[str, Any] = {}
    claude_view_state: dict[str, Any] = {}
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
                # The registry entry is the team's existence; the tmux window
                # is only its display. A dead window no longer retires the
                # sidecar (engines keep running headless) — a *missing*
                # registry file does (`hive delete` archives it). Corrupt or
                # foreign-instance entries are not "missing": never retire on
                # a read that might be wrong.
                from . import registry

                path = registry.entry_path(team)
                if path is not None and not path.is_file():
                    if not _is_tmux_window_alive(tmux_window_id):
                        return

            if now - last_daemon_cleanup >= 30.0:
                last_daemon_cleanup = now
                _cleanup_dead_daemons(workspace)
                try:
                    _codex_supervisor_tick(workspace, team)
                except Exception:
                    # Supervision must never take the sidecar down.
                    pass
                try:
                    _claude_supervisor_tick(workspace)
                except Exception:
                    pass
                try:
                    _write_registry_backfill(workspace, team)
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
            )
            # Never exec out from under an in-flight request thread: its
            # transport work would die mid-flight with the message already on
            # the bus. The stale hash is still stale 5s later.
            if stale_hash and not _requests_in_flight():
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

                replacement = _reexec_sidecar(
                    workspace=workspace,
                    team=team,
                    tmux_window=tmux_window,
                    tmux_window_id=tmux_window_id,
                    server=server,
                    busy_monitor=busy_monitor,
                    on_reexec=_emit_reexec,
                )
                if replacement is not None:
                    # exec failed: keep serving the old build on the rebound
                    # socket instead of dying with the socket torn down.
                    server = replacement

            tick_members = _team_member_bindings(team)

            try:
                _claude_name_tick(members=tick_members, team=team, state=claude_view_state)
                _claude_view_tick(
                    workspace=workspace,
                    team=team,
                    members=tick_members,
                    state=claude_view_state,
                )
            except Exception:
                # Border cosmetics must never take the sidecar down.
                pass

            if not _serve_requests(
                server=server,
                workspace=workspace,
                team=team,
                tmux_window=tmux_window,
                tmux_window_id=tmux_window_id,
                sidecar_started_at=sidecar_started_at,
                timeout=IDLE_NOTIFY_TICK_SECONDS,
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
