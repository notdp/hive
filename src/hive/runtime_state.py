"""Shared state projection helpers for Hive runtime and CLI surfaces."""

from __future__ import annotations

_BODY_WARNING_CHAR_LIMIT = 500
_BODY_WARNING_LINE_LIMIT = 3
_BODY_WARNING_MARKERS = ("# ", "- ", "* ")


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


def body_warning_hint(body: str) -> dict[str, object] | None:
    """Suggest when a message body looks better suited for an artifact."""
    text = body.strip()
    if not text:
        return None
    lines = text.splitlines()
    reasons: list[str] = []
    if len(text) > _BODY_WARNING_CHAR_LIMIT:
        reasons.append("chars")
    if len(lines) >= _BODY_WARNING_LINE_LIMIT:
        reasons.append("lines")
    if "```" in text:
        reasons.append("fenced_code")
    if any(line.lstrip().startswith(_BODY_WARNING_MARKERS) for line in lines if line.strip()):
        reasons.append("markdown")
    if not reasons:
        return None
    return {
        "chars": len(text),
        "lines": len(lines),
        "reasons": reasons,
    }


def format_body_warning(*, command: str, hint: dict[str, object]) -> str:
    """Render the stderr hint for long or structured message bodies."""
    reasons = set(str(reason) for reason in hint.get("reasons", []))
    summary: list[str] = [
        f"{int(hint.get('chars') or 0)} chars",
        f"{int(hint.get('lines') or 0)} lines",
    ]
    if "fenced_code" in reasons:
        summary.append("fenced code")
    if "markdown" in reasons:
        summary.append("markdown")
    details = ", ".join(summary)
    return (
        f"warning: body looks long or structured ({details}); consider stdin artifact:\n"
        f"  cat <<'EOF' | hive {command} <agent> \"<short summary>\" --artifact -\n"
        "  ...\n"
        "  EOF"
    )


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


def gate_guidance(gate_status: str) -> dict[str, str] | None:
    if gate_status == "skipped":
        return {
            "gateNote": "Send gate was bypassed (transcript/session resolution failed). "
                        "Submit was attempted but input-gate safety was not checked.",
        }
    return None


def send_guidance(state: str) -> dict[str, str] | None:
    if state == "confirmed":
        return {
            "meaning": "Delivery was confirmed during the initial send window.",
            "recommendedAction": "continue",
        }
    if state == "queued":
        return {
            "meaning": "Accepted for background delivery tracking; no action is needed now.",
            "recommendedAction": "continue",
        }
    if state == "pending":
        return {
            "meaning": "Submit completed and background delivery tracking continues.",
            "recommendedAction": "continue",
        }
    if state == "deferred":
        return {
            "meaning": "Accepted by Hive and deferred for receiver review.",
            "recommendedAction": "continue",
        }
    if state == "failed":
        return {
            "meaning": "Local submit attempt failed before background tracking began.",
            "recommendedAction": "retry",
        }
    if state == "unconfirmed":
        return {
            "meaning": "Delivery was not confirmed within the synchronous wait window.",
            "recommendedAction": "check_delivery",
        }
    if state == "unavailable":
        return {
            "meaning": "Delivery tracking is unavailable for this send result.",
            "recommendedAction": "check_delivery",
        }
    return None


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


def project_thread_event(event: dict[str, object]) -> dict[str, object]:
    """Project durable send events into the smaller thread-facing shape."""
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
