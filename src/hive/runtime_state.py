"""Shared state projection helpers for Hive runtime and CLI surfaces."""

from __future__ import annotations


def build_queue_probe_text(body: str, *, limit: int = 48) -> str:
    """Build a short body-derived needle for runtime queue detection."""
    text = body.strip()
    if not text:
        return ""
    for line in text.splitlines():
        collapsed = " ".join(line.split())
        if collapsed:
            return collapsed[:limit]
    return " ".join(text.split())[:limit]


def present_send_state(*, inject_status: str, turn_observed: str, runtime_queue_state: str) -> str:
    """Collapse internal delivery details into one default send state."""
    if inject_status == "failed":
        return "failed"
    if turn_observed == "confirmed":
        return "confirmed"
    if turn_observed == "unconfirmed":
        return "unconfirmed"
    if runtime_queue_state == "queued":
        return "queued"
    if turn_observed == "unavailable":
        return "unavailable"
    return "pending"


def present_delivery_state(
    *,
    inject_status: str,
    turn_observed: str,
    runtime_queue_state: str,
    observation_result: str = "",
) -> str:
    """Collapse persisted delivery detail into one primary state."""
    if inject_status == "failed":
        return "failed"
    if observation_result:
        return observation_result
    if turn_observed == "confirmed":
        return "confirmed"
    if turn_observed == "unconfirmed":
        return "unconfirmed"
    if runtime_queue_state == "queued":
        return "queued"
    if turn_observed == "unavailable":
        return "unavailable"
    return "pending"


def delivery_guidance(state: str) -> dict[str, str] | None:
    if state == "failed":
        return {
            "meaning": "Local submit attempt failed before delivery tracking began.",
            "recommendedAction": "retry",
        }
    if state == "tracking_lost":
        return {
            "meaning": "Delivery tracking was lost. Final delivery is unknown.",
            "recommendedAction": "investigate",
        }
    if state == "unconfirmed":
        return {
            "meaning": "Delivery was not confirmed before the timeout window elapsed.",
            "recommendedAction": "cautious_retry",
        }
    return None


def delivery_exception_body(
    state: str,
    *,
    message_id: str,
    target_agent: str,
    timeout_seconds: float,
) -> str | None:
    guidance = delivery_guidance(state)
    if guidance is None:
        return None
    meaning = guidance["meaning"]
    if state == "failed":
        return (
            f"Message {message_id} to {target_agent}: {meaning} "
            "Retry is reasonable."
        )
    if state == "tracking_lost":
        return (
            f"Message {message_id} to {target_agent}: {meaning} "
            "Inspect before retrying."
        )
    if state == "unconfirmed":
        return (
            f"Message {message_id} to {target_agent} was not confirmed within "
            f"{int(timeout_seconds)}s. {meaning} "
            "Retry only if duplicate delivery is acceptable."
        )
    return None


def project_inbox_event(event: dict[str, object]) -> dict[str, object]:
    """Project durable events into the smaller inbox-facing shape."""
    projected: dict[str, object] = {}
    for key in (
        "from",
        "to",
        "intent",
        "metadata",
        "createdAt",
        "msgId",
        "inReplyTo",
        "body",
        "artifact",
    ):
        value = event.get(key)
        if value in ("", None):
            continue
        projected[key] = value
    return projected


def project_inbox_observation(event: dict[str, object]) -> dict[str, object]:
    """Project observation events into a smaller inbox-facing shape."""
    projected: dict[str, object] = {}
    metadata = event.get("metadata") if isinstance(event.get("metadata"), dict) else {}
    msg_id = event.get("msgId") or metadata.get("msgId")
    if msg_id not in ("", None):
        projected["msgId"] = msg_id
    if event.get("intent"):
        projected["intent"] = event["intent"]
    if event.get("createdAt"):
        projected["createdAt"] = event["createdAt"]
    obs_meta: dict[str, object] = {}
    if metadata.get("result") not in ("", None):
        obs_meta["result"] = metadata["result"]
    if metadata.get("observedAt") not in ("", None):
        obs_meta["observedAt"] = metadata["observedAt"]
    if obs_meta:
        projected["metadata"] = obs_meta
    return projected


def format_hive_envelope(
    *,
    from_agent: str,
    to_agent: str,
    body: str,
    artifact: str = "",
    message_id: str = "",
    reply_to: str = "",
) -> str:
    attrs: list[tuple[str, str]] = [
        ("from", from_agent),
        ("to", to_agent),
    ]
    if message_id:
        attrs.append(("msgId", message_id))
    if reply_to:
        attrs.append(("reply-to", reply_to))
    if artifact:
        attrs.append(("artifact", artifact))
    header = "<HIVE " + " ".join(f"{key}={value}" for key, value in attrs) + ">"
    payload = body.strip() if body.strip() else "(no message)"
    return f"{header}\n{payload}\n</HIVE>"
