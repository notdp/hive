#!/usr/bin/env python3
"""Deliver a hive member's inbound messages into a session that channels cannot reach.

Claude Code delivers channel notifications only to sessions launched with
``--channels``. Sessions hosted by the Claude Code desktop app are not: the
app owns its argv, so a desktop-led duo worker never sees the push (measured
A/B: same binary, same stream-json transport, flag present -> delivered, flag
absent -> silently dropped). A hook is the only injection point that lives
*inside* the session process, so it is the delivery path for those members.

Two events, two jobs:

- ``PostToolUse`` — mid-turn delivery. A member doing real work calls tools
  constantly, so each tool result is a delivery point; a reply that lands
  during a long turn arrives within one tool call instead of at turn end.
  Never blocks, never waits.
- ``Stop`` — the idle/settle path. Drains what is left, and while one of the
  member's own messages is still unanswered it holds the turn open waiting
  for the answer (the duo's "wait for the verdict" semantics).

Silent no-op everywhere else: inside tmux the native channel/app-server
transports already deliver, and a session bound to no hive team has no inbox.
Each drain advances a durable cursor, so no message is delivered twice and a
blocked stop cannot repeat on the same message. ``--session`` scopes the
drain to the member's own session, so other desktop sessions running the same
hook never steal its inbox.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys

WAIT_SECONDS = 120  # ponytail: fixed budget; make it a setting if a duo outgrows it


def _collect(args: list[str]) -> dict:
    hive = shutil.which("hive")
    if not hive:
        return {}
    try:
        out = subprocess.run(
            [hive, "collect", *args],
            capture_output=True,
            text=True,
            timeout=WAIT_SECONDS + 30,
        )
    except (OSError, subprocess.SubprocessError):
        return {}
    if out.returncode != 0:
        return {}
    try:
        payload = json.loads(out.stdout)
    except ValueError:
        return {}
    return payload if isinstance(payload, dict) else {}


def _envelope(message: dict) -> str:
    head = f"<HIVE from={message.get('from', '?')} to={message.get('to', '?')}"
    if message.get("msgId"):
        head += f" msgId={message['msgId']}"
    if message.get("replyTo") or message.get("inReplyTo"):
        head += f" reply-to={message.get('replyTo') or message.get('inReplyTo')}"
    if message.get("artifact"):
        head += f" artifact={message['artifact']}"
    return f"{head}>\n{message.get('body', '')}\n</HIVE>"


def _preamble(count: int) -> str:
    return (
        f"{count} hive message{'s' if count > 1 else ''} arrived. Follow the hive "
        f"protocol for each (reply with `hive reply <agent>`):\n\n"
    )


def main() -> None:
    if os.environ.get("TMUX_PANE"):
        return  # native channel / app-server transports own delivery in tmux
    try:
        event = json.load(sys.stdin)
    except (ValueError, OSError):
        event = {}
    session = str(event.get("session_id") or "")
    claim = ["--session", session] if session else []

    if event.get("hook_event_name") == "PostToolUse":
        # Mid-turn delivery: every tool call is a delivery point, so a message
        # that lands while the member is working arrives with the next tool
        # result instead of waiting for the turn to end. Never waits — a turn
        # in progress must not be stalled by an empty inbox.
        messages = (_collect(claim).get("messages")) or []
        if not messages:
            return
        body = "\n\n".join(_envelope(m) for m in messages)
        print(json.dumps({
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": _preamble(len(messages)) + body,
            },
        }))
        return

    already_blocking = bool(event.get("stop_hook_active"))
    payload = _collect(claim)
    if not payload.get("messages") and not already_blocking:
        # Nothing unread. Wait for the answer only while one is actually owed,
        # so an idle session ends its turn immediately.
        payload = _collect([*claim, "--wait", str(WAIT_SECONDS), "--if-awaiting"])
    messages = payload.get("messages") or []
    if not messages:
        return

    body = "\n\n".join(_envelope(m) for m in messages)
    count = len(messages)
    print(json.dumps({
        "decision": "block",
        "reason": _preamble(count) + body,
        "systemMessage": f"hive: {count} message{'s' if count > 1 else ''} delivered",
    }))


if __name__ == "__main__":
    main()
