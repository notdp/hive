"""Agent CLI profiles: droid, claude, codex."""

from __future__ import annotations

import json
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from . import adapters
from . import settings as user_settings
from . import tmux

AGENT_CLI_NAMES = frozenset({"droid", "claude", "codex"})
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
# - `hive gang init` to pick skeptic's CLI
# - `hive init` peer discovery / spawn fallback
_ANTI_PEER_CLI = {"claude": "codex", "codex": "claude", "droid": "claude"}


def anti_peer_cli(current_cli: str) -> str:
    """Return the anti-family peer CLI for *current_cli* (claude↔codex; droid→claude).

    droid wraps arbitrary models; default peer = claude. Callers that know
    droid is running an Anthropic model (opus/sonnet) should override with
    'codex' explicitly.
    """
    return _ANTI_PEER_CLI.get(current_cli, "claude")


def classify_model_family(model: str) -> str:
    """Classify a model identifier into a coarse family for peer diversity.

    Returns 'anthropic', 'openai', or 'unknown'. Handles droid's 'custom:'
    prefix and common aliases.
    """
    if not model:
        return "unknown"
    m = model.lower().strip()
    if m.startswith("custom:"):
        m = m[len("custom:"):]
    m = m.lstrip("-")
    if "claude" in m or m.startswith(("opus", "sonnet", "haiku")):
        return "anthropic"
    if "codex" in m or m.startswith(("gpt", "o1", "o3", "o4")):
        return "openai"
    return "unknown"


def family_for_pane(pane_id: str) -> str:
    """Best-effort classify the agent pane's model family.

    Reads model via resolve_model_for_pane; falls back to CLI identity when
    the model is unavailable (claude→anthropic, codex→openai, droid→unknown).
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


# Cross-family preference list, strongest first. Match the entry's ``model``
# field exactly. To rank a new model, add it in the right slot. If a real
# customModels entry isn't in this list, it gets ignored (selfPeer falls
# back to claude/codex).
_CROSS_FAMILY_RANKING: dict[str, list[str]] = {
    "anthropic": [
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-opus-4-5-20251101",
        "claude-sonnet-4-6",
        "claude-sonnet-4-5-20250929",
        "claude-haiku-4-5-20251001",
    ],
    "openai": [
        "gpt-5.5",
        "gpt-5.4",
        "gpt-5.3-codex",
        "gpt-5.2-codex",
        "gpt-5.2",
        "gpt-5",
    ],
}


def _load_factory_settings() -> dict[str, Any]:
    factory_home = Path(os.environ.get("FACTORY_HOME", str(Path.home() / ".factory")))
    path = factory_home / "settings.json"
    try:
        text = path.read_text()
    except OSError:
        return {}
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        return {}
    return data if isinstance(data, dict) else {}


def _load_factory_custom_models() -> list[dict[str, Any]]:
    models = _load_factory_settings().get("customModels")
    return [m for m in (models or []) if isinstance(m, dict)]


def _factory_uses_managed_default() -> bool:
    """Heuristic for "user has a working Factory managed plan".

    If ``sessionDefaultSettings.model`` is a non-``custom:`` model id, the
    user is actively running droid against Factory's managed catalog —
    proof that the plan covers managed inference. When it's a ``custom:``
    id, missing, or empty, we cannot presume managed access and fall back
    to claude/codex peer instead of guessing.
    """
    default_model = (_load_factory_settings().get("sessionDefaultSettings") or {}).get("model")
    if not isinstance(default_model, str) or not default_model:
        return False
    return not default_model.startswith("custom:")


def pick_droid_cross_family_model(
    lead_family: str,
    custom_models: list[dict[str, Any]] | None = None,
) -> str | None:
    """Pick a cross-family model id for a droid peer spawn.

    1. Walks ``_CROSS_FAMILY_RANKING[opposite_family]`` and returns the
       **id** of the first BYOK customModels entry whose ``model`` field
       matches (e.g. ``custom:Claude-Opus-4.7-0``).
    2. If no BYOK match AND the user's Factory ``sessionDefaultSettings.model``
       is a managed (non-``custom:``) id — proof the plan covers managed
       inference — falls back to the **top of the ranking list** as a
       plain model id (e.g. ``gpt-5.5``).
    3. Otherwise returns ``None`` so the caller can fall through to the
       claude/codex peer instead of spawning a managed peer the user has
       no plan to support.
    """
    if lead_family not in {"anthropic", "openai"}:
        return None
    target_family = "openai" if lead_family == "anthropic" else "anthropic"
    ranking = _CROSS_FAMILY_RANKING.get(target_family, [])
    if not ranking:
        return None

    by_model: dict[str, str] = {}
    entries = custom_models if custom_models is not None else _load_factory_custom_models()
    for entry in entries:
        if (entry.get("provider") or "").lower() != target_family:
            continue
        model = entry.get("model") or ""
        entry_id = entry.get("id")
        if model and entry_id and model not in by_model:
            by_model[model] = str(entry_id)

    for model_name in ranking:
        if model_name in by_model:
            return by_model[model_name]

    # Managed fallback: only when the user's sessionDefaultSettings already
    # points at a managed (non-custom:) model — that's our signal that the
    # Factory plan covers managed inference. Otherwise we'd silently spawn
    # a peer the user has no plan to support; better to fall through to
    # claude/codex peer.
    if _factory_uses_managed_default():
        return ranking[0]
    return None


_DROID_SELF_PEER_ENV = "HIVE_DROID_SELF_PEER"
_TRUTHY = {"1", "true", "yes", "on"}


def _droid_self_peer_enabled() -> bool:
    """Resolve the droid self-peer toggle.

    ``HIVE_DROID_SELF_PEER`` env var (truthy: 1/true/yes/on) takes precedence
    over the ``droid.selfPeer`` key in ``~/.hive/settings.json``. **Default on**:
    droid leads spawn droid peers unless explicitly disabled via env or
    ``hive config set droid.selfPeer false``.
    """
    raw = os.environ.get(_DROID_SELF_PEER_ENV)
    if raw is not None:
        return raw.strip().lower() in _TRUTHY
    return bool(user_settings.get_setting("droid.selfPeer", True))


def resolve_peer_spawn(
    *,
    my_cli: str,
    my_family: str,
    custom_models: list[dict[str, Any]] | None = None,
) -> tuple[str, str]:
    """Decide ``(peer_cli, peer_model)`` for an anti-family peer spawn.

    Default behavior is unchanged: ``peer_cli_for_family(my_family)`` with an
    empty model (caller's CLI default applies). When the lead is droid AND
    selfPeer is enabled (env or settings), returns ``("droid", "<id-or-managed>")``
    so the peer also runs droid.
    """
    default = (peer_cli_for_family(my_family), "")
    if my_cli != "droid":
        return default
    if not _droid_self_peer_enabled():
        return default
    model_id = pick_droid_cross_family_model(my_family, custom_models)
    if not model_id:
        return default
    return ("droid", model_id)


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
    resume_cmd: str
    skill_cmd: str


PROFILES: dict[str, CLIProfile] = {
    "droid": CLIProfile(
        name="droid",
        ready_text="for help",
        resume_cmd="droid --fork {session_id}",
        skill_cmd="/{name}",
    ),
    "claude": CLIProfile(
        name="claude",
        ready_text="Claude Code",
        resume_cmd="claude -r {session_id} --fork-session",
        skill_cmd="/{name}",
    ),
    "codex": CLIProfile(
        name="codex",
        ready_text="OpenAI Codex",
        resume_cmd="codex fork {session_id}",
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
        profile = detect_profile_from_text(process.argv)
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
    if not session_id and profile.name == "codex":
        # Born-connected codex runs a per-pane app-server daemon; the session
        # jsonl is held by that daemon, not by any process in the pane's tty
        # tree, so the lsof-based resolver above can never see it. Ask the
        # daemon directly. Manual/embedded codex has no daemon and returns None.
        from .adapters import codex_app_server

        session_id = codex_app_server.session_id_for_pane(pane_id)
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
