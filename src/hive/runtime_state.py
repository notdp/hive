"""Shared state projection helpers for Hive runtime and CLI surfaces."""

from __future__ import annotations

_BODY_WARNING_CHAR_LIMIT = 500
_BODY_WARNING_LINE_LIMIT = 3
_BODY_WARNING_MARKERS = ("# ", "- ", "* ")


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
        f"  hive {command} <agent> \"<short summary>\" --artifact - <<'EOF'\n"
        "  ...\n"
        "  EOF"
    )


def present_send_state(*, inject_status: str, turn_observed: str) -> str:
    """Collapse internal delivery details into one outcome: queued | success | failed.

    Failed means the transport itself refused the message (synchronously
    visible). A transport-accepted send whose transcript confirmation has not
    appeared yet is queued — never failed: per the Channels contract busy
    sessions queue events, so absence of confirmation is not proof of failure.
    """
    if inject_status == "failed":
        return "failed"
    if turn_observed == "confirmed":
        return "success"
    return "queued"


def present_delivery_state(
    *,
    inject_status: str,
    turn_observed: str,
    observation_result: str = "",
) -> str:
    """Collapse persisted delivery detail into one outcome: queued | success | failed.

    Historical terminal records keep their meaning: a durable ``failed``
    observation stays failed and is never retroactively promoted; a durable
    ``success`` stays success. Everything transport-accepted but unconfirmed
    projects to queued.
    """
    if inject_status == "failed":
        return "failed"
    if observation_result == "success":
        return "success"
    if observation_result == "failed":
        return "failed"
    if turn_observed == "confirmed":
        return "success"
    return "queued"


_QUEUED_GUIDANCE = (
    "transport write accepted; final delivery unconfirmed — the target may be "
    "mid-turn (channel events queue and deliver on its next turn). Do not resend."
)


def send_guidance(delivery: str) -> dict[str, str] | None:
    if delivery == "queued":
        return {"guidance": _QUEUED_GUIDANCE}
    return None


def delivery_guidance(delivery: str) -> dict[str, str] | None:
    if delivery == "queued":
        return {"guidance": _QUEUED_GUIDANCE}
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
