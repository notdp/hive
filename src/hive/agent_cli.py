"""Agent CLI profiles: droid, claude, codex."""

from __future__ import annotations

import os
import re
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
    field and the model/id string does not carry the family token). Legacy
    OpenAI codenames (o1/o2/o3/o4) are intentionally not matched because
    they have been rebranded to the gpt-x.y numbering and treating the bare
    letter "o" as a family token causes false positives on unrelated names.
    """
    if not model:
        return "unknown"
    m = model.lower().strip()
    if m.startswith("custom:"):
        m = m[len("custom:"):]
    m = m.lstrip("-")
    if "anthropic" in m or "claude" in m or m.startswith(("opus", "sonnet", "haiku")):
        return "anthropic"
    if "openai" in m or "codex" in m or m.startswith("gpt"):
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

# Coarse Claude tier hierarchy. Opus > Sonnet > Haiku across every generation,
# so a newer Sonnet never beats an older Opus in peer selection. Version
# numbers are used as a secondary tie-breaker (see ``_model_tier``).
_CLAUDE_TIER: dict[str, float] = {
    "opus": 3.0,
    "sonnet": 2.0,
    "haiku": 1.0,
}

# Two leading-version extractors:
#
#   _DOT_VERSION_PATTERN  — only the first ``major[.minor]`` via ``.``. Used
#     for OpenAI and any callsite where a trailing ``-<digits>`` would be a
#     Factory registration suffix, not a model minor version. Example:
#     ``custom:GPT-5-9`` must stay ``5.0`` (the ``-9`` is the slot index).
#
#   _DASH_VERSION_PATTERN — accepts both ``.`` and ``-`` as the minor
#     separator. Used only for Anthropic Claude lineage, because Factory's
#     customModels stores Claude versions hyphen-separated in the ``model``
#     field (e.g. ``claude-opus-4-7``).
#
# Both stop after the minor segment on purpose: a third component
# (``gpt-5.1.3``, ``claude-opus-4.7.1``) used to break ``float()`` and
# silently collapse the tier to 0.0, which misranked patch releases. Now
# we only keep major.minor and drop the patch tail.
_DOT_VERSION_PATTERN = re.compile(r"(\d+)(?:\.(\d+))?")
_DASH_VERSION_PATTERN = re.compile(r"(\d+)(?:[.-](\d+))?")

_CLAUDE_LINEAGE_PATTERN = re.compile(r"claude|opus|sonnet|haiku")


def _effort_rank(effort: str) -> int:
    return _EFFORT_RANK.get((effort or "").strip().lower(), -1)


def _version_from(pattern: re.Pattern[str], text: str) -> float:
    """Run *pattern* against *text* and return the first major[.minor] as float.

    The pattern must expose group(1)=major and group(2)=optional minor.
    Patch tails (group(3)+) are deliberately ignored so ``gpt-5.1.3`` ->
    5.1 rather than raising ``ValueError`` inside ``float("5.1.3")`` and
    silently collapsing to 0.0.
    """
    match = pattern.search(text or "")
    if not match:
        return 0.0
    major_str = match.group(1)
    minor_str = match.group(2)
    try:
        if minor_str is not None:
            return float(f"{major_str}.{minor_str}")
        return float(major_str)
    except ValueError:
        return 0.0


def _extract_leading_version(text: str) -> float:
    """Best-effort major[.minor] version extractor.

    Uses dash-minor parsing only when *text* names a Claude lineage
    (``claude``/``opus``/``sonnet``/``haiku``). For everything else — OpenAI
    identifiers, opaque proxy names, Factory handle ids such as
    ``custom:GPT-5-9`` — the trailing ``-<digits>`` is treated as a Factory
    registration suffix, not as a version component, so ``gpt-5-9`` stays
    at 5.0 and loses to ``gpt-5.5``.

    Examples::

      gpt-5                    -> 5.0
      gpt-5.5                  -> 5.5
      gpt-5.1.3                -> 5.1  (patch segment dropped)
      gpt-5.3-codex            -> 5.3
      custom:GPT-5-9           -> 5.0  (``-9`` is a slot index, not minor)
      claude-opus-4-7          -> 4.7  (Claude lineage -> dash = minor)
      claude-opus-4.7.1        -> 4.7
      custom:Claude-Opus-4.7-0 -> 4.7

    Returns ``0.0`` when nothing numeric is present.
    """
    lowered = (text or "").lower()
    pattern = (
        _DASH_VERSION_PATTERN
        if _CLAUDE_LINEAGE_PATTERN.search(lowered)
        else _DOT_VERSION_PATTERN
    )
    return _version_from(pattern, lowered)


def _model_tier(family: str, raw: str) -> float:
    """Return a numeric tier score for ranking candidates of the same family.

    Anthropic: Opus=3.x, Sonnet=2.x, Haiku=1.x. The fractional part is the
    first version number encountered, clamped to ``[0, 1)`` so a wildly
    large Haiku version (``haiku-99``) can never leak past the Sonnet
    floor. Unknown lineage in the anthropic family falls back to 0.x with
    the version included, so it still ranks below any recognised lineage.

    OpenAI: returns the raw version number found in the string (gpt-5.5 ->
    5.5, gpt-5.3-codex -> 5.3). Bare ``gpt`` without a version falls to
    ``0`` but is still a valid candidate.

    All matching is case-insensitive because ``raw`` is lowercased before
    inspection.
    """
    lowered = (raw or "").lower()
    if family == "anthropic":
        base = 0.0
        for name, score in _CLAUDE_TIER.items():
            if name in lowered:
                base = score
                break
        version = _extract_leading_version(lowered)
        # Clamp the fractional slot to <1 so haiku-N can never reach sonnet.
        fractional = min(version / 10.0, 0.999)
        return base + fractional
    if family == "openai":
        return _extract_leading_version(lowered)
    return 0.0


def _candidate_tier(entry: dict, family: str) -> float:
    """Pick the most informative field for tier extraction."""
    best = 0.0
    for key in ("model", "id", "displayName"):
        value = entry.get(key)
        if not isinstance(value, str) or not value:
            continue
        tier = _model_tier(family, value)
        if tier > best:
            best = tier
    return best


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
      2. Sort candidates by ``(model_tier, effort_rank, maxOutputTokens,
         -index)``. Tier is the primary axis so Opus-4 always beats
         Sonnet-4 even if Sonnet is configured with a higher effort;
         inside the same tier effort acts as the deciding factor, then
         token budget and registration order.
      3. Effort resolution per entry: own ``reasoningEffort`` first, else
         the global ``sessionDefaultSettings.reasoningEffort``, else ``""``.
         An empty effort still competes but loses ties to a peer with an
         explicit high value.
      4. All string matching (family tokens, effort names, tier keywords)
         is case-insensitive; the inputs are lowercased at each decision
         boundary.
      5. Returns ``None`` when nothing qualifies — caller falls back to the
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

    best: tuple[float, int, int, int, str, str] | None = None
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
        tier = _candidate_tier(entry, target_family)
        key = (tier, rank, max_tokens, -idx, model_id, effort)
        if best is None or key > best:
            best = key
    if best is None:
        return None
    _tier, _rank, _tokens, _neg_idx, model_id, effort = best
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
