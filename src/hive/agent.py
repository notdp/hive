"""Agent: a droid instance running in a tmux pane."""

from __future__ import annotations

import json
import os
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from . import draft_guard
from . import skill_sync
from . import tmux
from .agent_cli import resolve_session_id_for_pane

DROID_BIN = os.environ.get("DROID_PATH", str(Path.home() / ".local" / "bin" / "droid"))
AGENT_STARTUP_TIMEOUT = 30
_TMUX_REQUIRED_MESSAGE = "Hive requires tmux. Start or attach to a tmux session first."

CLI_BINS: dict[str, str] = {
    "droid": DROID_BIN,
    "claude": "claude",
    "codex": "codex",
}


def _factory_home() -> Path:
    return Path(os.environ.get("FACTORY_HOME", str(Path.home() / ".factory")))


def _settings_file() -> Path:
    return _factory_home() / "settings.json"



def _shell_escape(s: str) -> str:
    """Escape a string for safe shell use."""
    return "'" + s.replace("'", "'\\''") + "'"


def _resolve_model_id(model: str, settings: dict[str, Any]) -> str:
    """Resolve model alias/displayName to canonical model ID from settings.json."""
    if not model:
        return model

    base = model.replace("custom:", "", 1)
    for m in settings.get("customModels", []):
        model_id = m.get("id")
        if not model_id:
            continue
        if (
            model_id == model
            or m.get("model", "") == base
            or m.get("displayName", "") == base
        ):
            return model_id
    return model


def _load_settings() -> dict[str, Any]:
    settings_file = _settings_file()
    if not settings_file.is_file():
        return {}
    with open(settings_file) as f:
        return json.load(f)



def _resolve_session_id_from_runtime(pane_id: str = "") -> str | None:
    resolved_pane = pane_id or tmux.get_current_pane_id() or ""
    if not resolved_pane:
        return None
    return resolve_session_id_for_pane(resolved_pane)


def detect_current_session_id(cwd: str, model: str = "", pane_id: str = "") -> str | None:
    """Best-effort lookup for the current droid session ID."""
    return _resolve_session_id_from_runtime(pane_id)


def _build_droid_model_settings(model: str) -> tuple[str, str]:
    """Resolve model ID and return inline JSON for --settings process substitution.

    Returns (json_str, resolved_model_id).  Empty json_str when no model given.
    """
    if not model:
        return "", model
    settings = _load_settings()
    target_id = _resolve_model_id(model, settings)
    return json.dumps({"sessionDefaultSettings": {"model": target_id}}), target_id


def _submit_interactive_text(pane_id: str, text: str, cli: str) -> None:
    """Submit text to an interactive agent TUI, preserving any pending draft."""
    if tmux.is_pane_in_mode(pane_id):
        tmux.cancel_pane_mode(pane_id)
        time.sleep(0.05)

    profile_name = _resolve_profile_name(pane_id, cli)
    buffer_name = _save_and_clear_draft(pane_id, profile_name)

    tmux.send_keys(pane_id, text, enter=False)
    time.sleep(0.05)
    tmux.send_key(pane_id, "Enter")

    if buffer_name:
        _restore_draft(pane_id, profile_name, buffer_name)


def _save_and_clear_draft(pane_id: str, profile_name: str) -> str:
    """Best-effort: if a draft exists, save it to a tmux buffer and clear input.

    Returns the buffer name to restore later, or '' when no draft / on any error.
    """
    if not draft_guard.supported_profile(profile_name):
        return ""
    try:
        draft_text = draft_guard.parse_draft(pane_id, profile_name)
        if not draft_text:
            return ""
        buffer_name = f"hive_draft_{pane_id.replace('%', '')}"
        tmux.load_buffer(buffer_name, draft_text)
        draft_guard.clear_input(pane_id, profile_name)
        draft_guard.wait_input_empty(pane_id, profile_name, timeout=1.0)
        return buffer_name
    except Exception:
        return ""


def _restore_draft(pane_id: str, profile_name: str, buffer_name: str) -> None:
    try:
        draft_guard.wait_input_empty(pane_id, profile_name, timeout=2.0)
        tmux.paste_buffer(buffer_name, pane_id, bracketed=True)
    finally:
        tmux.delete_buffer(buffer_name)


def _resolve_profile_name(pane_id: str, cli: str) -> str:
    """Prefer runtime detection; fall back to the declared cli."""
    try:
        from .agent_cli import detect_profile_for_pane, get_profile
    except Exception:
        return cli
    profile = detect_profile_for_pane(pane_id)
    if profile is None and cli:
        profile = get_profile(cli)
    if profile is not None:
        return getattr(profile, "name", cli)
    return cli


@dataclass
class Agent:
    name: str
    team_name: str
    pane_id: str
    model: str = ""
    prompt: str = ""
    cwd: str = field(default_factory=os.getcwd)
    session_id: str | None = None
    spawned_at: float = field(default_factory=time.time)
    cli: str = "droid"

    # --- Lifecycle ---

    @classmethod
    def spawn(
        cls,
        name: str,
        team_name: str,
        target_pane: str,
        model: str = "",
        prompt: str = "",
        cwd: str = "",
        session_id: str | None = None,
        is_first: bool = False,
        split_horizontal: bool = True,
        split_size: str | None = None,
        split_window: bool = True,
        skill: str = "hive",
        extra_env: dict[str, str] | None = None,
        cli: str = "droid",
    ) -> Agent:
        """Spawn an agent CLI (droid/claude/codex) in a tmux pane.

        If split_window is True (default), splits *target_pane* and runs the
        CLI in the new pane. If False, runs the CLI in *target_pane* itself
        (target must be a shell pane, not already running an agent).
        """
        if cli not in CLI_BINS:
            raise ValueError(f"unsupported cli '{cli}', must be one of: {', '.join(CLI_BINS)}")
        cwd = cwd or os.getcwd()
        if not tmux.is_inside_tmux():
            raise ValueError(_TMUX_REQUIRED_MESSAGE)

        from .agent_cli import get_profile
        profile = get_profile(cli)
        ready_text = profile.ready_text if profile else "for help"

        resolved_model = model

        if split_window:
            pane_id = tmux.split_window(target_pane, horizontal=split_horizontal, size=split_size)
        else:
            pane_id = target_pane
        tmux.set_pane_title(pane_id, f"[{name}]")
        tmux.tag_pane(pane_id, "agent", name, team_name, cli=cli)

        bin_path = CLI_BINS[cli]
        cmd_parts = ["exec", _shell_escape(bin_path)]
        if cli == "codex":
            cmd_parts.extend(["-c", "check_for_update_on_startup=false"])
        pre_cmd_parts: list[str] = []

        if model and not session_id:
            if cli == "droid":
                json_str, resolved_model = _build_droid_model_settings(model)
                if json_str:
                    pre_cmd_parts.extend([
                        'settings_file=$(mktemp "${TMPDIR:-/tmp}/hive-droid-settings.XXXXXX")',
                        f"printf '%s' {_shell_escape(json_str)} > \"$settings_file\"",
                    ])
                    cmd_parts.extend(["--settings", "\"$settings_file\""])
            elif cli == "claude":
                cmd_parts.extend(["--model", _shell_escape(model)])
            elif cli == "codex":
                cmd_parts.extend(["-m", _shell_escape(model)])

        # Resume uses the original session's model; no --model flag needed.
        if session_id:
            if cli == "droid":
                cmd_parts.extend(["-r", _shell_escape(session_id)])
            elif cli == "claude":
                cmd_parts.extend(["-r", _shell_escape(session_id), "--fork-session"])
            elif cli == "codex":
                cmd_parts = ["exec", _shell_escape(bin_path), "-c", "check_for_update_on_startup=false", "fork", _shell_escape(session_id)]

        env_parts: list[str] = []
        if extra_env:
            for k, v in extra_env.items():
                env_parts.append(f"{k}={_shell_escape(v)}")

        cmd = f"cd {_shell_escape(cwd)}"
        if env_parts:
            cmd = f"{cmd} && export {' '.join(env_parts)}"
        if pre_cmd_parts:
            cmd = f"{cmd} && {' && '.join(pre_cmd_parts)}"
        cmd = f"{cmd} && {' '.join(cmd_parts)}"
        tmux.send_keys(pane_id, cmd)

        agent = cls(
            name=name,
            team_name=team_name,
            pane_id=pane_id,
            model=model,
            prompt=prompt,
            cwd=cwd,
            session_id=session_id,
            cli=cli,
        )

        if tmux.wait_for_text(pane_id, ready_text, timeout=AGENT_STARTUP_TIMEOUT):
            if cli == "droid":
                detected_session = resolve_session_id_for_pane(pane_id)
                if detected_session:
                    agent.session_id = detected_session

            time.sleep(1)

            if skill and skill != "none":
                agent.load_skill(skill)

            if prompt:
                _submit_interactive_text(pane_id, prompt, cli)

        return agent

    # --- Control ---

    def send(self, text: str) -> None:
        """Send a prompt to the agent TUI."""
        _submit_interactive_text(self.pane_id, text, self.cli)

    def load_skill(self, skill_name: str) -> None:
        """Load a skill in the pane using the CLI-specific command.

        Uses raw `tmux.send_keys` instead of `_submit_interactive_text` —
        skill loading happens at spawn time on a fresh pane with no user
        draft to preserve, and the draft-guard placeholder detection can
        misidentify CLI placeholder hints (e.g. codex's rotating
        `Find and fix a bug in @filename`) as drafts and paste them back
        as real input after the skill submits.
        """
        if not skill_name or skill_name == "none":
            return
        if skill_name == "hive":
            skill_sync.maybe_warn_hive_skill_drift(self.cli)
        from .agent_cli import get_profile
        profile = get_profile(self.cli)
        text = profile.skill_cmd.format(name=skill_name) if profile else f"/{skill_name}"
        # Type + wait for picker to open; codex skill picker = 2 Enters
        # (pick entry, then submit), others = 1.
        tmux.send_keys(self.pane_id, text, enter=False)
        time.sleep(0.1)
        for _ in range(2 if self.cli == "codex" else 1):
            tmux.send_key(self.pane_id, "Enter")
        time.sleep(2)

    def interrupt(self) -> None:
        """Press Escape to interrupt."""
        tmux.send_key(self.pane_id, "Escape")

    def capture(self, lines: int = 50) -> str:
        """Capture pane output."""
        return tmux.capture_pane(self.pane_id, lines)

    def is_alive(self) -> bool:
        return tmux.is_pane_alive(self.pane_id)

    def shutdown(self) -> None:
        """Send Ctrl+C twice then exit."""
        tmux.send_key(self.pane_id, "C-c")
        time.sleep(0.5)
        tmux.send_key(self.pane_id, "C-c")
        time.sleep(0.5)
        tmux.send_keys(self.pane_id, "exit")

    def kill(self) -> None:
        """Force kill the pane."""
        tmux.kill_pane(self.pane_id)

    # --- Serialization ---

    def to_dict(self) -> dict:
        return {
            "agentId": f"{self.name}@{self.team_name}",
            "name": self.name,
            "model": self.model,
            "prompt": self.prompt,
            "cwd": self.cwd,
            "tmuxPaneId": self.pane_id,
            "sessionId": self.session_id,
            "spawnedAt": self.spawned_at,
            "isActive": self.is_alive(),
            "cli": self.cli,
        }
