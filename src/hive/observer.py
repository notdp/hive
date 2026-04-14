"""Helpers for reading observation events from the workspace."""

from __future__ import annotations

from . import bus

def find_observation(workspace: str, message_id: str) -> dict | None:
    """Find the observation event for a given message ID."""
    return bus.find_latest_observation(workspace, message_id)
