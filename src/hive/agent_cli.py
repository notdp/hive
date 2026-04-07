"""Agent CLI profiles: droid, claude, codex."""

from __future__ import annotations

import json
import re
from dataclasses import dataclass
from pathlib import Path

from . import core_hooks
from . import tmux

AGENT_CLI_NAMES = frozenset({"droid", "claude", "codex"})
SHELL_NAMES = frozenset({"zsh", "bash", "fish", "sh", "dash", "ksh", "tcsh", "csh"})
CLI_ALIASES = {
    "claude-code": "claude",
    "claudecode": "claude",
}


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
    fork_cmd: str | None
    fork_needs_tui: bool
    skill_cmd: str
    extra_enter: bool = False


PROFILES: dict[str, CLIProfile] = {
    "droid": CLIProfile(
        name="droid",
        ready_text="for help",
        resume_cmd="droid -r {session_id}",
        fork_cmd="/fork",
        fork_needs_tui=True,
        skill_cmd="/{name}",
    ),
    "claude": CLIProfile(
        name="claude",
        ready_text="for help",
        resume_cmd="claude -r {session_id} --fork-session",
        fork_cmd=None,
        fork_needs_tui=False,
        skill_cmd="/{name}",
    ),
    "codex": CLIProfile(
        name="codex",
        ready_text="for help",
        resume_cmd="codex fork {session_id}",
        fork_cmd=None,
        fork_needs_tui=False,
        skill_cmd="${name}",
        extra_enter=True,
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


def _read_json_file(path: Path) -> dict[str, object] | None:
    try:
        data = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return None
    return data if isinstance(data, dict) else None


def _path_mtime(path: Path) -> float:
    try:
        return path.stat().st_mtime
    except OSError:
        return -1


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


def _resolve_droid_session_id(pane_id: str) -> str | None:
    record = core_hooks.resolve_session_record(
        pane_id=pane_id,
        tty=tmux.get_pane_tty(pane_id) or "",
    )
    if not record:
        return None
    session_id = record.get("session_id")
    return str(session_id) if session_id else None


def _resolve_claude_session_id(pane_id: str) -> str | None:
    sessions_dir = Path.home() / ".claude" / "sessions"
    tty = tmux.get_pane_tty(pane_id) or ""
    for process in tmux.list_tty_processes(tty):
        if normalize_command(process.command) != "claude":
            continue
        payload = _read_json_file(sessions_dir / f"{process.pid}.json")
        if not payload:
            continue
        session_id = payload.get("sessionId")
        if session_id:
            return str(session_id)
    return None


def _read_codex_session_meta(path: Path) -> tuple[str, str] | None:
    try:
        with path.open() as handle:
            line = handle.readline().strip()
    except OSError:
        return None
    if not line:
        return None
    try:
        payload = json.loads(line)
    except json.JSONDecodeError:
        return None
    if payload.get("type") != "session_meta":
        return None
    body = payload.get("payload")
    if not isinstance(body, dict):
        return None
    session_id = body.get("id")
    cwd = body.get("cwd")
    if not session_id or not cwd:
        return None
    return str(session_id), str(cwd)


_CODEX_SESSION_UUID_RE = re.compile(
    r"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$"
)


def _resolve_codex_session_id(pane_id: str) -> str | None:
    # 1. Try session-map (backward compat with already-registered hooks).
    record = core_hooks.resolve_session_record(
        pane_id=pane_id,
        tty=tmux.get_pane_tty(pane_id) or "",
    )
    if record:
        session_id = record.get("session_id")
        if session_id:
            return str(session_id)

    # 2. Try lsof: find open .jsonl in ~/.codex/sessions/ for the codex pid.
    sessions_prefix = str(Path.home() / ".codex" / "sessions") + "/"
    tty = tmux.get_pane_tty(pane_id) or ""
    for process in tmux.list_tty_processes(tty):
        if normalize_command(process.command) != "codex":
            continue
        for fpath in tmux.list_open_files(process.pid):
            if not fpath.startswith(sessions_prefix) or not fpath.endswith(".jsonl"):
                continue
            match = _CODEX_SESSION_UUID_RE.search(fpath)
            if match:
                return match.group(1)

    # 3. Fallback: scan jsonl files by cwd.
    sessions_dir = Path.home() / ".codex" / "sessions"
    cwd = tmux.display_value(pane_id, "#{pane_current_path}") or ""
    if not cwd or not sessions_dir.is_dir():
        return None
    session_files = sorted(sessions_dir.rglob("*.jsonl"), key=_path_mtime, reverse=True)
    for path in session_files:
        meta = _read_codex_session_meta(path)
        if meta and meta[1] == cwd:
            return meta[0]
    return None


def resolve_session_id_for_pane(pane_id: str, profile: CLIProfile | None = None) -> str | None:
    resolved_profile = profile or detect_profile_for_pane(pane_id)
    if not resolved_profile:
        return None
    if resolved_profile.name == "droid":
        return _resolve_droid_session_id(pane_id)
    if resolved_profile.name == "claude":
        return _resolve_claude_session_id(pane_id)
    if resolved_profile.name == "codex":
        return _resolve_codex_session_id(pane_id)
    return None
