from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any


def hive_home() -> Path:
    from .team import HIVE_HOME

    return HIVE_HOME


def factory_home() -> Path:
    return Path(os.environ.get("FACTORY_HOME", str(Path.home() / ".factory")))


def settings_path() -> Path:
    return factory_home() / "settings.json"


def claude_home() -> Path:
    return Path(os.environ.get("CLAUDE_HOME", str(Path.home() / ".claude")))


def claude_settings_path() -> Path:
    return claude_home() / "settings.json"


def codex_home() -> Path:
    return Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex")))


def codex_hooks_path() -> Path:
    return codex_home() / "hooks.json"


def load_settings() -> dict[str, Any]:
    path = settings_path()
    if not path.exists():
        return {}
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return {}


def save_settings(data: dict[str, Any]) -> None:
    path = settings_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")


def _load_json_file(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return {}


def _save_json_file(path: Path, data: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")


def _merge_hooks_in_data(
    data: dict[str, Any],
    hook_defs: dict[str, list[dict[str, Any]]],
) -> bool:
    hooks = data.setdefault("hooks", {})
    changed = False
    for event, groups in hook_defs.items():
        existing = hooks.setdefault(event, [])
        for group in groups:
            if group in existing:
                continue
            existing.append(group)
            changed = True
    return changed


def _remove_hooks_in_data(
    data: dict[str, Any],
    hook_defs: dict[str, list[dict[str, Any]]],
) -> bool:
    hooks = data.get("hooks")
    if not isinstance(hooks, dict):
        return False
    changed = False
    for event, groups in hook_defs.items():
        existing = hooks.get(event)
        if not isinstance(existing, list):
            continue
        new_existing = [g for g in existing if g not in groups]
        if new_existing != existing:
            changed = True
            if new_existing:
                hooks[event] = new_existing
            else:
                hooks.pop(event, None)
    if changed and not hooks:
        data.pop("hooks", None)
    return changed


def _ensure_codex_hooks_enabled() -> None:
    config_path = codex_home() / "config.toml"
    content = ""
    if config_path.exists():
        try:
            content = config_path.read_text()
        except OSError:
            return
    if "codex_hooks" in content:
        return
    config_path.parent.mkdir(parents=True, exist_ok=True)
    section = "\n[features]\ncodex_hooks = true\n"
    if "[features]" in content:
        content = content.replace("[features]", "[features]\ncodex_hooks = true", 1)
    else:
        content = content.rstrip() + section
    config_path.write_text(content)


CODEX_SUPPORTED_HOOK_EVENTS = {
    "SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop",
}


def _filter_hook_defs_for_codex(
    hook_defs: dict[str, list[dict[str, Any]]],
) -> dict[str, list[dict[str, Any]]]:
    return {k: v for k, v in hook_defs.items() if k in CODEX_SUPPORTED_HOOK_EVENTS}


def merge_hook_groups(hook_defs: dict[str, list[dict[str, Any]]]) -> dict[str, list[dict[str, Any]]]:
    added: dict[str, list[dict[str, Any]]] = {}
    # Droid
    droid_data = load_settings()
    if _merge_hooks_in_data(droid_data, hook_defs):
        save_settings(droid_data)
    # Claude Code
    claude_path = claude_settings_path()
    claude_data = _load_json_file(claude_path)
    if _merge_hooks_in_data(claude_data, hook_defs):
        _save_json_file(claude_path, claude_data)
    # Codex
    codex_path = codex_hooks_path()
    codex_defs = _filter_hook_defs_for_codex(hook_defs)
    if codex_defs:
        codex_data = _load_json_file(codex_path)
        if _merge_hooks_in_data(codex_data, codex_defs):
            _save_json_file(codex_path, codex_data)
        _ensure_codex_hooks_enabled()
    # Compute added (for return value, based on droid as reference)
    for event, groups in hook_defs.items():
        for group in groups:
            added.setdefault(event, []).append(group)
    return added


def remove_hook_groups(hook_defs: dict[str, list[dict[str, Any]]]) -> None:
    # Droid
    droid_data = load_settings()
    if _remove_hooks_in_data(droid_data, hook_defs):
        save_settings(droid_data)
    # Claude Code
    claude_path = claude_settings_path()
    claude_data = _load_json_file(claude_path)
    if _remove_hooks_in_data(claude_data, hook_defs):
        _save_json_file(claude_path, claude_data)
    # Codex
    codex_path = codex_hooks_path()
    codex_defs = _filter_hook_defs_for_codex(hook_defs)
    if codex_defs:
        codex_data = _load_json_file(codex_path)
        if _remove_hooks_in_data(codex_data, codex_defs):
            _save_json_file(codex_path, codex_data)
