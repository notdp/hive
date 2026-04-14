"""Team-scoped sidecar for pending send lifecycle tracking.

The sidecar is a background process that:
1. Polls transcript for pending sends
2. Writes observation events to events/
3. Injects HIVE-SYSTEM exception blocks to sender panes on failure

Lifecycle: starts with team (init/create), stops with team (delete),
self-exits when team window/workspace is gone.
"""

from __future__ import annotations

import json
import os
import signal
import time
from pathlib import Path

from .agent_cli import detect_profile_for_pane

IDLE_SLEEP = 5.0  # seconds between scans when no pending work
ACTIVE_SLEEP = 0.5  # seconds between scans when pending work exists
OBSERVATION_TIMEOUT = 60.0  # max seconds to track a single message
POST_QUEUE_TIMEOUT = 10.0  # extra wait after queue disappears


def _now_iso() -> str:
    from datetime import UTC, datetime
    return datetime.now(UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def _write_observation(workspace: str, message_id: str, result: str) -> None:
    ts = _now_iso()
    path = Path(workspace) / "events" / f"{time.time_ns()}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "from": "_system",
        "to": "",
        "intent": "observation",
        "metadata": {
            "msgId": message_id,
            "result": result,
            "observedAt": ts,
        },
        "createdAt": ts,
    }
    path.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n")


def _inject_exception(pane_id: str, message_id: str, target_agent: str, result: str) -> None:
    """Inject a HIVE-SYSTEM exception block into the sender's pane."""
    from . import tmux

    if result == "unconfirmed":
        body = f"Message {message_id} to {target_agent} was not confirmed within {int(OBSERVATION_TIMEOUT)}s. Do not assume delivery."
    else:
        body = f"Message {message_id} to {target_agent}: delivery tracking lost."

    block = (
        f"<HIVE-SYSTEM type=delivery-exception msgId={message_id} "
        f"result={result} to={target_agent}>\n{body}\n</HIVE-SYSTEM>"
    )
    try:
        tmux.send_keys(pane_id, block, enter=True)
    except Exception:
        pass  # best-effort


def enqueue_pending(workspace: str, message_id: str, sender_agent: str,
                    sender_pane: str, target_agent: str,
                    transcript_path: str, baseline: int, *,
                    target_pane: str = "",
                    target_cli: str = "",
                    runtime_queue_state: str = "unknown",
                    queue_source: str = "none",
                    queue_probe_text: str = "") -> None:
    """Write a pending record for the sidecar to track."""
    pending_dir = Path(workspace) / "state" / "pending"
    pending_dir.mkdir(parents=True, exist_ok=True)
    record = {
        "msgId": message_id,
        "senderAgent": sender_agent,
        "senderPane": sender_pane,
        "targetAgent": target_agent,
        "targetPane": target_pane,
        "targetCli": target_cli,
        "targetTranscript": transcript_path,
        "baseline": baseline,
        "runtimeQueueState": runtime_queue_state,
        "queueSource": queue_source,
        "queueProbeText": queue_probe_text,
        "createdAt": _now_iso(),
        "deadlineAt": time.time() + OBSERVATION_TIMEOUT,
    }
    if runtime_queue_state == "queued":
        now = time.time()
        record["firstQueuedAt"] = now
        record["lastQueuedAt"] = now
    record["lastQueueProbeAt"] = time.time()
    (pending_dir / f"{message_id}.json").write_text(
        json.dumps(record, indent=2, ensure_ascii=False) + "\n"
    )


def detect_runtime_queue_state(
    *,
    pane_id: str,
    message_id: str,
    queue_probe_text: str,
    transcript_path: str,
    baseline: int,
    cli_name: str = "",
) -> dict[str, str]:
    resolved_cli = cli_name
    if not resolved_cli and pane_id:
        profile = detect_profile_for_pane(pane_id)
        resolved_cli = profile.name if profile else ""

    if resolved_cli == "claude":
        state = _detect_claude_queue_state(Path(transcript_path), message_id, baseline)
        if state != "unknown":
            return {"state": state, "source": "transcript", "observedAt": _now_iso()}
        return {"state": "unknown", "source": "none", "observedAt": _now_iso()}

    if resolved_cli == "codex":
        state = _detect_capture_queue_state(
            pane_id,
            message_id,
            "Messages to be submitted after next tool call",
            queue_probe_text=queue_probe_text,
        )
        source = "capture" if state != "unknown" else "none"
        return {"state": state, "source": source, "observedAt": _now_iso()}

    if resolved_cli == "droid":
        state = _detect_capture_queue_state(
            pane_id,
            message_id,
            "Queued messages:",
            queue_probe_text=queue_probe_text,
        )
        source = "capture" if state != "unknown" else "none"
        return {"state": state, "source": source, "observedAt": _now_iso()}

    return {"state": "unknown", "source": "none", "observedAt": _now_iso()}


def _detect_claude_queue_state(transcript_path: Path, message_id: str, baseline: int) -> str:
    from .adapters.base import safe_json_loads

    if not transcript_path.exists():
        return "unknown"

    try:
        with transcript_path.open("r") as handle:
            handle.seek(baseline)
            data = handle.read()
    except OSError:
        return "unknown"

    state = "not_queued"
    for line in data.splitlines():
        if message_id not in line:
            continue
        parsed = safe_json_loads(line)
        if parsed is None:
            continue
        if parsed.get("type") == "queue-operation":
            operation = parsed.get("operation")
            if operation == "enqueue":
                state = "queued"
            elif operation in {"dequeue", "remove"}:
                state = "not_queued"
        elif "queued_command" in line:
            state = "queued"
    return state


def _detect_capture_queue_state(
    pane_id: str,
    message_id: str,
    phrase: str,
    *,
    queue_probe_text: str = "",
) -> str:
    from . import tmux

    if not pane_id:
        return "unknown"
    try:
        capture = tmux.capture_pane(pane_id, lines=200)
    except Exception:
        return "unknown"

    if phrase not in capture:
        return "not_queued"
    if message_id in capture:
        return "queued"
    if queue_probe_text:
        collapsed_capture = " ".join(capture.split())
        collapsed_probe = " ".join(queue_probe_text.split())
        if collapsed_probe and collapsed_probe in collapsed_capture:
            return "queued"
    return "unknown"


def _effective_deadline(record: dict) -> float:
    last_queued_at = record.get("lastQueuedAt")
    if isinstance(last_queued_at, (int, float)) and last_queued_at > 0:
        return last_queued_at + POST_QUEUE_TIMEOUT
    deadline = record.get("deadlineAt", 0)
    return float(deadline) if isinstance(deadline, (int, float)) else 0.0


def _apply_queue_probe(record: dict, probe: dict[str, str]) -> None:
    now = time.time()
    record["lastQueueProbeAt"] = now

    state = probe.get("state", "unknown")
    source = probe.get("source", "none")

    if state == "queued":
        record["runtimeQueueState"] = "queued"
        record["queueSource"] = source
        if not record.get("firstQueuedAt"):
            record["firstQueuedAt"] = now
        record["lastQueuedAt"] = now
        return

    if state == "not_queued":
        record["runtimeQueueState"] = "not_queued"
        if source != "none":
            record["queueSource"] = source
        return

    if "runtimeQueueState" not in record:
        record["runtimeQueueState"] = "unknown"


def _read_sidecar_state(workspace: str) -> dict | None:
    path = Path(workspace) / "state" / "sidecar.json"
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return None


def _write_sidecar_state(workspace: str, pid: int, team: str, tmux_window: str) -> None:
    path = Path(workspace) / "state" / "sidecar.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    state = {
        "pid": pid,
        "team": team,
        "workspace": workspace,
        "tmuxWindow": tmux_window,
        "startedAt": _now_iso(),
        "lastHeartbeat": _now_iso(),
    }
    path.write_text(json.dumps(state, indent=2, ensure_ascii=False) + "\n")


def _update_heartbeat(workspace: str) -> None:
    path = Path(workspace) / "state" / "sidecar.json"
    try:
        state = json.loads(path.read_text())
        state["lastHeartbeat"] = _now_iso()
        path.write_text(json.dumps(state, indent=2, ensure_ascii=False) + "\n")
    except (OSError, json.JSONDecodeError):
        pass


def _clear_sidecar_state(workspace: str) -> None:
    path = Path(workspace) / "state" / "sidecar.json"
    try:
        path.unlink()
    except OSError:
        pass


def _is_pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except (OSError, ProcessLookupError):
        return False


def _is_tmux_window_alive(tmux_window: str) -> bool:
    import subprocess
    try:
        session = tmux_window.split(":")[0] if ":" in tmux_window else tmux_window
        window_idx = tmux_window.split(":")[-1] if ":" in tmux_window else ""
        result = subprocess.run(
            ["tmux", "list-windows", "-t", session, "-F", "#{window_index}"],
            capture_output=True, text=True, timeout=5,
        )
        return window_idx in result.stdout.strip().split("\n")
    except Exception:
        return False


def _is_heartbeat_fresh(state: dict, max_age: float = 60.0) -> bool:
    """Check if sidecar heartbeat is recent enough."""
    from .adapters.base import parse_iso_timestamp
    hb = parse_iso_timestamp(state.get("lastHeartbeat"))
    if hb is None:
        return False
    from datetime import UTC, datetime
    age = (datetime.now(UTC) - hb).total_seconds()
    return age < max_age


def ensure_sidecar(workspace: str, team: str, tmux_window: str) -> int | None:
    """Ensure the team sidecar is running. Returns PID or None.

    Uses a lockfile to prevent concurrent starts.
    """
    lock_path = Path(workspace) / "state" / "sidecar.lock"
    lock_path.parent.mkdir(parents=True, exist_ok=True)

    import fcntl
    try:
        lock_fd = os.open(str(lock_path), os.O_CREAT | os.O_RDWR)
        fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except (OSError, BlockingIOError):
        # Another process is starting a sidecar — just return
        state = _read_sidecar_state(workspace)
        return state.get("pid") if state else None

    try:
        state = _read_sidecar_state(workspace)
        if state is not None:
            pid = state.get("pid", 0)
            if _is_pid_alive(pid) and _is_heartbeat_fresh(state):
                return pid

        # Sidecar not running or stale — start it
        return _start_sidecar(workspace, team, tmux_window)
    finally:
        fcntl.flock(lock_fd, fcntl.LOCK_UN)
        os.close(lock_fd)


def _start_sidecar(workspace: str, team: str, tmux_window: str) -> int:
    """Fork a detached sidecar process. Returns child PID."""
    pid = os.fork()
    if pid == 0:
        try:
            os.setsid()
            devnull = os.open(os.devnull, os.O_RDWR)
            os.dup2(devnull, 0)
            os.dup2(devnull, 1)
            os.dup2(devnull, 2)
            os.close(devnull)
            signal.signal(signal.SIGINT, signal.SIG_IGN)

            _sidecar_loop(workspace, team, tmux_window)
        except Exception:
            pass
        finally:
            _clear_sidecar_state(workspace)
            os._exit(0)

    _write_sidecar_state(workspace, pid, team, tmux_window)
    return pid


def _sidecar_loop(workspace: str, team: str, tmux_window: str) -> None:
    """Main sidecar loop. Runs until team/workspace is gone."""
    _write_sidecar_state(workspace, os.getpid(), team, tmux_window)
    last_window_check = 0.0

    while True:
        # Check if workspace still exists
        if not Path(workspace).is_dir():
            return

        # Check if tmux window still exists (every ~30s)
        now = time.monotonic()
        if now - last_window_check >= 30.0:
            last_window_check = now
            if not _is_tmux_window_alive(tmux_window):
                return

        # Process pending sends
        pending_dir = Path(workspace) / "state" / "pending"
        has_work = False

        if pending_dir.is_dir():
            for record_path in list(pending_dir.glob("*.json")):
                try:
                    record = json.loads(record_path.read_text())
                except (OSError, json.JSONDecodeError):
                    record_path.unlink(missing_ok=True)
                    continue

                has_work = True
                result = _check_pending(record)

                if result is not None:
                    # Write observation event
                    msg_id = record["msgId"]
                    _write_observation(workspace, msg_id, result)

                    # Notify sender on exception
                    if result in ("unconfirmed", "tracking_lost"):
                        sender_pane = record.get("senderPane", "")
                        target_agent = record.get("targetAgent", "")
                        if sender_pane:
                            _inject_exception(sender_pane, msg_id, target_agent, result)

                    # Remove pending record
                    record_path.unlink(missing_ok=True)
                else:
                    record_path.write_text(json.dumps(record, indent=2, ensure_ascii=False) + "\n")

        _update_heartbeat(workspace)
        time.sleep(ACTIVE_SLEEP if has_work else IDLE_SLEEP)


def _check_pending(record: dict) -> str | None:
    """Check a single pending record. Returns result or None if still pending."""
    from .adapters.base import transcript_has_id_in_new_user_turn

    transcript_path = Path(record.get("targetTranscript", ""))
    message_id = record.get("msgId", "")
    baseline = record.get("baseline", 0)
    deadline = _effective_deadline(record)
    now = time.time()

    if not transcript_path.exists():
        probe = detect_runtime_queue_state(
            pane_id=record.get("targetPane", ""),
            message_id=message_id,
            queue_probe_text=record.get("queueProbeText", ""),
            transcript_path=str(transcript_path),
            baseline=baseline,
            cli_name=record.get("targetCli", ""),
        )
        _apply_queue_probe(record, probe)
        if probe.get("state") == "queued":
            return None
        if now > deadline:
            return "tracking_lost"
        return None

    if transcript_has_id_in_new_user_turn(transcript_path, message_id, baseline):
        return "confirmed"

    probe = detect_runtime_queue_state(
        pane_id=record.get("targetPane", ""),
        message_id=message_id,
        queue_probe_text=record.get("queueProbeText", ""),
        transcript_path=str(transcript_path),
        baseline=baseline,
        cli_name=record.get("targetCli", ""),
    )
    _apply_queue_probe(record, probe)
    if probe.get("state") == "queued":
        return None

    if now > _effective_deadline(record):
        return "unconfirmed"
    return None


def stop_sidecar(workspace: str) -> None:
    """Best-effort stop the sidecar for a workspace."""
    state = _read_sidecar_state(workspace)
    if state is None:
        return
    pid = state.get("pid", 0)
    if pid and _is_pid_alive(pid):
        try:
            os.kill(pid, signal.SIGTERM)
        except OSError:
            pass
    _clear_sidecar_state(workspace)


def check_stale_sidecar(workspace: str, message_id: str) -> str | None:
    """Check if sidecar is alive and tracking this message.

    Returns observation result if available, "tracking_lost" if sidecar
    is dead with no result, or None if still being tracked.
    """
    from .observer import find_observation

    obs = find_observation(workspace, message_id)
    if obs is not None:
        return obs["metadata"]["result"]

    # Check if pending record still exists
    pending_path = Path(workspace) / "state" / "pending" / f"{message_id}.json"
    if not pending_path.exists():
        # No pending record and no observation — lost
        _write_observation(workspace, message_id, "tracking_lost")
        return "tracking_lost"

    # Pending record exists — check if sidecar is alive and healthy
    state = _read_sidecar_state(workspace)
    if state is None or not _is_pid_alive(state.get("pid", 0)) or not _is_heartbeat_fresh(state):
        # Sidecar dead or stale — write tracking_lost
        _write_observation(workspace, message_id, "tracking_lost")
        pending_path.unlink(missing_ok=True)
        return "tracking_lost"

    return None  # Still being tracked
