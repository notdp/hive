"""Agent CLI profiles: claude, codex."""

from __future__ import annotations

import json
import os
import shlex
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from . import adapters
from . import settings as user_settings
from . import tmux

AGENT_CLI_NAMES = frozenset({"claude", "codex"})

# Non-authoritative suggestions for the interactive picker. Custom values
# always accepted; these just save the user from typing common aliases.
# Sources:
#   claude: https://code.claude.com/docs/en/model-config
#   codex:  https://developers.openai.com/codex/models
MODEL_SUGGESTIONS: dict[str, list[str]] = {
    "claude": [
        "claude-fable-5",
        "claude-opus-4-8",
        "claude-opus-4-7[1m]", "claude-opus-4-7",
        "claude-opus-4-6[1m]", "claude-opus-4-6",
        "claude-sonnet-4-6[1m]", "claude-sonnet-4-6",
    ],
    "codex": ["gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex-spark"],
}
SHELL_NAMES = frozenset({"zsh", "bash", "fish", "sh", "dash", "ksh", "tcsh", "csh"})
CLI_ALIASES = {
    "claude-code": "claude",
    "claudecode": "claude",
    # macOS Claude Code reports its process comm as "claude.exe"; without this
    # the command probe misses and detection falls back to the pane title,
    # which misclassifies a claude pane whose title happens to contain another
    # CLI's name (e.g. "Research Codex app server" -> codex).
    "claude.exe": "claude",
}

# Anti-homogeneous peer CLI mapping. Peers across model families (Anthropic vs
# OpenAI) produce more diverse viewpoints than same-family pairs. Used by:
# - `hive squad init` to pick challenger's CLI
# - `hive init` peer discovery / spawn fallback
_ANTI_PEER_CLI = {"claude": "codex", "codex": "claude"}


def anti_peer_cli(current_cli: str) -> str:
    """Return the anti-family peer CLI for *current_cli* (claude↔codex)."""
    return _ANTI_PEER_CLI.get(current_cli, "claude")


def classify_model_family(model: str) -> str:
    """Classify a model identifier into a coarse family for peer diversity.

    Returns 'anthropic', 'openai', or 'unknown'.
    """
    if not model:
        return "unknown"
    m = model.lower().strip()
    m = m.lstrip("-")
    if "claude" in m or m.startswith(("opus", "sonnet", "haiku")):
        return "anthropic"
    if "codex" in m or m.startswith(("gpt", "o1", "o3", "o4")):
        return "openai"
    return "unknown"


def family_for_pane(pane_id: str) -> str:
    """Best-effort classify the agent pane's model family.

    Reads model via resolve_model_for_pane; falls back to CLI identity when
    the model is unavailable (claude→anthropic, codex→openai).
    """
    profile = detect_profile_for_pane(pane_id)
    if not profile:
        return "unknown"
    model = resolve_model_for_pane(pane_id, cli_name=profile.name)
    family = classify_model_family(model)
    if family != "unknown":
        return family
    if profile.name == "claude":
        return "anthropic"
    if profile.name == "codex":
        return "openai"
    return "unknown"


def peer_cli_for_family(my_family: str) -> str:
    """CLI to spawn as an anti-family peer when my family is *my_family*."""
    if my_family == "anthropic":
        return "codex"
    if my_family == "openai":
        return "claude"
    return "claude"


def normalize_command(command: str) -> str:
    value = (command or "").strip().lower().rsplit("/", 1)[-1]
    value = value.lstrip("-")
    return CLI_ALIASES.get(value, value)


def is_agent_command(command: str) -> bool:
    return normalize_command(command) in AGENT_CLI_NAMES


def is_shell_command(command: str) -> bool:
    return normalize_command(command) in SHELL_NAMES


def member_role(command: str) -> str:
    if is_agent_command(command):
        return "agent"
    return "terminal"


@dataclass(frozen=True)
class CLIProfile:
    name: str
    ready_text: str
    fork_cmd: str
    skill_cmd: str


PROFILES: dict[str, CLIProfile] = {
    "claude": CLIProfile(
        name="claude",
        ready_text="Claude Code",
        fork_cmd="claude -r {session_id} --fork-session",
        skill_cmd="/{name}",
    ),
    "codex": CLIProfile(
        name="codex",
        ready_text="OpenAI Codex",
        fork_cmd="codex fork {session_id}",
        skill_cmd="${name}",
    ),
}

def get_profile(command: str) -> CLIProfile | None:
    return PROFILES.get(normalize_command(command))


def detect_profile_from_pane_command(command: str) -> CLIProfile | None:
    return get_profile(command)


def detect_profile_from_text(text: str) -> CLIProfile | None:
    value = (text or "").strip().lower()
    if not value:
        return None
    if "claude code" in value:
        return PROFILES["claude"]
    for alias, profile_name in CLI_ALIASES.items():
        if alias in value:
            return PROFILES[profile_name]
    for profile_name, profile in PROFILES.items():
        if profile_name in value:
            return profile
    return None


def detect_profile_from_argv(argv: str) -> CLIProfile | None:
    try:
        parts = shlex.split(argv or "")
    except ValueError:
        parts = (argv or "").split()
    for token in parts[:4]:
        profile = get_profile(token)
        if profile:
            return profile
    return None


def detect_profile_for_pane(pane_id: str) -> CLIProfile | None:
    profile = detect_profile_from_pane_command(tmux.get_pane_current_command(pane_id) or "")
    if profile:
        return profile
    profile = detect_profile_from_text(tmux.get_pane_title(pane_id) or "")
    if profile:
        return profile
    tty = tmux.get_pane_tty(pane_id) or ""
    for process in tmux.list_tty_processes(tty):
        profile = detect_profile_from_pane_command(process.command)
        if profile:
            return profile
        profile = detect_profile_from_argv(process.argv)
        if profile:
            return profile
    return None


def member_role_for_pane(pane_id: str) -> str:
    return "agent" if detect_profile_for_pane(pane_id) else member_role(tmux.get_pane_current_command(pane_id) or "")


def resolve_session_id_for_pane(pane_id: str, profile: CLIProfile | None = None) -> str | None:
    resolved_profile = profile or detect_profile_for_pane(pane_id)
    if not resolved_profile:
        return None
    adapter = adapters.get(resolved_profile.name)
    if not adapter:
        return None
    return adapter.resolve_current_session_id(pane_id)


def resolve_model_for_pane(
    pane_id: str,
    *,
    cli_name: str = "",
    current_model: str = "",
) -> str:
    profile = get_profile(cli_name) if cli_name else detect_profile_for_pane(pane_id)
    if not profile:
        return current_model
    adapter = adapters.get(profile.name)
    if not adapter:
        return current_model
    session_id = adapter.resolve_current_session_id(pane_id)
    if not session_id:
        return current_model
    cwd_hint = tmux.display_value(pane_id, "#{pane_current_path}")
    transcript = adapter.find_session_file(session_id, cwd=cwd_hint)
    if transcript is None:
        return current_model
    meta = adapter.read_meta(transcript)
    if meta is None or not meta.model:
        return current_model
    return meta.model
