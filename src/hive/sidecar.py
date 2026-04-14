"""Team-scoped sidecar for pending send lifecycle tracking.

The sidecar owns runtime pending-send state in memory and exposes a tiny
workspace-local Unix socket for enqueue/status/shutdown. Durable facts still
land in the workspace database as observation events.
"""

from __future__ import annotations

import json
import os
import signal
import socket
import time
from pathlib import Path
from typing import Any

from . import bus
from .agent_cli import detect_profile_for_pane

IDLE_SLEEP = 5.0
ACTIVE_SLEEP = 0.5
OBSERVATION_TIMEOUT = 60.0
POST_QUEUE_TIMEOUT = 10.0
SOCKET_READY_TIMEOUT = 2.0
SOCKET_RETRY_INTERVAL = 0.1


def _now_iso() -> str:
    from datetime import UTC, datetime

    return datetime.now(UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def _run_dir(workspace: str) -> Path:
    return Path(workspace) / "run"


def _socket_path(workspace: str) -> Path:
    return _run_dir(workspace) / "sidecar.sock"


def _lock_path(workspace: str) -> Path:
    return _run_dir(workspace) / "sidecar.lock"


def _write_observation(workspace: str, message_id: str, result: str) -> None:
    ts = _now_iso()
    bus.write_event(
        workspace,
        from_agent="_system",
        to_agent="",
        intent="observation",
        message_id=message_id,
        metadata={
            "msgId": message_id,
            "result": result,
            "observedAt": ts,
        },
    )


def _inject_exception(pane_id: str, message_id: str, target_agent: str, result: str) -> None:
    """Inject a HIVE-SYSTEM exception block into the sender's pane."""
    from . import tmux

    if result == "unconfirmed":
        body = (
            f"Message {message_id} to {target_agent} was not confirmed within "
            f"{int(OBSERVATION_TIMEOUT)}s. Do not assume delivery."
        )
    else:
        body = f"Message {message_id} to {target_agent}: delivery tracking lost."

    block = (
        f"<HIVE-SYSTEM type=delivery-exception msgId={message_id} "
        f"result={result} to={target_agent}>\n{body}\n</HIVE-SYSTEM>"
    )
    try:
        tmux.send_keys(pane_id, block, enter=True)
    except Exception:
        pass


def enqueue_pending(
    workspace: str,
    message_id: str,
    sender_agent: str,
    sender_pane: str,
    target_agent: str,
    transcript_path: str,
    baseline: int,
    *,
    target_pane: str = "",
    target_cli: str = "",
    runtime_queue_state: str = "unknown",
    queue_source: str = "none",
    queue_probe_text: str = "",
) -> bool:
    """Queue a pending send for sidecar tracking."""
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

    for _ in range(int(SOCKET_READY_TIMEOUT / SOCKET_RETRY_INTERVAL)):
        response = _request_sidecar(
            workspace,
            {"action": "enqueue", "record": record},
            timeout=SOCKET_RETRY_INTERVAL,
        )
        if response and response.get("ok") is True:
            return True
        time.sleep(SOCKET_RETRY_INTERVAL)
    return False


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


def _effective_deadline(record: dict[str, Any]) -> float:
    last_queued_at = record.get("lastQueuedAt")
    if isinstance(last_queued_at, (int, float)) and last_queued_at > 0:
        return last_queued_at + POST_QUEUE_TIMEOUT
    deadline = record.get("deadlineAt", 0)
    return float(deadline) if isinstance(deadline, (int, float)) else 0.0


def _apply_queue_probe(record: dict[str, Any], probe: dict[str, str]) -> None:
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


def _socket_alive(workspace: str) -> bool:
    response = _request_sidecar(workspace, {"action": "ping"}, timeout=SOCKET_RETRY_INTERVAL)
    return bool(response and response.get("ok") is True)


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


def _is_tmux_window_alive(tmux_window: str) -> bool:
    import subprocess

    try:
        session = tmux_window.split(":")[0] if ":" in tmux_window else tmux_window
        window_idx = tmux_window.split(":")[-1] if ":" in tmux_window else ""
        result = subprocess.run(
            ["tmux", "list-windows", "-t", session, "-F", "#{window_index}"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        return window_idx in result.stdout.strip().split("\n")
    except Exception:
        return False


def ensure_sidecar(workspace: str, team: str, tmux_window: str) -> int | None:
    """Ensure the team sidecar socket is alive."""
    lock_path = _lock_path(workspace)
    lock_path.parent.mkdir(parents=True, exist_ok=True)

    import fcntl

    lock_fd = os.open(str(lock_path), os.O_CREAT | os.O_RDWR)
    try:
        fcntl.flock(lock_fd, fcntl.LOCK_EX)
        if _socket_alive(workspace):
            return None
        _cleanup_socket(workspace)
        pid = _start_sidecar(workspace, team, tmux_window)
        deadline = time.monotonic() + SOCKET_READY_TIMEOUT
        while time.monotonic() < deadline:
            if _socket_alive(workspace):
                return pid
            time.sleep(SOCKET_RETRY_INTERVAL)
        return pid
    finally:
        fcntl.flock(lock_fd, fcntl.LOCK_UN)
        os.close(lock_fd)


def _start_sidecar(workspace: str, team: str, tmux_window: str) -> int:
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
            _cleanup_socket(workspace)
            os._exit(0)
    return pid


def _open_server_socket(workspace: str) -> socket.socket:
    _run_dir(workspace).mkdir(parents=True, exist_ok=True)
    path = _socket_path(workspace)
    _cleanup_socket(workspace)
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(path))
    server.listen()
    return server


def _live_state(record: dict[str, Any]) -> str:
    return "queued" if record.get("runtimeQueueState") == "queued" else "pending"


def _handle_request(
    *,
    workspace: str,
    pending: dict[str, dict[str, Any]],
    request: dict[str, Any],
) -> tuple[dict[str, Any], bool]:
    action = request.get("action")
    if action == "ping":
        return {"ok": True}, True
    if action == "enqueue":
        record = request.get("record")
        if not isinstance(record, dict) or not record.get("msgId"):
            return {"ok": False, "error": "invalid record"}, True
        pending[str(record["msgId"])] = record
        return {"ok": True, "state": _live_state(record)}, True
    if action == "status":
        message_id = request.get("msgId", "")
        if message_id in pending:
            record = pending[message_id]
            return {
                "ok": True,
                "tracked": True,
                "state": _live_state(record),
                "runtimeQueueState": record.get("runtimeQueueState", "unknown"),
                "queueSource": record.get("queueSource", "none"),
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
                pending=pending,
                request=request if isinstance(request, dict) else {},
            )
            try:
                conn.sendall((json.dumps(response, ensure_ascii=False) + "\n").encode())
            except OSError:
                pass
        if not keep_running:
            return False


def _sidecar_loop(workspace: str, team: str, tmux_window: str) -> None:
    pending: dict[str, dict[str, Any]] = {}
    last_window_check = 0.0
    server = _open_server_socket(workspace)

    try:
        while True:
            if not Path(workspace).is_dir():
                return

            now = time.monotonic()
            if now - last_window_check >= 30.0:
                last_window_check = now
                if not _is_tmux_window_alive(tmux_window):
                    return

            if not _serve_requests(
                server=server,
                workspace=workspace,
                pending=pending,
                timeout=ACTIVE_SLEEP if pending else IDLE_SLEEP,
            ):
                return

            for message_id, record in list(pending.items()):
                result = _check_pending(record)
                if result is None:
                    continue
                _write_observation(workspace, message_id, result)
                if result in ("unconfirmed", "tracking_lost"):
                    sender_pane = record.get("senderPane", "")
                    target_agent = record.get("targetAgent", "")
                    if sender_pane:
                        _inject_exception(sender_pane, message_id, target_agent, result)
                pending.pop(message_id, None)
    finally:
        try:
            server.close()
        except OSError:
            pass
        _cleanup_socket(workspace)


def _check_pending(record: dict[str, Any]) -> str | None:
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
    _request_sidecar(workspace, {"action": "shutdown"}, timeout=SOCKET_READY_TIMEOUT)
    deadline = time.monotonic() + SOCKET_READY_TIMEOUT
    while time.monotonic() < deadline:
        if not _socket_path(workspace).exists():
            return
        time.sleep(SOCKET_RETRY_INTERVAL)
    _cleanup_socket(workspace)


def check_stale_sidecar(workspace: str, message_id: str) -> str | None:
    """Check whether a message is still actively tracked by the sidecar."""
    from .observer import find_observation

    obs = find_observation(workspace, message_id)
    if obs is not None:
        metadata = obs.get("metadata", {})
        if isinstance(metadata, dict):
            return str(metadata.get("result", ""))
        return None

    response = _request_sidecar(workspace, {"action": "status", "msgId": message_id}, timeout=SOCKET_RETRY_INTERVAL)
    if response and response.get("tracked") is True:
        return None

    _write_observation(workspace, message_id, "tracking_lost")
    return "tracking_lost"
