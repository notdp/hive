from __future__ import annotations

import json
import os
import shutil
import subprocess
import time
from dataclasses import dataclass
from importlib import resources
from pathlib import Path
from typing import Any

from . import core_hooks


@dataclass
class PluginManifest:
    name: str
    description: str


def _plugins_root():
    return resources.files("hive.plugins")


def _state_path() -> Path:
    return core_hooks.hive_home() / "plugins" / "state.json"


def _installed_root() -> Path:
    return core_hooks.hive_home() / "plugins" / "installed"


def _factory_commands_dir() -> Path:
    return core_hooks.factory_home() / "commands"


def _factory_skills_dir() -> Path:
    return core_hooks.factory_home() / "skills"


def _claude_skills_dir() -> Path:
    return core_hooks.claude_home() / "skills"


def _codex_skills_dir() -> Path:
    return core_hooks.codex_home() / "skills"


def _skill_install_dirs() -> list[Path]:
    """Target directories where real plugin skills are symlinked.

    Command plugins (cvim/vim/vfork/hfork/notify) intentionally do NOT
    receive Claude or Codex wrappers; those agents are expected to invoke
    `hive <command>` via their built-in shell escape. Real model skills
    (e.g. `code-review`) still land in all three agent skill dirs.
    """
    return [
        _factory_skills_dir(),
        _claude_skills_dir(),
        _codex_skills_dir(),
    ]


def find_installed_command(plugin_name: str, command_name: str) -> Path | None:
    """Return the installed plugin command script path, or None if not present.

    Used by the top-level `hive cvim` / `hive vim` / `hive vfork` / `hive hfork`
    CLI subcommands to forward execution into the plugin-materialized bash
    helper without depending on Factory's slash command path.
    """
    path = _installed_root() / plugin_name / "commands" / command_name
    if path.is_file():
        return path
    return None


def _load_state() -> dict[str, Any]:
    path = _state_path()
    if not path.exists():
        return {"plugins": {}}
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return {"plugins": {}}


def _save_state(data: dict[str, Any]) -> None:
    path = _state_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")


def _remove_path(path: Path) -> None:
    if path.is_symlink() or path.is_file():
        path.unlink(missing_ok=True)
    elif path.is_dir():
        shutil.rmtree(path)


def _ensure_executable_if_script(path: Path) -> None:
    try:
        first_line = path.read_text(errors="ignore").splitlines()[0]
    except Exception:
        return
    if first_line.startswith("#!"):
        path.chmod(0o755)


def _copy_tree(src, dst: Path) -> None:
    dst.mkdir(parents=True, exist_ok=True)
    for child in src.iterdir():
        if child.name == "__pycache__" or child.suffix in {".pyc", ".pyo"}:
            continue
        target = dst / child.name
        if child.is_dir():
            _copy_tree(child, target)
        else:
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(child.read_text())
            _ensure_executable_if_script(target)


def _plugin_resource_dir(name: str):
    root = _plugins_root().joinpath(name)
    if not root.is_dir() or not root.joinpath("plugin.json").is_file():
        raise ValueError(f"plugin '{name}' not found")
    return root


def load_manifest(name: str) -> PluginManifest:
    root = _plugin_resource_dir(name)
    data = json.loads(root.joinpath("plugin.json").read_text())
    return PluginManifest(
        name=data["name"],
        description=data.get("description", ""),
    )


def list_plugins() -> list[dict[str, object]]:
    state = _load_state().get("plugins", {})
    rows: list[dict[str, object]] = []
    for child in sorted(_plugins_root().iterdir(), key=lambda item: item.name):
        if not child.is_dir() or not child.joinpath("plugin.json").is_file():
            continue
        manifest = load_manifest(child.name)
        rows.append(
            {
                "name": manifest.name,
                "description": manifest.description,
                "enabled": manifest.name in state,
            }
        )
    return rows


def _link_path(src: Path, dst: Path) -> None:
    dst.parent.mkdir(parents=True, exist_ok=True)
    _remove_path(dst)
    dst.symlink_to(src, target_is_directory=src.is_dir())


def _copy_path(src: Path, dst: Path) -> None:
    dst.parent.mkdir(parents=True, exist_ok=True)
    _remove_path(dst)
    shutil.copy2(src, dst)
    _ensure_executable_if_script(dst)


def _copy_text_with_plugin_root(src: Path, dst: Path, *, install_dir: Path) -> None:
    dst.parent.mkdir(parents=True, exist_ok=True)
    _remove_path(dst)
    content = src.read_text()
    dst.write_text(_render_plugin_text(content, install_dir=install_dir))
    _ensure_executable_if_script(dst)


def _render_plugin_text(content: str, *, install_dir: Path) -> str:
    return content.replace("${HIVE_PLUGIN_ROOT}", str(install_dir))


def _materialize_installed_commands(install_dir: Path) -> list[Path]:
    commands_dir = install_dir / "commands"
    if not commands_dir.is_dir():
        return []
    materialized: list[Path] = []
    for command_path in sorted(commands_dir.iterdir()):
        if command_path.name.startswith("."):
            continue
        command_path.write_text(_render_plugin_text(command_path.read_text(), install_dir=install_dir))
        _ensure_executable_if_script(command_path)
        materialized.append(command_path)
    return materialized


def _generate_factory_shim(command_path: Path) -> str:
    """Build a thin Factory slash-command shim that delegates to ``hive <name>``.

    The shim preserves all ``# DROID:`` directive comments from the source
    script so that Droid renders the correct description and honours the
    "return control immediately" behaviour, but the actual logic is
    forwarded to the ``hive`` CLI subcommand.
    """
    droid_lines: list[str] = []
    for line in command_path.read_text().splitlines():
        stripped = line.lstrip()
        if stripped.startswith("# DROID:"):
            droid_lines.append(stripped)
    body = "\n".join(droid_lines)
    name = command_path.name
    return f"#!/usr/bin/env bash\n{body}\nset -euo pipefail\nexec hive {name} \"$@\"\n"


def _install_commands(command_paths: list[Path]) -> list[str]:
    """Install thin shim scripts under ``~/.factory/commands/``.

    Each shim preserves the ``# DROID:`` comment directives from the plugin
    source script and delegates execution to ``hive <command-name>``.  The
    real logic stays in ``~/.hive/plugins/installed/<plugin>/commands/<name>``
    (the installed origin), which ``hive <name>`` exec's into.
    """
    if not command_paths:
        return []
    installed: list[str] = []
    target_dir = _factory_commands_dir()
    for command_path in command_paths:
        dst = target_dir / command_path.name
        dst.parent.mkdir(parents=True, exist_ok=True)
        _remove_path(dst)
        dst.write_text(_generate_factory_shim(command_path))
        _ensure_executable_if_script(dst)
        installed.append(str(dst))
    return installed


def _source_tmux_conf(conf: Path) -> bool:
    if not conf.is_file():
        return False
    try:
        subprocess.run(
            ["tmux", "source-file", str(conf)],
            capture_output=True, check=True, timeout=5,
        )
        return True
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired, FileNotFoundError):
        return False


def _install_tmux_bindings(install_dir: Path) -> bool:
    return _source_tmux_conf(install_dir / "tmux" / "enable.conf")


def _uninstall_tmux_bindings(install_dir: Path) -> bool:
    return _source_tmux_conf(install_dir / "tmux" / "disable.conf")


def _is_plugin_managed_skill(path: Path) -> bool:
    """Return True if *path* is a symlink pointing into the hive plugin installed tree."""
    if not path.is_symlink():
        return False
    try:
        target = path.resolve()
        return target.is_relative_to(_installed_root())
    except (OSError, ValueError):
        return False


def _install_skills(install_dir: Path) -> list[str]:
    """Install plugin skill directories for Factory, Claude Code, and Codex.

    Used for real model skills (e.g. `code-review`). Command plugins do not
    reach this path because their source trees do not ship a `skills/`
    directory; they only carry `commands/` scripts that land in Factory.
    """
    skills_dir = install_dir / "skills"
    if not skills_dir.is_dir():
        return []
    linked: list[str] = []
    skipped: list[str] = []
    for target_dir in _skill_install_dirs():
        for skill_dir in sorted(skills_dir.iterdir()):
            if skill_dir.name.startswith("."):
                continue
            dst = target_dir / skill_dir.name
            if dst.exists() and not _is_plugin_managed_skill(dst):
                skipped.append(str(dst))
                continue
            _link_path(skill_dir.resolve(), dst)
            linked.append(str(dst))
    if skipped:
        import click
        click.echo(
            f"Warning: skipped skill(s) {', '.join(skipped)} — "
            "already exists and is not managed by a hive plugin. "
            "Remove or rename the existing skill to allow the plugin to install it.",
            err=True,
        )
    return linked


def _substitute_hook_value(value: Any, *, install_dir: Path) -> Any:
    if isinstance(value, str):
        return value.replace("${HIVE_PLUGIN_ROOT}", str(install_dir))
    if isinstance(value, list):
        return [_substitute_hook_value(item, install_dir=install_dir) for item in value]
    if isinstance(value, dict):
        return {key: _substitute_hook_value(item, install_dir=install_dir) for key, item in value.items()}
    return value


def _plugin_hook_defs(install_dir: Path) -> dict[str, list[dict[str, Any]]]:
    path = install_dir / "hooks" / "hooks.json"
    if not path.exists():
        return {}
    data = json.loads(path.read_text())
    return _substitute_hook_value(data, install_dir=install_dir)


def disable_plugin(name: str, *, missing_ok: bool = False) -> dict[str, object]:
    state = _load_state()
    plugins = state.setdefault("plugins", {})
    plugin_state = plugins.get(name)
    if plugin_state is None:
        if missing_ok:
            return {"name": name, "enabled": False}
        raise ValueError(f"plugin '{name}' is not enabled")

    for path_str in plugin_state.get("commands", []):
        _remove_path(Path(path_str))
    for path_str in plugin_state.get("skills", []):
        skill_path = Path(path_str)
        if skill_path.exists() and not _is_plugin_managed_skill(skill_path):
            continue
        _remove_path(skill_path)
    hook_defs = plugin_state.get("hooks", {})
    if isinstance(hook_defs, dict) and hook_defs:
        core_hooks.remove_hook_groups(hook_defs)
    install_root = Path(plugin_state.get("installRoot", "")) if plugin_state.get("installRoot") else None
    if install_root and plugin_state.get("tmux"):
        _uninstall_tmux_bindings(install_root)
    if install_root:
        _remove_path(install_root)
    plugins.pop(name, None)
    _save_state(state)
    return {"name": name, "enabled": False}


def enable_plugin(name: str) -> dict[str, object]:
    manifest = load_manifest(name)
    disable_plugin(name, missing_ok=True)

    install_dir = _installed_root() / name
    _remove_path(install_dir)
    install_dir.parent.mkdir(parents=True, exist_ok=True)
    _copy_tree(_plugin_resource_dir(name), install_dir)

    command_paths = _materialize_installed_commands(install_dir)
    commands = _install_commands(command_paths)
    skills = _install_skills(install_dir)
    hook_defs = _plugin_hook_defs(install_dir)
    if hook_defs:
        core_hooks.merge_hook_groups(hook_defs)
    has_tmux = _install_tmux_bindings(install_dir)

    state = _load_state()
    plugin_state: dict[str, object] = {
        "installRoot": str(install_dir),
        "commands": commands,
        "skills": skills,
        "hooks": hook_defs,
        "enabledAt": int(time.time()),
    }
    if has_tmux:
        plugin_state["tmux"] = True
    state.setdefault("plugins", {})[name] = plugin_state
    _save_state(state)

    return {
        "name": manifest.name,
        "description": manifest.description,
        "enabled": True,
        "installRoot": str(install_dir),
        "commands": commands,
        "skills": skills,
        "tmux": has_tmux,
    }
