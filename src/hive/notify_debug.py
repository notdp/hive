"""Notify debug tracing.

Always-on JSONL log of notify state-machine transitions, covering both the
hived idle watcher and the notify_ui delivery path. Business-path events are
always recorded; low-information hived heartbeat events are only recorded
when Hive is running from a source checkout or ``HIVE_LOG_VERBOSITY=dev``.

Logs go to ``<workspace>/run/notify.jsonl`` when the workspace is known
(hived paths, select-hook cleanup with ``@hive-workspace``) and fall back
to ``~/.cache/hive/notify.jsonl`` (or ``$XDG_CACHE_HOME/hive/...``) when no
workspace can be resolved.

Hived callers already know their workspace and pass it explicitly via
``emit_for_window(..., workspace=...)``; UI helpers without the hint resolve
``@hive-workspace`` on the target window. ``workspace_for_window`` failures
fall back to the global log silently.

Multiple processes (hived loop, select-hook cleanup) write to the same log
via a single ``os.write`` call on an ``O_APPEND`` fd.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any

from . import devlog
from . import tmux


_LOG_NAME = "notify.jsonl"

_GLOBAL_DIR = devlog.GLOBAL_HIVE_DIR
_GLOBAL_LOG = devlog.global_notify_log_path()


def log_path(workspace: str) -> Path:
    return devlog.notify_log_path(workspace)


def workspace_for_window(window_target: str) -> str:
    if not window_target:
        return ""
    try:
        value = tmux.get_window_option(window_target, "hive-workspace") or ""
    except Exception:
        return ""
    return value.strip()


def emit_for_window(
    window_target: str,
    event: str,
    *,
    workspace: str = "",
    **fields: Any,
) -> None:
    """Emit by window. Pass ``workspace`` to skip the tmux lookup."""
    if not workspace:
        workspace = workspace_for_window(window_target)
    emit(workspace, event, **fields)


def emit(workspace: str, event: str, **fields: Any) -> None:
    if not devlog.should_emit(event):
        return
    record: dict[str, Any] = {
        "ts": devlog.utc_timestamp_ms(),
        "pid": os.getpid(),
        "component": "notify",
        "workspace": workspace or "<global>",
        "event": event,
    }
    for key, value in fields.items():
        if value is None:
            continue
        record[key] = value
    payload = json.dumps(record, ensure_ascii=False, separators=(",", ":")) + "\n"
    path = log_path(workspace) if workspace else _GLOBAL_LOG
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
    except OSError:
        return
    try:
        os.write(fd, payload.encode("utf-8"))
    except OSError:
        pass
    finally:
        os.close(fd)
