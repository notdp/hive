#!/usr/bin/env python3
"""Deliver a hive member's inbound messages into a session that channels cannot reach.

Claude Code delivers channel notifications only to sessions launched with
``--channels``. Sessions hosted by the Claude Code desktop app are not: the
app owns its argv, so a desktop-led duo worker never sees the push (measured
A/B: same binary, same stream-json transport, flag present -> delivered, flag
absent -> silently dropped). Plugin hooks *do* run there, so this Stop hook is
the delivery path for those sessions: it drains the member's bus inbox and
returns the messages as blocked-stop feedback, which Claude Code injects as
context and continues on — the same effect the channel would have had.

Silent no-op everywhere else: inside tmux the native channel/app-server
transports already deliver, and a session bound to no hive team has no inbox.
Only a real message ever blocks a stop, and each drain advances a durable
cursor, so the block cannot repeat on the same message.
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


def main() -> None:
    if os.environ.get("TMUX_PANE"):
        return  # native channel / app-server transports own delivery in tmux
    try:
        event = json.load(sys.stdin)
    except (ValueError, OSError):
        event = {}
    already_blocking = bool(event.get("stop_hook_active"))

    payload = _collect([])
    if not payload.get("messages") and not already_blocking:
        # Nothing unread. Wait for the answer only while one is actually owed,
        # so an idle session ends its turn immediately.
        payload = _collect(["--wait", str(WAIT_SECONDS), "--if-awaiting"])
    messages = payload.get("messages") or []
    if not messages:
        return

    body = "\n\n".join(_envelope(m) for m in messages)
    count = len(messages)
    print(json.dumps({
        "decision": "block",
        "reason": (
            f"{count} hive message{'s' if count > 1 else ''} arrived. Follow the hive "
            f"protocol for each (reply with `hive reply <agent>`):\n\n{body}"
        ),
        "systemMessage": f"hive: {count} message{'s' if count > 1 else ''} delivered",
    }))


if __name__ == "__main__":
    main()
