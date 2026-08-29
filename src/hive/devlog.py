"""Developer-facing log paths and verbosity policy."""

from __future__ import annotations

from datetime import UTC, datetime
import os
from pathlib import Path


RUN_DIR_NAME = "run"
NOTIFY_LOG_NAME = "notify.jsonl"
HIVED_STDERR_NAME = "hived.stderr"
CVIM_DIR_NAME = "cvim"
GLOBAL_HIVE_DIR = Path(os.environ.get("XDG_CACHE_HOME", str(Path.home() / ".cache"))) / "hive"

_VERBOSITY_ENV = "HIVE_LOG_VERBOSITY"
_DEV_ONLY_EVENTS = frozenset({
    "active.changed",
    "tick.summary",
    "windows.changed",
})


def utc_timestamp_ms() -> str:
    return datetime.now(UTC).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def run_dir(workspace: str | Path) -> Path:
    return Path(workspace) / RUN_DIR_NAME


def notify_log_path(workspace: str | Path) -> Path:
    return run_dir(workspace) / NOTIFY_LOG_NAME


def global_notify_log_path() -> Path:
    return GLOBAL_HIVE_DIR / NOTIFY_LOG_NAME


def hived_stderr_path(workspace: str | Path) -> Path:
    return run_dir(workspace) / HIVED_STDERR_NAME


def cvim_log_dir(workspace: str | Path = "") -> Path:
    if workspace:
        return run_dir(workspace) / CVIM_DIR_NAME
    return GLOBAL_HIVE_DIR / CVIM_DIR_NAME


def log_paths(workspace: str | Path) -> dict[str, str]:
    return {
        "notify": str(notify_log_path(workspace)),
        "hived_stderr": str(hived_stderr_path(workspace)),
        "cvim_dir": str(cvim_log_dir(workspace)),
    }


def default_verbosity() -> str:
    env_value = os.environ.get(_VERBOSITY_ENV, "").strip().lower()
    if env_value in {"dev", "normal"}:
        return env_value
    source = Path(__file__).resolve()
    installed = any(parent.name in {"site-packages", "dist-packages"} for parent in source.parents)
    return "normal" if installed else "dev"


def should_emit(event: str) -> bool:
    if event not in _DEV_ONLY_EVENTS:
        return True
    return default_verbosity() == "dev"
