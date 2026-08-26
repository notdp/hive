"""Agent CLI profiles: claude, codex, grok."""

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

AGENT_CLI_NAMES = frozenset({"claude", "codex", "grok"})

def _catalog_model_ids(cli: str) -> list[str]:
    """Model ids from the CLI's own backend catalog on disk; [] = no catalog.

    codex and grok refresh these caches from their backends themselves, so
    the list never drifts the way a hand-maintained table did. claude keeps
    no local catalog — its aliases and ids are validated by the CLI itself.
    """
    try:
        if cli == "codex":
            from .adapters.codex_app_server import codex_home

            data = json.loads((codex_home() / "models_cache.json").read_text())
            return [str(m["slug"]) for m in data.get("models", []) if m.get("slug")]
        if cli == "grok":
            from .adapters.grok_leader import grok_home

            data = json.loads((grok_home() / "models_cache.json").read_text())
            return [str(k) for k in data.get("models", {})]
    except (OSError, ValueError, KeyError, TypeError):
        return []
    return []


_CLI_FAMILY = {"claude": "anthropic", "codex": "openai", "grok": "xai"}


def validate_spawn_model(cli: str, model: str) -> str | None:
    """Error string when *model* is surely wrong for *cli*, else None.

    Two gates: a cross-family check (a gpt model handed to claude is always
    a mistake, catalog or not), then the CLI's own catalog when one exists
    on disk. No catalog or an unreadable cache fails open — the CLI is the
    final authority and its own rejection is visible in the pane (claude
    keeps no local catalog but rejects unknown ids itself at launch).
    """
    if not model:
        return None
    family = classify_model_family(model)
    cli_family = _CLI_FAMILY.get(cli)
    if family != "unknown" and cli_family and family != cli_family:
        return (
            f"model '{model}' is a {family} model, but {cli} runs "
            f"{cli_family} models — wrong --cli or wrong -m"
        )
    known = _catalog_model_ids(cli)
    if not known or model in known:
        return None
    import difflib

    close = difflib.get_close_matches(model, known, n=1, cutoff=0.4)
    hint = f" (did you mean '{close[0]}'?)" if close else ""
    return (
        f"unknown {cli} model '{model}'{hint}; "
        f"its catalog has: {', '.join(known)}"
    )


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
# OpenAI vs xAI) produce more diverse viewpoints than same-family pairs. Used by:
# - anti-family spawn suggestions (heterogeneous review)
# - `hive init` peer discovery / spawn fallback
_ANTI_PEER_CLI = {"claude": "codex", "codex": "claude", "grok": "claude"}


def anti_peer_cli(current_cli: str) -> str:
    """Return the anti-family peer CLI for *current_cli* (claude↔codex, grok→claude)."""
    return _ANTI_PEER_CLI.get(current_cli, "claude")


def classify_model_family(model: str) -> str:
    """Classify a model identifier into a coarse family for peer diversity.

    Returns 'anthropic', 'openai', 'xai', or 'unknown'.
    """
    if not model:
        return "unknown"
    m = model.lower().strip()
    m = m.lstrip("-")
    if "claude" in m or m.startswith(("opus", "sonnet", "haiku")):
        return "anthropic"
    if "codex" in m or m.startswith(("gpt", "o1", "o3", "o4")):
        return "openai"
    if "grok" in m:
        return "xai"
    return "unknown"


def family_for_pane(pane_id: str) -> str:
    """Best-effort classify the agent pane's model family.

    Reads model via resolve_model_for_pane; falls back to CLI identity when
    the model is unavailable (claude→anthropic, codex→openai, grok→xai).
    """
    profile = detect_profile_for_pane(pane_id)
    if not profile:
        return "unknown"
    model = resolve_model_for_pane(pane_id, cli_name=profile.name)
    family = classify_model_family(model)
    if family != "unknown":
        return family
    return _CLI_FAMILY.get(profile.name, "unknown")


def peer_cli_for_family(my_family: str) -> str:
    """CLI to spawn as an anti-family peer when my family is *my_family*."""
    if my_family == "anthropic":
        return "codex"
    if my_family in ("openai", "xai"):
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
        fork_cmd="hive claude -r {session_id} --fork-session",
        skill_cmd="/{name}",
    ),
    "codex": CLIProfile(
        name="codex",
        ready_text="OpenAI Codex",
        fork_cmd="hive codex fork {session_id}",
        skill_cmd="${name}",
    ),
    "grok": CLIProfile(
        name="grok",
        ready_text="Shift+Tab:mode",
        fork_cmd="hive grok --resume {session_id} --fork-session",
        skill_cmd="/{name}",
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


# Script runtimes whose argv[1] is the launched CLI's entry script — the one
# verified wrapper shape (codex runs as `node /.../codex ...`). Anything else
# in argv is ordinary argument text and must never identify a CLI.
_SCRIPT_RUNTIMES = frozenset({"node"})


def detect_profile_from_process(command: str, argv: str) -> CLIProfile | None:
    """CLI identity from process fields, not argument text.

    Matches the executable itself (ps comm / argv[0]) or the verified script
    runtime shape ``node <.../codex|claude> ...``. Later argv tokens are the
    process's own arguments — ``rg codex src`` is a search, not a CLI — so
    they are never scanned.
    """
    profile = get_profile(command)
    if profile:
        return profile
    try:
        parts = shlex.split(argv or "")
    except ValueError:
        parts = (argv or "").split()
    if not parts:
        return None
    profile = get_profile(parts[0])
    if profile:
        return profile
    if len(parts) >= 2 and normalize_command(parts[0]) in _SCRIPT_RUNTIMES:
        return get_profile(parts[1])
    return None


def detect_cli_process_for_pane(pane_id: str) -> CLIProfile | None:
    """CLI profile from live agent evidence only — never the pane title.

    A retained shell keeps the pane (and often a stale title naming a CLI)
    after the agent process exits, so title text must not count as liveness
    evidence. Evidence is the pane's current command and its TTY process
    table, parsed by the same matchers as :func:`detect_profile_for_pane` —
    plus, for claude, the pane's bg job record: a claude member's engine runs
    on claude's own supervisor, and the pane only shows it through an attach
    viewer, so a viewer gap (reattach window, closed viewer) with a live
    engine still counts as a live claude. Any probe failure fails closed to
    None.
    """
    try:
        profile = detect_profile_from_pane_command(tmux.get_pane_current_command(pane_id) or "")
        if profile:
            return profile
        tty = tmux.get_pane_tty(pane_id) or ""
        for process in tmux.list_tty_processes(tty):
            profile = detect_profile_from_process(process.command, process.argv)
            if profile:
                return profile
        from .adapters.claude_bg import pane_engine_alive

        if pane_engine_alive(pane_id):
            return PROFILES["claude"]
    except Exception:
        return None
    return None


def claude_pid_for_pane(pane_id: str) -> int | None:
    """Pid of the live claude process on *pane_id*'s tty (process evidence
    only, same matchers as :func:`detect_cli_process_for_pane`).

    On a bg-member pane this is the attach *viewer*'s pid — never member
    identity or delivery routing (both key on the pane's job record). It
    answers only tty-scoped questions: is there a viewer to keystroke into,
    or an interactive (non-member) claude session on this pane.
    """
    try:
        tty = tmux.get_pane_tty(pane_id) or ""
        for process in tmux.list_tty_processes(tty):
            profile = detect_profile_from_process(process.command, process.argv)
            if profile and profile.name == "claude":
                return int(process.pid)
    except Exception:
        return None
    return None


def detect_profile_for_pane(pane_id: str) -> CLIProfile | None:
    profile = detect_cli_process_for_pane(pane_id)
    if profile:
        return profile
    return detect_profile_from_text(tmux.get_pane_title(pane_id) or "")


def member_role_for_pane(pane_id: str) -> str:
    if detect_profile_for_pane(pane_id):
        return "agent"
    # A pane bound to a bg job is an agent pane even while its engine is
    # parked (asleep is not dead): the lead's role — and with it runtime
    # ticks and idle notify — must not ride the viewer's life.
    try:
        from .adapters.claude_bg import job_id_for_pane

        if job_id_for_pane(pane_id):
            return "agent"
    except Exception:
        pass
    return member_role(tmux.get_pane_current_command(pane_id) or "")


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
