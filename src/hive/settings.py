"""User-level settings stored at ``$HIVE_HOME/settings.json``.

Dot-path keys (e.g. ``droid.selfPeer``) map to nested JSON. Missing file or
unreadable JSON returns an empty dict — settings are entirely optional.
"""

from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path
from typing import Any

CONFIGURABLE_ROLES = frozenset({"orch", "challenger", "worker", "validator"})
APPLIED_ROLES = frozenset({"challenger", "worker", "validator"})


def _hive_home() -> Path:
    return Path(os.environ.get("HIVE_HOME", str(Path.home() / ".hive")))


def _settings_path() -> Path:
    return _hive_home() / "settings.json"


def load_user_settings() -> dict[str, Any]:
    path = _settings_path()
    try:
        text = path.read_text()
    except OSError:
        return {}
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        return {}
    if not isinstance(data, dict):
        return {}
    return data


def get_setting(key: str, default: Any = None) -> Any:
    parts = [p for p in key.split(".") if p]
    if not parts:
        return default
    node: Any = load_user_settings()
    for part in parts:
        if not isinstance(node, dict) or part not in node:
            return default
        node = node[part]
    return node


def set_setting(key: str, value: Any) -> None:
    parts = [p for p in key.split(".") if p]
    if not parts:
        raise ValueError("empty key")
    data = load_user_settings()
    node = data
    for part in parts[:-1]:
        existing = node.get(part)
        if not isinstance(existing, dict):
            existing = {}
            node[part] = existing
        node = existing
    node[parts[-1]] = value
    _write_atomic(data)


def unset_setting(key: str) -> bool:
    parts = [p for p in key.split(".") if p]
    if not parts:
        return False
    data = load_user_settings()
    node: Any = data
    for part in parts[:-1]:
        if not isinstance(node, dict) or part not in node:
            return False
        node = node[part]
    if not isinstance(node, dict) or parts[-1] not in node:
        return False
    del node[parts[-1]]
    _write_atomic(data)
    return True


def resolve_role_config(role: str) -> tuple[str, str]:
    """Return ``(cli, model)`` overrides for *role* from user settings.

    Only values under ``roles.<role>.{cli, model}`` are read.  ``cli`` must be
    one of the recognised agent CLI names to be returned; otherwise ``""``.
    ``model`` must be a non-empty string; otherwise ``""``.  Unknown or
    unsupported *role* names always return ``("", "")``.
    """
    if role not in CONFIGURABLE_ROLES:
        return ("", "")

    from .agent_cli import AGENT_CLI_NAMES

    raw_cli = get_setting(f"roles.{role}.cli", "")
    cli = raw_cli if isinstance(raw_cli, str) and raw_cli in AGENT_CLI_NAMES else ""

    raw_model = get_setting(f"roles.{role}.model", "")
    model = raw_model if isinstance(raw_model, str) and raw_model else ""

    return (cli, model)


def _write_atomic(data: dict[str, Any]) -> None:
    path = _settings_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_path = tempfile.mkstemp(prefix=".settings.", suffix=".json.tmp", dir=str(path.parent))
    try:
        with os.fdopen(fd, "w") as fh:
            json.dump(data, fh, indent=2, sort_keys=True)
            fh.write("\n")
        os.replace(tmp_path, path)
    except Exception:
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise
