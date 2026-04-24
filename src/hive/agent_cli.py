"""Agent CLI profiles: droid, claude, codex."""

from __future__ import annotations

import os
from dataclasses import dataclass

from . import adapters
from . import tmux

AGENT_CLI_NAMES = frozenset({"droid", "claude", "codex"})
SHELL_NAMES = frozenset({"zsh", "bash", "fish", "sh", "dash", "ksh", "tcsh", "csh"})
CLI_ALIASES = {
    "claude-code": "claude",
    "claudecode": "claude",
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
    prefix, common aliases, and bare provider labels like 'anthropic' /
    'openai' (useful when Factory customModels store provider in its own
    field and the model/id string does not carry the family token).
    """
    if not model:
        return "unknown"
    m = model.lower().strip()
    if m.startswith("custom:"):
        m = m[len("custom:"):]
    m = m.lstrip("-")
    if "anthropic" in m or "claude" in m or m.startswith(("opus", "sonnet", "haiku")):
        return "anthropic"
    if "openai" in m or "codex" in m or m.startswith(("gpt", "o1", "o3", "o4")):
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


# Environment-variable overrides keyed by "orch family". The env var names
# describe the *peer* family (i.e. orch=anthropic → OPENAI peer). When set
# they skip the settings-driven selector below so users can pin a pair by
# hand without editing code or ~/.factory/settings.json.
_DROID_PEER_ENV: dict[str, tuple[str, str]] = {
    "anthropic": ("HIVE_DROID_PEER_OPENAI_MODEL", "HIVE_DROID_PEER_OPENAI_EFFORT"),
    "openai": ("HIVE_DROID_PEER_ANTHROPIC_MODEL", "HIVE_DROID_PEER_ANTHROPIC_EFFORT"),
}

# Relative rank for reasoningEffort strings. Higher = deeper thinking. Hive
# picks the peer with the highest rank among candidates. Anything unknown
# falls to the bottom so an explicit "max" or "xhigh" always wins over an
# empty / misspelled value.
_EFFORT_RANK: dict[str, int] = {
    "max": 5,
    "xhigh": 4,
    "high": 3,
    "medium": 2,
    "low": 1,
    "minimal": 0,
}

_OPPOSITE_FAMILY = {"anthropic": "openai", "openai": "anthropic"}


def _effort_rank(effort: str) -> int:
    return _EFFORT_RANK.get((effort or "").strip().lower(), -1)


def _candidate_family(entry: dict) -> str:
    """Classify a customModels entry by inspecting its named fields.

    Droid's ``customModels[]`` stores the authoritative backend model name in
    ``model``; ``id`` is the Factory handle (often ``custom:`` prefixed) and
    ``displayName`` is free-form. ``provider`` is the final fallback — it is
    the coarsest signal but still distinguishes hosted-proxy setups that do
    not leak the family token anywhere else.
    """
    for key in ("model", "id", "displayName", "provider"):
        value = entry.get(key)
        if isinstance(value, str) and value:
            family = classify_model_family(value)
            if family != "unknown":
                return family
    return "unknown"


def select_droid_peer_from_settings(
    orch_family: str,
    settings: dict | None,
) -> tuple[str, str] | None:
    """Pick the best droid peer (model_id, effort) from ``customModels``.

    Rules:
      1. Only consider entries on the opposite family of ``orch_family``
         (anthropic ↔ openai). Kimi / glm / unknown slots are ignored so
         the hive two-family peer invariant holds.
      2. Sort candidates by (effort_rank desc, maxOutputTokens desc,
         index asc). The first winner's ``id`` + effort is returned.
      3. Effort resolution per entry: own ``reasoningEffort`` first, else
         the global ``sessionDefaultSettings.reasoningEffort``, else ``""``.
         An empty effort still competes but loses ties to a peer with an
         explicit high value.
      4. Returns ``None`` when nothing qualifies — caller falls back to the
         legacy anti-family CLI flow instead of silently cloning orch.
    """
    target_family = _OPPOSITE_FAMILY.get(orch_family)
    if target_family is None:
        return None
    if not isinstance(settings, dict):
        return None
    models = settings.get("customModels")
    if not isinstance(models, list):
        return None

    global_effort = ""
    defaults = settings.get("sessionDefaultSettings")
    if isinstance(defaults, dict):
        raw = defaults.get("reasoningEffort")
        if isinstance(raw, str):
            global_effort = raw

    best: tuple[int, int, int, str, str] | None = None
    for entry in models:
        if not isinstance(entry, dict):
            continue
        model_id = entry.get("id") or entry.get("model")
        if not isinstance(model_id, str) or not model_id:
            continue
        if _candidate_family(entry) != target_family:
            continue
        effort_raw = entry.get("reasoningEffort")
        effort = effort_raw if isinstance(effort_raw, str) and effort_raw else global_effort
        rank = _effort_rank(effort)
        max_tokens = int(entry.get("maxOutputTokens") or 0)
        idx_raw = entry.get("index")
        idx = int(idx_raw) if isinstance(idx_raw, int) else 9999
        key = (rank, max_tokens, -idx, model_id, effort)
        if best is None or key > best:
            best = key
    if best is None:
        return None
    _rank, _tokens, _neg_idx, model_id, effort = best
    return model_id, effort


def droid_peer_plan(
    my_family: str,
    *,
    settings: dict | None = None,
) -> tuple[str, str] | None:
    """Return (model, reasoning_effort) for a droid-as-droid peer.

    Resolution order:
      1. Environment-variable override (``HIVE_DROID_PEER_*_MODEL`` /
         ``_EFFORT``). Effort defaults to ``""`` when only the model var
         is set, leaving droid's own default in play.
      2. ``select_droid_peer_from_settings(my_family, settings)``.
      3. ``None`` — caller falls back to the legacy anti-family CLI route.

    Only ``anthropic`` ↔ ``openai`` are mapped today (returns ``None`` for
    anything else) to match the hive "two-family peer review" invariant.
    """
    if my_family not in _DROID_PEER_ENV:
        return None
    env_model_key, env_effort_key = _DROID_PEER_ENV[my_family]
    env_model = os.environ.get(env_model_key, "").strip()
    env_effort = os.environ.get(env_effort_key, "").strip()
    if env_model:
        return env_model, env_effort

    return select_droid_peer_from_settings(my_family, settings)


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
