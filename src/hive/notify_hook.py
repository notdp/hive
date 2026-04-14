from __future__ import annotations

import json
import os
import sys
from typing import Any

from . import notify_state
from . import notify_ui


def resolve_target_pane() -> str:
    return os.environ.get("TMUX_PANE", "").strip()


def classify_hook_payload(payload: dict[str, Any]) -> tuple[str, str] | None:
    event_name = str(payload.get("hook_event_name") or payload.get("hookEventName") or "")
    if event_name == "Notification":
        message = str(payload.get("message") or "").strip()
        return "waiting_input", message or "Droid needs your attention. Return to the pane."
    if event_name == "Stop":
        if payload.get("stop_hook_active"):
            return None
        return "completed", "Droid finished responding. Return to the pane to review the result."
    return None


def handle_hook_payload(payload: dict[str, Any]) -> int:
    target = classify_hook_payload(payload)
    if target is None:
        return 0
    pane_id = resolve_target_pane()
    if not pane_id:
        return 0
    kind, message = target
    if notify_state.should_suppress_hook_notification(pane_id, kind=kind, message=message):
        return 0
    notify_ui.notify(message, pane_id, source=notify_state.SOURCE_HOOK, kind=kind)
    return 0


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except (OSError, json.JSONDecodeError):
        return 0
    if not isinstance(payload, dict):
        return 0
    try:
        return handle_hook_payload(payload)
    except Exception:
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
