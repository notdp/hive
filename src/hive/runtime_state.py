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
