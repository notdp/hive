from __future__ import annotations

import json
import os
from importlib import resources
from pathlib import Path
from typing import Any


SESSION_LOCATOR_HOOK_NAME = "droid-session-map-hook"
SESSION_LOCATOR_EVENTS = ("SessionStart", "UserPromptSubmit", "SessionEnd")


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


def session_map_path() -> Path:
    if os.environ.get("HIVE_SESSION_MAP_FILE"):
        return Path(os.environ["HIVE_SESSION_MAP_FILE"])
    cache_root = Path(os.environ.get("XDG_CACHE_HOME", str(Path.home() / ".cache")))
    return cache_root / "hive" / "session-map.json"


def core_bin_dir() -> Path:
    return hive_home() / "core" / "bin"


def session_hook_script_path() -> Path:
    return core_bin_dir() / "droid-session-map-hook"


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


def _session_hook_resource_text() -> str:
    return resources.files("hive.core_assets").joinpath("droid-session-map-hook").read_text()


def core_session_hook_defs(script_path: Path | None = None) -> dict[str, list[dict[str, Any]]]:
    resolved = str(script_path or session_hook_script_path())
    group = {"hooks": [{"type": "command", "command": resolved}]}
    return {
        "SessionStart": [group],
        "UserPromptSubmit": [group],
        "SessionEnd": [group],
    }


def _is_session_locator_group(group: Any, *, script_path: Path | None = None) -> bool:
    if not isinstance(group, dict):
        return False
    hooks = group.get("hooks")
    if not isinstance(hooks, list) or len(hooks) != 1:
        return False
    hook = hooks[0]
    if not isinstance(hook, dict) or hook.get("type") != "command":
        return False
    command = str(hook.get("command", "") or "")
    if not command:
        return False
    normalized = str(Path(command).expanduser())
    managed_command = str((script_path or session_hook_script_path()).expanduser())
    return normalized == managed_command


def install_or_update_session_locator_hooks(script_path: Path | None = None) -> dict[str, int]:
    settings = load_settings()
    hooks = settings.setdefault("hooks", {})
    resolved_script_path = script_path or session_hook_script_path()
    target_group = core_session_hook_defs(resolved_script_path)["SessionStart"][0]
    removed = 0
    added = 0

    for event in SESSION_LOCATOR_EVENTS:
        existing = hooks.get(event, [])
        if not isinstance(existing, list):
            existing = []
        filtered = [group for group in existing if not _is_session_locator_group(group, script_path=resolved_script_path)]
        removed += len(existing) - len(filtered)
        filtered.append(target_group)
        hooks[event] = filtered
        added += 1

    save_settings(settings)
    return {"removed": removed, "installed": added}


def _install_hooks_in_json_file(config_path: Path, script_path: Path) -> dict[str, int]:
    try:
        data = json.loads(config_path.read_text()) if config_path.exists() else {}
    except (OSError, json.JSONDecodeError):
        data = {}
    if not isinstance(data, dict):
        data = {}
    hooks = data.setdefault("hooks", {})
    target_group = core_session_hook_defs(script_path)["SessionStart"][0]
    removed = 0
    added = 0
    for event in SESSION_LOCATOR_EVENTS:
        existing = hooks.get(event, [])
        if not isinstance(existing, list):
            existing = []
        filtered = [g for g in existing if not _is_session_locator_group(g, script_path=script_path)]
        removed += len(existing) - len(filtered)
        filtered.append(target_group)
        hooks[event] = filtered
        added += 1
    config_path.parent.mkdir(parents=True, exist_ok=True)
    config_path.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")
    return {"removed": removed, "installed": added}


def ensure_session_locator_hook_installed() -> dict[str, object]:
    script_path = session_hook_script_path()
    script_path.parent.mkdir(parents=True, exist_ok=True)
    script_path.write_text(_session_hook_resource_text())
    script_path.chmod(0o755)
    factory_result = install_or_update_session_locator_hooks(script_path)
    _install_hooks_in_json_file(claude_settings_path(), script_path)
    codex_path = codex_hooks_path()
    _install_hooks_in_json_file(codex_path, script_path)
    _ensure_codex_hooks_enabled()
    return {
        "script": str(script_path),
        "settings": str(settings_path()),
        "sessionMap": str(session_map_path()),
        "hooksInstalled": factory_result["installed"],
        "hooksRemoved": factory_result["removed"],
    }


def load_session_map() -> dict[str, dict[str, dict[str, Any]]]:
    path = session_map_path()
    if not path.exists():
        return {"by_pane": {}, "by_tty": {}, "by_pid": {}}
    try:
        data = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return {"by_pane": {}, "by_tty": {}, "by_pid": {}}
    return {
        "by_pane": dict(data.get("by_pane", {})),
        "by_tty": dict(data.get("by_tty", {})),
        "by_pid": dict(data.get("by_pid", {})),
    }


def resolve_session_record(*, pane_id: str = "", tty: str = "", pid: str = "") -> dict[str, Any] | None:
    data = load_session_map()
    if pane_id:
        record = data.get("by_pane", {}).get(pane_id)
        if isinstance(record, dict) and record.get("session_id"):
            return record
    if pid:
        record = data.get("by_pid", {}).get(str(pid))
        if isinstance(record, dict) and record.get("session_id"):
            return record
    if tty:
        record = data.get("by_tty", {}).get(tty)
        if isinstance(record, dict) and record.get("session_id"):
            return record
    return None
