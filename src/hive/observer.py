"""Async observation of message delivery via transcript polling.

The observer is a one-shot background process forked by `hive send`.
It polls the target agent's transcript for the message ID, then writes
an observation event back to the workspace.
"""

from __future__ import annotations

import json
import os
import signal
import sys
import time
from pathlib import Path


def _write_observation(workspace: str, message_id: str, result: str) -> None:
    from datetime import UTC, datetime

    ts = datetime.now(UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z")
    path = Path(workspace) / "events" / f"{time.time_ns()}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "from": "_system",
        "to": "",
        "intent": "observation",
        "metadata": {
            "messageId": message_id,
            "result": result,
            "observedAt": ts,
            "observerPid": os.getpid(),
        },
        "createdAt": ts,
    }
    path.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n")


def run_observer(
    workspace: str,
    transcript_path: str,
    message_id: str,
    baseline: int,
    timeout: float = 45.0,
) -> None:
    """Poll transcript for message_id and write observation event.

    This function is meant to run in a forked child process.
    """
    # Ignore SIGINT so Ctrl-C in parent doesn't kill observer
    signal.signal(signal.SIGINT, signal.SIG_IGN)

    from .adapters.base import wait_for_id_in_transcript

    tp = Path(transcript_path)
    confirmed = wait_for_id_in_transcript(tp, message_id, baseline, timeout)
    result = "confirmed" if confirmed else "unconfirmed"
    _write_observation(workspace, message_id, result)


def fork_observer(
    workspace: str,
    transcript_path: str,
    message_id: str,
    baseline: int,
    timeout: float = 45.0,
) -> int:
    """Fork a background observer process. Returns child PID."""
    pid = os.fork()
    if pid == 0:
        # Child process — detach from parent's process group
        try:
            os.setsid()
            # Close stdin/stdout/stderr to fully detach
            devnull = os.open(os.devnull, os.O_RDWR)
            os.dup2(devnull, 0)
            os.dup2(devnull, 1)
            os.dup2(devnull, 2)
            os.close(devnull)

            run_observer(workspace, transcript_path, message_id, baseline, timeout)
        except Exception:
            pass
        finally:
            os._exit(0)
    return pid


def write_observer_lost(workspace: str, message_id: str) -> None:
    """Write an observer_lost observation event."""
    _write_observation(workspace, message_id, "observer_lost")


def find_observation(workspace: str, message_id: str) -> dict | None:
    """Find the observation event for a given message ID."""
    events_dir = Path(workspace) / "events"
    if not events_dir.is_dir():
        return None
    result = None
    for path in sorted(events_dir.glob("*.json")):
        try:
            data = json.loads(path.read_text())
        except (json.JSONDecodeError, OSError):
            continue
        if (
            data.get("intent") == "observation"
            and isinstance(data.get("metadata"), dict)
            and data["metadata"].get("messageId") == message_id
        ):
            result = data  # Keep scanning — return the latest observation
    return result


def check_stale_observer(workspace: str, message_id: str, observer_pid: int) -> str | None:
    """Check if observer is stale and write observer_lost if so.

    Returns the observation result if already available, or writes
    observer_lost and returns "observer_lost" if the observer process
    is gone with no result. Returns None if observer is still running.
    """
    obs = find_observation(workspace, message_id)
    if obs is not None:
        return obs["metadata"]["result"]

    # Check if observer process is still alive
    try:
        os.kill(observer_pid, 0)
        return None  # Still running
    except (OSError, ProcessLookupError):
        # Process gone, no observation written
        write_observer_lost(workspace, message_id)
        return "observer_lost"
