"""Helpers for reading observation events from the workspace."""

from __future__ import annotations

import json
from pathlib import Path

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
            and data["metadata"].get("msgId") == message_id
        ):
            result = data  # Keep scanning — return the latest observation
    return result
