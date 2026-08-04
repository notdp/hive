"""Persisted Hive CLI context for standalone skill usage.

Context is stored **per tmux pane** so that multiple agents in the same
window don't overwrite each other's identity.  When TMUX_PANE is not set
(e.g. outside tmux) the file falls back to ``default.json``.
"""

from __future__ import annotations

import json
import os
from pathlib import Path


HIVE_HOME = Path(os.environ.get("HIVE_HOME", str(Path.home() / ".hive")))
CONTEXT_DIR = HIVE_HOME / "contexts"

# Legacy single-file path kept for clear_current_context cleanup.
CURRENT_CONTEXT_FILE = HIVE_HOME / "current.json"


def _context_file() -> Path:
    """Return the per-pane context file path."""
    pane = os.environ.get("TMUX_PANE", "")
    slug = pane.replace("%", "pane-") if pane else "default"
    return CONTEXT_DIR / f"{slug}.json"


def load_current_context() -> dict[str, str]:
    path = _context_file()
    if not path.exists():
        # Migrate from legacy single-file if it exists
        if CURRENT_CONTEXT_FILE.exists():
            try:
                data = json.loads(CURRENT_CONTEXT_FILE.read_text())
                return {str(k): str(v) for k, v in dict(data).items() if v}
            except (OSError, json.JSONDecodeError):
                pass
        return {}
    try:
        data = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return {}
    return {str(k): str(v) for k, v in dict(data).items() if v}


def save_current_context(
    *, team: str = "", workspace: str = "", agent: str = "", session: str = ""
) -> Path:
    path = _context_file()
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "team": team,
        "workspace": workspace,
        "agent": agent,
    }
    if session:
        # Which host session owns this identity. Only the desktop path sets it,
        # to keep sibling sessions running the same inbox hook from draining a
        # member's inbox that isn't theirs.
        payload["session"] = session
    path.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n")
    return path


def save_context_for_pane(pane_id: str, *, team: str = "", workspace: str = "", agent: str = "") -> Path:
    """Write context for an arbitrary pane (used by hive init to pre-bind agents)."""
    slug = pane_id.replace("%", "pane-") if pane_id else "default"
    path = CONTEXT_DIR / f"{slug}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "team": team,
        "workspace": workspace,
        "agent": agent,
    }
    path.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n")
    return path


def clear_context_for_pane(pane_id: str) -> None:
    """Remove a pane's saved context (registration rollback)."""
    slug = pane_id.replace("%", "pane-") if pane_id else "default"
    path = CONTEXT_DIR / f"{slug}.json"
    try:
        path.unlink()
    except OSError:
        pass


def clear_current_context() -> None:
    path = _context_file()
    if path.exists():
        path.unlink()
    # Also clean up legacy file
    if CURRENT_CONTEXT_FILE.exists():
        CURRENT_CONTEXT_FILE.unlink()
