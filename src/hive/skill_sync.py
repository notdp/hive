from __future__ import annotations

import hashlib
import json
import shlex
import sys
import time
from importlib import resources
from pathlib import Path
from typing import Any, Callable

from . import core_hooks
from .agent_cli import normalize_command

_HIVE_SKILL_NAME = "hive"
_HIVE_SKILL_RESOURCE = ("skills", "hive", "SKILL.md")
_WARNING_INTERVAL_SECONDS = 24 * 60 * 60
_DEFAULT_REMOTE_SKILL_SOURCE = "https://github.com/notdp/hive"


def _canonical_hive_skill_bytes() -> bytes:
    resource = resources.files("hive.core_assets")
    for part in _HIVE_SKILL_RESOURCE:
        resource = resource.joinpath(part)
    return resource.read_bytes()


def _canonical_hive_skill_hash() -> str:
    return hashlib.sha256(_canonical_hive_skill_bytes()).hexdigest()


def _local_repo_root() -> Path | None:
    root = Path(__file__).resolve().parents[2]
    if (root / "skills" / _HIVE_SKILL_NAME / "SKILL.md").is_file():
        return root
    return None


def _refresh_command() -> str:
    repo_root = _local_repo_root()
    source = shlex.quote(str(repo_root)) if repo_root is not None else _DEFAULT_REMOTE_SKILL_SOURCE
    return f"npx skills add {source} -g --skill {_HIVE_SKILL_NAME} --agent '*' -y"


def _shared_hive_skill_path() -> Path:
    return Path.home() / ".agents" / "skills" / _HIVE_SKILL_NAME / "SKILL.md"


def hive_skill_path_for_cli(cli: str) -> Path:
    normalized = normalize_command(cli)
    if normalized == "codex":
        return core_hooks.codex_home() / "skills" / _HIVE_SKILL_NAME / "SKILL.md"
    if normalized == "claude":
        return core_hooks.claude_home() / "skills" / _HIVE_SKILL_NAME / "SKILL.md"
    if normalized == "droid":
        return core_hooks.factory_home() / "skills" / _HIVE_SKILL_NAME / "SKILL.md"
    raise ValueError(f"unsupported cli '{cli}'")


def _read_hash(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def diagnose_hive_skill(cli: str) -> dict[str, Any]:
    normalized = normalize_command(cli)
    payload: dict[str, Any] = {
        "skill": _HIVE_SKILL_NAME,
        "cli": normalized,
        "refreshCommand": _refresh_command(),
        "canonicalSource": "package:hive.core_assets/skills/hive/SKILL.md",
    }

    try:
        expected_hash = _canonical_hive_skill_hash()
    except (FileNotFoundError, ModuleNotFoundError, OSError) as exc:
        payload.update({
            "state": "error",
            "error": f"canonical hive skill unavailable: {exc}",
            "recommendedAction": "reinstall",
        })
        return payload

    payload["expectedHash"] = expected_hash

    try:
        installed_path = hive_skill_path_for_cli(normalized)
    except ValueError as exc:
        payload.update({
            "state": "error",
            "error": str(exc),
            "recommendedAction": "reinstall",
        })
        return payload

    shared_path = _shared_hive_skill_path()
    payload["installedPath"] = str(installed_path)
    payload["sharedPath"] = str(shared_path)
    payload["installedExists"] = installed_path.is_file()
    payload["sharedExists"] = shared_path.is_file()

    if installed_path.is_file():
        try:
            payload["actualHash"] = _read_hash(installed_path)
        except OSError as exc:
            payload.update({
                "state": "error",
                "error": f"failed to read installed hive skill: {exc}",
                "recommendedAction": "refresh",
            })
            return payload

    if shared_path.is_file():
        try:
            payload["sharedHash"] = _read_hash(shared_path)
        except OSError:
            pass

    actual_hash = str(payload.get("actualHash") or "")
    if not installed_path.is_file():
        payload.update({
            "state": "missing",
            "recommendedAction": "refresh",
            "meaning": "Installed hive skill for this CLI was not found.",
        })
        return payload

    if actual_hash == expected_hash:
        payload.update({
            "state": "current",
            "recommendedAction": "none",
            "meaning": "Installed hive skill matches the packaged canonical skill.",
        })
        return payload

    payload.update({
        "state": "stale",
        "recommendedAction": "refresh",
        "meaning": "Installed hive skill differs from the packaged canonical skill.",
    })
    return payload


def _warning_state_path(cli: str) -> Path:
    return core_hooks.hive_home() / "state" / "skill-sync" / f"{normalize_command(cli)}.json"


def _load_warning_state(path: Path) -> dict[str, Any]:
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return {}


def _warning_key(payload: dict[str, Any]) -> dict[str, str]:
    return {
        "state": str(payload.get("state") or ""),
        "cli": str(payload.get("cli") or ""),
        "installedPath": str(payload.get("installedPath") or ""),
        "expectedHash": str(payload.get("expectedHash") or ""),
        "actualHash": str(payload.get("actualHash") or ""),
    }


def _should_emit_warning(payload: dict[str, Any], *, now: float) -> bool:
    path = _warning_state_path(str(payload.get("cli") or ""))
    current_key = _warning_key(payload)
    previous = _load_warning_state(path)
    previous_key = previous.get("key")
    previous_at = float(previous.get("lastNotifiedAt") or 0)
    if previous_key == current_key and now - previous_at < _WARNING_INTERVAL_SECONDS:
        return False
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps({
            "key": current_key,
            "lastNotifiedAt": now,
        }, indent=2, ensure_ascii=False) + "\n")
    except OSError:
        return True
    return True


def render_hive_skill_warning(payload: dict[str, Any]) -> str:
    lines = [
        f"Warning: installed hive skill for {payload.get('cli', 'unknown')} is {payload.get('state', 'unknown')}.",
        f"  installed: {payload.get('installedPath', '(missing)')}",
        f"  expected:  {payload.get('expectedHash', '(unknown)')}",
    ]
    if payload.get("actualHash"):
        lines.append(f"  actual:    {payload['actualHash']}")
    if payload.get("sharedExists"):
        lines.append(f"  shared:    {payload.get('sharedPath', '')}")
    lines.extend([
        "Refresh with:",
        f"  {payload.get('refreshCommand', _refresh_command())}",
        "Inspect details with:",
        "  hive doctor --skills",
    ])
    return "\n".join(lines)


def maybe_warn_hive_skill_drift(
    cli: str,
    *,
    emit: Callable[[str], None] | None = None,
    now: float | None = None,
) -> dict[str, Any]:
    payload = diagnose_hive_skill(cli)
    if payload.get("state") not in {"missing", "stale"}:
        return payload

    observed_at = now if now is not None else time.time()
    if not _should_emit_warning(payload, now=observed_at):
        return payload

    if emit is None:
        emit = lambda message: sys.stderr.write(message + "\n")
    emit(render_hive_skill_warning(payload))
    return payload
