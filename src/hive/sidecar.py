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
from collections import defaultdict
from pathlib import Path
from typing import Any

from . import bus
from .agent_cli import detect_profile_for_pane
from .runtime_state import (
    build_queue_probe_text,
    delivery_exception_body,
    delivery_guidance,
    format_hive_envelope,
    gate_guidance,
    present_delivery_state,
    present_send_state,
    project_thread_event,
    send_guidance,
)

IDLE_SLEEP = 5.0
ACTIVE_SLEEP = 0.5
OBSERVATION_TIMEOUT = 60.0
POST_QUEUE_TIMEOUT = 10.0
SOCKET_READY_TIMEOUT = 2.0
SOCKET_RETRY_INTERVAL = 0.1
SEND_GRACE_TIMEOUT = 3.0
SEND_REQUEST_TIMEOUT = SEND_GRACE_TIMEOUT + 2.0
SIDECAR_API_VERSION = 5


def _now_iso() -> str:
    from datetime import UTC, datetime

    return datetime.now(UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def _run_dir(workspace: str) -> Path:
    return Path(workspace) / "run"


def _socket_path(workspace: str) -> Path:
    return _run_dir(workspace) / "sidecar.sock"


def _lock_path(workspace: str) -> Path:
    return _run_dir(workspace) / "sidecar.lock"


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
    response = request_ping(workspace)
    return bool(
        response
        and response.get("ok") is True
        and response.get("apiVersion") == SIDECAR_API_VERSION
    )


def request_ping(workspace: str) -> dict[str, Any] | None:
    return _request_sidecar(workspace, {"action": "ping"}, timeout=SOCKET_RETRY_INTERVAL)


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


def request_answer(
    workspace: str,
    *,
    team: str,
    sender_agent: str,
    target_agent: str,
    text: str,
) -> dict[str, Any] | None:
    return _request_sidecar(
        workspace,
        {
            "action": "answer",
            "team": team,
            "senderAgent": sender_agent,
            "targetAgent": target_agent,
            "text": text,
        },
        timeout=15.0,
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


def request_suggest(
    workspace: str,
    *,
    team: str,
    source_agent: str,
) -> dict[str, Any] | None:
    return _request_sidecar(
        workspace,
        {"action": "suggest", "team": team, "sourceAgent": source_agent},
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
    session_id = adapter.resolve_current_session_id(target.pane_id)
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
            "target agent is waiting for a user answer; use `hive answer` or answer in the target pane directly"
        )
    return result.status


def _observe_send_grace(
    *,
    pane_id: str,
    transcript_path: Path | None,
    message_id: str,
    baseline: int,
    queue_probe_text: str,
    cli_name: str,
) -> tuple[str, dict[str, str]]:
    from .adapters.base import transcript_has_id_in_new_user_turn

    deadline = time.monotonic() + SEND_GRACE_TIMEOUT
    last_probe: dict[str, str] = {"state": "unknown", "source": "none", "observedAt": ""}

    while True:
        if transcript_path is not None and transcript_has_id_in_new_user_turn(transcript_path, message_id, baseline):
            return "confirmed", last_probe

        last_probe = detect_runtime_queue_state(
            pane_id=pane_id,
            message_id=message_id,
            queue_probe_text=queue_probe_text,
            transcript_path=str(transcript_path) if transcript_path is not None else "",
            baseline=baseline,
            cli_name=cli_name,
        )
        if last_probe.get("state") == "queued":
            return "queued", last_probe

        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return "pending", last_probe
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
    runtime_queue_state: str,
    queue_source: str,
    queue_probe_text: str,
) -> dict[str, Any]:
    record: dict[str, Any] = {
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
    return record


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

    message_id = ""
    transcript_path: Path | None = None
    baseline = 0
    try:
        transcript_path, baseline = _resolve_ack_baseline(target)
    except Exception:
        transcript_path = None

    gate_status = _check_send_gate(transcript_path)

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

    runtime_queue_state = "unknown"
    probe: dict[str, str] = {"source": "none"}
    turn_observed = "pending"
    profile = detect_profile_for_pane(target.pane_id)
    queue_probe_text = build_queue_probe_text(normalized_body)

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
        grace_state, probe = _observe_send_grace(
            pane_id=target.pane_id,
            transcript_path=transcript_path,
            message_id=message_id,
            baseline=baseline,
            queue_probe_text=queue_probe_text,
            cli_name=profile.name if profile else "",
        )
        if grace_state == "confirmed":
            turn_observed = "confirmed"
            _write_observation(
                workspace,
                message_id,
                "confirmed",
                metadata={
                    "injectStatus": "submitted",
                    "turnObserved": "confirmed",
                },
            )
        elif grace_state == "queued":
            runtime_queue_state = "queued"
            pending[message_id] = _pending_record(
                message_id=message_id,
                sender_agent=sender_agent,
                sender_pane=sender_pane,
                target_agent=target_agent,
                target_pane=target.pane_id,
                target_cli=profile.name if profile else "",
                transcript_path=str(transcript_path) if transcript_path is not None else "",
                baseline=baseline,
                runtime_queue_state=runtime_queue_state,
                queue_source=probe.get("source", "none"),
                queue_probe_text=queue_probe_text,
            )
            turn_observed = "pending"
        elif transcript_path is not None:
            from .adapters.base import wait_for_id_in_transcript

            if wait_for_id_in_transcript(transcript_path, message_id, baseline):
                turn_observed = "confirmed"
                _write_observation(
                    workspace,
                    message_id,
                    "confirmed",
                    metadata={
                        "injectStatus": "submitted",
                        "turnObserved": "confirmed",
                    },
                )
            else:
                turn_observed = "unconfirmed"
                _write_observation(
                    workspace,
                    message_id,
                    "unconfirmed",
                    metadata={
                        "injectStatus": "submitted",
                        "turnObserved": "unconfirmed",
                    },
                )
        else:
            pending[message_id] = _pending_record(
                message_id=message_id,
                sender_agent=sender_agent,
                sender_pane=sender_pane,
                target_agent=target_agent,
                target_pane=target.pane_id,
                target_cli=profile.name if profile else "",
                transcript_path="",
                baseline=baseline,
                runtime_queue_state="unknown",
                queue_source=probe.get("source", "none"),
                queue_probe_text=queue_probe_text,
            )
            turn_observed = "pending"
    else:
        grace_state, probe = _observe_send_grace(
            pane_id=target.pane_id,
            transcript_path=transcript_path,
            message_id=message_id,
            baseline=baseline,
            queue_probe_text=queue_probe_text,
            cli_name=profile.name if profile else "",
        )
        if grace_state == "confirmed":
            turn_observed = "confirmed"
            _write_observation(
                workspace,
                message_id,
                "confirmed",
                metadata={
                    "injectStatus": "submitted",
                    "turnObserved": "confirmed",
                },
            )
        else:
            runtime_queue_state = "queued" if grace_state == "queued" else "unknown"
            pending[message_id] = _pending_record(
                message_id=message_id,
                sender_agent=sender_agent,
                sender_pane=sender_pane,
                target_agent=target_agent,
                target_pane=target.pane_id,
                target_cli=profile.name if profile else "",
                transcript_path=str(transcript_path) if transcript_path is not None else "",
                baseline=baseline,
                runtime_queue_state=runtime_queue_state,
                queue_source=probe.get("source", "none"),
                queue_probe_text=queue_probe_text,
            )
            turn_observed = "pending"

    payload = {
        "ok": True,
        "from": sender_agent,
        "to": target_agent,
        "msgId": message_id,
        "artifact": artifact,
        "gate": gate_status,
        "state": present_send_state(
            inject_status=inject_status,
            turn_observed=turn_observed,
            runtime_queue_state=runtime_queue_state,
        ),
    }
    guidance = send_guidance(str(payload["state"]))
    if guidance is not None:
        payload.update(guidance)
    gate_info = gate_guidance(gate_status)
    if gate_info is not None:
        payload.update(gate_info)
    return payload


def _answer_payload(
    *,
    workspace: str,
    team_name: str,
    sender_agent: str,
    target_agent: str,
    text: str,
) -> dict[str, Any]:
    _, target = _resolve_live_agent(team_name, target_agent)
    transcript_path, _ = _resolve_ack_baseline(target)

    from .adapters.base import check_input_gate, extract_pending_question
    gate = check_input_gate(transcript_path)
    if gate.status != "waiting":
        raise RuntimeError(f"agent '{target_agent}' is not waiting for an answer (inputState: {gate.status})")

    pending_question = extract_pending_question(transcript_path)
    bus.write_event(
        workspace,
        from_agent=sender_agent,
        to_agent=target_agent,
        intent="answer",
        body=text.strip(),
    )

    from .agent import _submit_interactive_text
    _submit_interactive_text(target.pane_id, text, target.cli)

    ack_status = "unconfirmed"
    deadline = time.monotonic() + 15.0
    while time.monotonic() < deadline:
        time.sleep(0.5)
        result = check_input_gate(transcript_path)
        if result.status == "clear":
            ack_status = "confirmed"
            break

    payload: dict[str, Any] = {
        "ok": True,
        "from": sender_agent,
        "to": target_agent,
        "ack": ack_status,
    }
    if pending_question:
        payload["question"] = pending_question
    if text.strip():
        payload["answer"] = text.strip()
    return payload


def _delivery_payload(workspace: str, pending: dict[str, dict[str, Any]], message_id: str) -> dict[str, Any]:
    send_event = bus.find_send_event(workspace, message_id)
    if send_event is None:
        return {"ok": False, "error": f"no send event found with msgId '{message_id}'"}

    obs = bus.find_latest_observation(workspace, message_id)
    if obs is None and message_id not in pending:
        _write_observation(
            workspace,
            message_id,
            "tracking_lost",
            metadata={
                "injectStatus": "submitted",
                "turnObserved": "pending",
            },
        )
        obs = bus.find_latest_observation(workspace, message_id)

    inject_status = "submitted"
    turn_observed = "pending"
    runtime_queue_state = "unknown"
    queue_source = ""

    if message_id in pending:
        record = pending[message_id]
        runtime_queue_state = str(record.get("runtimeQueueState", "unknown"))
        queue_source = str(record.get("queueSource", "none"))
        payload: dict[str, Any] = {
            "ok": True,
            "msgId": message_id,
            "to": send_event.get("to", ""),
            "state": present_delivery_state(
                inject_status=inject_status,
                turn_observed="pending",
                runtime_queue_state=runtime_queue_state,
            ),
            "injectStatus": inject_status,
            "turnObserved": "pending",
        }
        if runtime_queue_state != "unknown":
            payload["runtimeQueueState"] = runtime_queue_state
        if queue_source:
            payload["queueSource"] = queue_source
        guidance = delivery_guidance(str(payload["state"]))
        if guidance is not None:
            payload.update(guidance)
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
        if not turn_observed:
            if result in {"confirmed", "unconfirmed"}:
                turn_observed = str(result)
            elif result == "failed":
                turn_observed = "unavailable"
            else:
                turn_observed = "pending"
        runtime_queue_state = (
            str(metadata.get("runtimeQueueState", "unknown"))
            if isinstance(metadata, dict)
            else "unknown"
        )
        queue_source = (
            str(metadata.get("queueSource", ""))
            if isinstance(metadata, dict)
            else ""
        )
        payload = {
            "ok": True,
            "msgId": message_id,
            "to": send_event.get("to", ""),
            "state": present_delivery_state(
                inject_status=inject_status,
                turn_observed=turn_observed,
                runtime_queue_state=runtime_queue_state,
                observation_result=str(result),
            ),
            "injectStatus": inject_status,
            "turnObserved": turn_observed,
        }
        if runtime_queue_state != "unknown":
            payload["runtimeQueueState"] = runtime_queue_state
        if queue_source:
            payload["queueSource"] = queue_source
        if observed_at:
            payload["observedAt"] = observed_at
        guidance = delivery_guidance(str(payload["state"]))
        if guidance is not None:
            payload.update(guidance)
        return payload

    payload = {
        "ok": True,
        "msgId": message_id,
        "to": send_event.get("to", ""),
        "state": present_delivery_state(
            inject_status=inject_status,
            turn_observed=turn_observed,
            runtime_queue_state=runtime_queue_state,
        ),
        "injectStatus": inject_status,
        "turnObserved": turn_observed,
    }
    if runtime_queue_state != "unknown":
        payload["runtimeQueueState"] = runtime_queue_state
    if queue_source:
        payload["queueSource"] = queue_source
    guidance = delivery_guidance(str(payload["state"]))
    if guidance is not None:
        payload.update(guidance)
    return payload


def _doctor_payload(workspace: str, team_name: str, target_agent: str, *, verbose: bool = False) -> dict[str, Any]:
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
    runtime = _member_runtime_payload(target.pane_id, role="agent")
    diag["alive"] = bool(runtime.get("alive", alive))
    if runtime.get("model"):
        diag["model"] = runtime["model"]
    if runtime.get("sessionId"):
        diag["sessionId"] = runtime["sessionId"]
    if runtime.get("inputState"):
        diag["inputState"] = runtime["inputState"]
    if "_gate" in runtime:
        diag["gate"] = runtime["_gate"]
    if verbose:
        diag["pane"] = target.pane_id
        diag["teamMembers"] = len(list(team.agents.values()))
        if runtime.get("_cli"):
            diag["cli"] = runtime["_cli"]
        if "inputReason" in runtime:
            diag["inputReason"] = runtime["inputReason"]
        if "pendingQuestion" in runtime:
            diag["pendingQuestion"] = runtime["pendingQuestion"]
        if "_transcript" in runtime:
            diag["transcript"] = runtime["_transcript"]
        if "_transcriptExists" in runtime:
            diag["transcriptExists"] = runtime["_transcriptExists"]
        if "_transcriptSize" in runtime:
            diag["transcriptSize"] = runtime["_transcriptSize"]
        if "_gateReason" in runtime:
            diag["gateReason"] = runtime["_gateReason"]
        diag["workspace"] = str(workspace)
        diag["eventCount"] = bus.count_events(workspace)
    return diag


def _agent_runtime_payload(pane_id: str) -> dict[str, Any]:
    from . import adapters, tmux
    from .adapters.base import check_input_gate, extract_pending_question
    from .agent_cli import resolve_model_for_pane

    runtime: dict[str, Any] = {
        "alive": tmux.is_pane_alive(pane_id),
    }
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

    session_id = adapter.resolve_current_session_id(pane_id)
    runtime["sessionId"] = session_id or "unresolved"
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
        question = extract_pending_question(transcript)
        if question:
            runtime["pendingQuestion"] = question
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
        return {"alive": tmux.is_pane_alive(pane_id)}
    return _agent_runtime_payload(pane_id)


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

    for name in sorted(team.terminals):
        terminal = team.terminals[name]
        members[name] = _member_runtime_payload(terminal.pane_id, role="terminal")

    payload: dict[str, Any] = {
        "ok": True,
        "team": team_name,
        "members": members,
    }
    if needs_answer:
        payload["needsAnswer"] = needs_answer
    return payload


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


def _candidate_score(
    *,
    source_model: str,
    source_cli: str,
    candidate_model: str,
    candidate_cli: str,
    input_state: str,
) -> tuple[int, list[str]]:
    score = 0
    reasons: list[str] = []

    if input_state == "ready":
        score += 100
        reasons.append("ready")
    elif input_state:
        reasons.append(f"inputState={input_state}")

    if source_model and candidate_model:
        if source_model != candidate_model:
            score += 30
            reasons.append("different_model")
        else:
            reasons.append("same_model_fallback")

    if source_cli and candidate_cli:
        if source_cli != candidate_cli:
            score += 10
            reasons.append("different_cli")
        else:
            reasons.append("same_cli_fallback")

    if not reasons:
        reasons.append("alive")
    return score, reasons


def _suggest_payload(team_name: str, source_agent: str) -> dict[str, Any]:
    bindings = _team_member_bindings(team_name)
    runtime_payload = _team_runtime_payload(team_name)
    runtime_members = runtime_payload.get("members")
    if not isinstance(runtime_members, dict):
        runtime_members = {}

    if source_agent not in bindings and source_agent not in runtime_members:
        return {"ok": False, "error": f"agent '{source_agent}' is not registered in team '{team_name}'"}

    source_binding = bindings.get(source_agent, {})
    source_runtime = runtime_members.get(source_agent, {})
    if not isinstance(source_runtime, dict):
        source_runtime = {}
    source_cli = str(source_runtime.get("_cli") or source_binding.get("cli") or "")
    source_model = str(source_runtime.get("model") or "")

    candidates: list[dict[str, Any]] = []
    for name, binding in bindings.items():
        if name == source_agent:
            continue
        runtime = runtime_members.get(name, {})
        if not isinstance(runtime, dict):
            runtime = {}
        if not bool(runtime.get("alive", False)):
            continue
        candidate_cli = str(runtime.get("_cli") or binding.get("cli") or "")
        candidate_model = str(runtime.get("model") or "")
        input_state = str(runtime.get("inputState") or "unknown")
        score, reasons = _candidate_score(
            source_model=source_model,
            source_cli=source_cli,
            candidate_model=candidate_model,
            candidate_cli=candidate_cli,
            input_state=input_state,
        )
        candidate: dict[str, Any] = {
            "name": name,
            "role": binding.get("role", ""),
            "pane": binding.get("pane", ""),
            "alive": True,
            "inputState": input_state,
            "score": score,
            "reasons": reasons,
        }
        if candidate_cli:
            candidate["cli"] = candidate_cli
        if candidate_model:
            candidate["model"] = candidate_model
        if runtime.get("sessionId"):
            candidate["sessionId"] = runtime["sessionId"]
        candidates.append(candidate)

    candidates.sort(key=lambda item: (-int(item.get("score", 0)), str(item.get("name", ""))))

    source: dict[str, Any] = {"name": source_agent}
    if source_cli:
        source["cli"] = source_cli
    if source_model:
        source["model"] = source_model
    if source_runtime.get("inputState"):
        source["inputState"] = source_runtime["inputState"]

    return {
        "ok": True,
        "team": team_name,
        "source": source,
        "candidates": candidates,
    }


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
            delivery: dict[str, Any] = {
                "state": present_delivery_state(
                    inject_status="submitted",
                    turn_observed="pending",
                    runtime_queue_state=str(record.get("runtimeQueueState", "unknown")),
                )
            }
            if record.get("queueSource"):
                delivery["queueSource"] = record["queueSource"]
            item["delivery"] = delivery
        elif thread_msg_id in latest_observations:
            _, observation = latest_observations[thread_msg_id]
            metadata = observation.get("metadata", {})
            if isinstance(metadata, dict):
                delivery = {
                    "state": str(metadata.get("result") or "pending"),
                }
                if metadata.get("observedAt"):
                    delivery["observedAt"] = metadata["observedAt"]
                guidance = delivery_guidance(delivery["state"])
                if guidance is not None:
                    delivery.update(guidance)
                item["delivery"] = delivery

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
            _sidecar_loop(workspace, team, tmux_window, tmux_window_id)
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
    team: str,
    tmux_window: str,
    tmux_window_id: str,
    pending: dict[str, dict[str, Any]],
    request: dict[str, Any],
) -> tuple[dict[str, Any], bool]:
    action = request.get("action")
    if action == "ping":
        return {
            "ok": True,
            "apiVersion": SIDECAR_API_VERSION,
            "team": team,
            "tmuxWindow": tmux_window,
            "tmuxWindowId": tmux_window_id,
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
    if action == "answer":
        try:
            response = _answer_payload(
                workspace=workspace,
                team_name=str(request.get("team") or team),
                sender_agent=str(request.get("senderAgent", "")),
                target_agent=str(request.get("targetAgent", "")),
                text=str(request.get("text", "")),
            )
        except Exception as exc:
            response = {"ok": False, "error": str(exc)}
        return response, True
    if action == "enqueue":
        record = request.get("record")
        if not isinstance(record, dict) or not record.get("msgId"):
            return {"ok": False, "error": "invalid record"}, True
        pending[str(record["msgId"])] = record
        return {"ok": True, "state": _live_state(record)}, True
    if action == "delivery":
        return _delivery_payload(workspace, pending, str(request.get("msgId", ""))), True
    if action == "doctor":
        try:
            response = _doctor_payload(
                workspace,
                str(request.get("team") or team),
                str(request.get("agent", "")),
                verbose=bool(request.get("verbose", False)),
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
    if action == "suggest":
        try:
            response = _suggest_payload(
                str(request.get("team") or team),
                str(request.get("sourceAgent", "")),
            )
        except Exception as exc:
            response = {"ok": False, "error": str(exc)}
        return response, True
    if action == "thread":
        try:
            response = _thread_payload(workspace, pending, str(request.get("msgId", "")))
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
    team: str,
    tmux_window: str,
    tmux_window_id: str,
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
                pending=pending,
                request=request if isinstance(request, dict) else {},
            )
            try:
                conn.sendall((json.dumps(response, ensure_ascii=False) + "\n").encode())
            except OSError:
                pass
        if not keep_running:
            return False


def _sidecar_loop(workspace: str, team: str, tmux_window: str, tmux_window_id: str) -> None:
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
                if not _is_tmux_window_alive(tmux_window_id):
                    return

            if not _serve_requests(
                server=server,
                workspace=workspace,
                team=team,
                tmux_window=tmux_window,
                tmux_window_id=tmux_window_id,
                pending=pending,
                timeout=ACTIVE_SLEEP if pending else IDLE_SLEEP,
            ):
                return

            for message_id, record in list(pending.items()):
                result = _check_pending(record)
                if result is None:
                    continue
                _write_observation(
                    workspace,
                    message_id,
                    result,
                    metadata=_observation_metadata_for_pending(record, result),
                )
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


def _observation_metadata_for_pending(record: dict[str, Any], result: str) -> dict[str, str]:
    metadata: dict[str, str] = {
        "injectStatus": "submitted",
    }
    if result == "confirmed":
        metadata["turnObserved"] = "confirmed"
    elif result == "unconfirmed":
        metadata["turnObserved"] = "unconfirmed"
    elif result == "tracking_lost":
        metadata["turnObserved"] = "pending"
    if str(record.get("runtimeQueueState", "unknown")) != "unknown":
        metadata["runtimeQueueState"] = str(record["runtimeQueueState"])
    if str(record.get("queueSource", "")) not in ("", "none"):
        metadata["queueSource"] = str(record["queueSource"])
    return metadata


def stop_sidecar(workspace: str) -> None:
    _request_sidecar(workspace, {"action": "shutdown"}, timeout=SOCKET_READY_TIMEOUT)
    deadline = time.monotonic() + SOCKET_READY_TIMEOUT
    while time.monotonic() < deadline:
        if not _socket_path(workspace).exists():
            return
        time.sleep(SOCKET_RETRY_INTERVAL)
    _cleanup_socket(workspace)
