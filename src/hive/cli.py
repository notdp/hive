"""CLI entry point for hive."""

from __future__ import annotations

import json
import os
import re
import secrets
import shlex
import shutil
import subprocess
import sys
import time
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

import click

from . import bus
from . import context as hive_context
from . import squad_names
from . import notify_ui
from . import plugin_manager
from . import tmux
from .agent import AGENT_STARTUP_TIMEOUT, Agent, _submit_interactive_text
from .agent_cli import AGENT_CLI_NAMES, anti_peer_cli, detect_profile_for_pane, family_for_pane, member_role_for_pane, normalize_command, peer_cli_for_family, resolve_session_id_for_pane
from .team import HIVE_HOME, LEAD_AGENT_NAME, Team


_COMMAND_HELP_SECTIONS = {
    # Daily — per-turn agent collaboration loop.
    "init": "Daily",
    "team": "Daily",
    "send": "Daily",
    "reply": "Daily",
    "notify": "Daily",
    "compact": "Daily",
    "skills": "Daily",
    # Handoff — hand a thread to another pane (same/new/forked).
    "handoff": "Handoff",
    "fork": "Handoff",
    "spawn": "Handoff",
    # Workflow — higher-level flows on top of Hive.
    "squad": "Workflow",
    "duo": "Workflow",
    "worktree": "Workflow",
    "ls": "Workflow",
    "resume": "Workflow",
    # Team — wire up the tmux team around the current window.
    "create": "Team",
    "delete": "Team",
    "register": "Team",
    "peer": "Team",
    "layout": "Team",
    # Human Helpers — human-only popup + split helpers.
    "cvim": "Human Helpers",
    "vim": "Human Helpers",
    "vfork": "Human Helpers",
    "hfork": "Human Helpers",
    # Debug — troubleshooting, rarely on the happy path.
    "doctor": "Debug",
    "thread": "Debug",
    "capture": "Debug",
    "inject": "Debug",
    "interrupt": "Debug",
    "kill": "Debug",
    # Extensions.
    "plugin": "Extensions",
    "config": "Extensions",
    # Launchers — hive-managed claude/codex entry points + shell integration.
    "claude": "Launchers",
    "codex": "Launchers",
    "shell-init": "Launchers",
}
_COMMAND_HELP_SECTION_ORDER = [
    "Daily",
    "Handoff",
    "Workflow",
    "Team",
    "Human Helpers",
    "Debug",
    "Extensions",
    "Launchers",
    "Other Commands",
]
_COMMAND_HELP_SECTION_DESCRIPTIONS = {
    "Daily": "Core loop per turn: inspect context, talk to peers, pull the human in when blocked.",
    "Handoff": "Hand a thread to another worker — same pane, a fresh spawn, or a forked clone.",
    "Workflow": "Higher-level flows on top of Hive: duos, squads, worktrees.",
    "Team": "Create, extend, and wire up the tmux team around the current window.",
    "Human Helpers": "Popup editor and split helpers for the human (not the model). In Claude Code / Codex, type `!hive cvim` via shell escape. Requires tmux >= 3.2.",
    "Debug": "Troubleshoot delivery, runtime state, and low-level pane behavior. Not on the happy path.",
    "Extensions": "Manage first-party Hive plugins (Claude Code, Codex).",
    "Launchers": (
        "hive-managed launchers, usually invoked through the `hive shell-init` shell "
        "integration rather than by hand. All arguments are forwarded verbatim, so "
        "`hive claude --help` shows claude's own help, not this wrapper's."
    ),
}
_ROOT_HELP_EXAMPLES = '''# Team lifecycle
hive init                                    # bind current tmux window as a team
hive team                                    # members + runtime state (busy / inputState / turnPhase)

# Messaging (root thread: body is a short summary, details go in --artifact)
hive send dodo "review this diff" --artifact /tmp/diff.md
hive send dodo "see report" --artifact - <<'EOF'
# Findings
- item
EOF

# Reply (continue an existing thread)
hive reply dodo "fixed"                      # auto-picks latest unanswered inbound

# Handoff, fork, spawn
hive handoff dodo --artifact /tmp/task.md    # delegate a thread
hive fork                                    # split the current pane into a clone
hive spawn claude                            # bring up a new agent pane

# Debug connectivity
hive doctor dodo                             # probe a peer's connectivity'''

_TMUX_REQUIRED_MESSAGE = "Hive requires tmux. Start or attach to a tmux session first."
_TMUX_OPTIONAL_ROOT_COMMANDS = {"plugin", "config", "shell-init", "codex", "claude", "resume-hint", "skills", "worktree", "ls"}


class SectionedHelpGroup(click.Group):
    def format_commands(self, ctx: click.Context, formatter: click.HelpFormatter) -> None:
        sections: dict[str, list[tuple[str, str]]] = defaultdict(list)
        for subcommand in self.list_commands(ctx):
            cmd = self.get_command(ctx, subcommand)
            if cmd is None or cmd.hidden:
                continue
            section = _COMMAND_HELP_SECTIONS.get(subcommand, "Other Commands")
            sections[section].append((subcommand, cmd.get_short_help_str(formatter.width)))

        for section in _COMMAND_HELP_SECTION_ORDER:
            rows = sections.get(section)
            if not rows:
                continue
            with formatter.section(section):
                description = _COMMAND_HELP_SECTION_DESCRIPTIONS.get(section, "")
                if description:
                    formatter.write_text(description)
                    formatter.write_paragraph()
                formatter.write_dl(rows)

    def format_epilog(self, ctx: click.Context, formatter: click.HelpFormatter) -> None:
        with formatter.section("Examples"):
            for block in _ROOT_HELP_EXAMPLES.split("\n\n"):
                formatter.write(f"  {block.replace(chr(10), chr(10) + '  ')}\n")
                formatter.write_paragraph()


def _discover_tmux_binding() -> dict[str, str]:
    if not tmux.is_inside_tmux():
        return {}
    current_pane = tmux.get_current_pane_id()
    if not current_pane:
        return {}
    team_name = tmux.get_pane_option(current_pane, "hive-team")
    if not team_name:
        return {}
    agent_name = tmux.get_pane_option(current_pane, "hive-agent") or ""
    role = tmux.get_pane_option(current_pane, "hive-role") or ""
    if not agent_name and not role:
        return {}
    window_target = tmux.get_current_window_target() or ""
    session_name = tmux.get_current_session_name() or ""
    workspace = tmux.get_window_option(window_target, "hive-workspace") if window_target else ""
    group = tmux.get_pane_option(current_pane, "hive-group") or ""
    payload = {
        "team": team_name,
        "workspace": workspace or "",
        "agent": agent_name,
        "role": role,
        "pane": current_pane,
        "tmuxSession": session_name,
        "tmuxWindow": window_target,
    }
    if group:
        payload["group"] = group
    return payload


def _default_team() -> str | None:
    discovered = _discover_tmux_binding().get("team")
    if discovered:
        return discovered
    if not tmux.is_inside_tmux():
        # Desktop-led worker: outside tmux the identity lives in the saved
        # default context (written by `hive duo init --channel`).
        return hive_context.load_current_context().get("team") or None
    return None


def _default_agent() -> str | None:
    discovered = _discover_tmux_binding().get("agent")
    if discovered:
        return discovered
    if not tmux.is_inside_tmux():
        return hive_context.load_current_context().get("agent") or None
    return None


def _require_team(team: str | None) -> str:
    if team:
        return team
    click.echo("Error: --team/-t required (or bind this tmux window with `hive init` / `hive create`)", err=True)
    sys.exit(1)


def _resolve_sender(agent_name: str | None) -> str:
    return agent_name or _default_agent() or LEAD_AGENT_NAME


def _load_team(team: str, *, prefer_pane: str = "") -> Team:
    try:
        return Team.load(team, prefer_pane=prefer_pane)
    except FileNotFoundError:
        click.echo(f"Error: team '{team}' not found", err=True)
        sys.exit(1)


def _resolve_member_cli_name(team: Team, member_name: str) -> str:
    member = team.get(member_name)
    cli_name = normalize_command(getattr(member, "cli", "") or "")
    if cli_name in AGENT_CLI_NAMES:
        return cli_name
    pane_id = getattr(member, "pane_id", "") or ""
    option_cli = normalize_command(tmux.get_pane_option(pane_id, "hive-cli") or "")
    if option_cli in AGENT_CLI_NAMES:
        return option_cli
    profile = detect_profile_for_pane(pane_id) if pane_id else None
    return profile.name if profile else "claude"


def _ensure_team_matches_current_window(t: Team) -> None:
    if not tmux.is_inside_tmux():
        return
    current_session = tmux.get_current_session_name() or ""
    current_window = tmux.get_current_window_target() or ""
    team_window = getattr(t, "tmux_window", "") or ""
    team_session = getattr(t, "tmux_session", "") or ""
    if not team_window:
        _fail(f"team '{t.name}' is not bound to a tmux window")
    if team_session and current_session and team_session != current_session:
        _fail(
            f"team '{t.name}' belongs to tmux session '{team_session}', not the current session '{current_session}'"
        )
    if current_window and team_window != current_window:
        _fail(f"team '{t.name}' belongs to tmux window '{team_window}', not the current window '{current_window}'")


def _resolve_scoped_team(team: str | None, *, required: bool = True) -> tuple[str | None, Team | None]:
    if team:
        loaded = _load_team(team)
        _ensure_team_matches_current_window(loaded)
        return team, loaded
    discovered_team = _default_team()
    if discovered_team:
        return discovered_team, _load_team(discovered_team, prefer_pane=tmux.get_current_pane_id() or "")
    if required:
        _fail("no Hive team is bound to this tmux window (run `hive init` in this window)")
    return None, None


@dataclass(frozen=True)
class _PaneTarget:
    """Identity of an agent pane resolved straight from its tmux options.

    Deliberately team-agnostic: ``team_name`` is empty for panes not bound to
    any Hive team, and ``member_label`` falls back to the literal pane id. This
    lets compact/fork operate on the pane in front of you whether or not it
    belongs to a team.
    """

    pane_id: str
    team_name: str
    is_team_bound: bool
    cli: str
    member_label: str


def _resolve_pane_target(pane_id: str = "") -> _PaneTarget:
    """Resolve a pane's identity from tmux pane options *only*.

    Never loads a ``Team`` and never calls ``_resolve_scoped_team`` /
    ``_resolve_sender`` / ``t.get``: re-resolving an agent by name is exactly the
    cross-window same-name bug that PR #8 fixed for ``compact --pane``. The pane
    facts (id, cli, team tag) are all read directly off the literal pane, so the
    command always acts on the pane in hand, team-bound or not.
    """
    pane = pane_id or tmux.get_current_pane_id() or ""
    if not pane:
        _fail("cannot determine current pane (pass --pane explicitly)")

    team_name = tmux.get_pane_option(pane, "hive-team") or ""
    option_cli = normalize_command(tmux.get_pane_option(pane, "hive-cli") or "")
    if option_cli in AGENT_CLI_NAMES:
        cli_name = option_cli
    else:
        profile = detect_profile_for_pane(pane)
        cli_name = profile.name if profile else ""
    if not cli_name:
        _fail(f"unsupported agent pane '{pane}'")

    member_label = tmux.get_pane_option(pane, "hive-agent") or pane
    return _PaneTarget(
        pane_id=pane,
        team_name=team_name,
        is_team_bound=bool(team_name),
        cli=cli_name,
        member_label=member_label,
    )


def _ensure_pane_in_scope(t: Team, pane_id: str) -> None:
    if not pane_id:
        return
    pane_window = tmux.get_pane_window_target(pane_id) or ""
    team_window = getattr(t, "tmux_window", "") or ""
    if team_window and pane_window and pane_window != team_window:
        _fail(f"pane '{pane_id}' is in tmux window '{pane_window}', not team '{t.name}' window '{team_window}'")
    pane_team = tmux.get_pane_option(pane_id, "hive-team")
    if pane_team and pane_team != t.name:
        _fail(f"pane '{pane_id}' already belongs to team '{pane_team}'")


def _reject_legacy_recipient_options(
    to_option: str | None,
    msg_option: str | None,
    *,
    command: str,
    to_agent: str,
) -> None:
    """Reject --to/--msg misuse and require a positional target agent.

    Raises UsageError so these argument-shape failures exit 2 with usage,
    matching Click's own parser errors; business validation stays exit 1.
    """
    if to_option is None and msg_option is None:
        if to_agent:
            return
        raise click.UsageError(
            f"hive {command} requires <agent>. Usage: hive {command} <agent> \"<body>\".",
            ctx=click.get_current_context(silent=True),
        )
    raise click.UsageError(
        f"hive {command} takes positional args: hive {command} <agent> \"<body>\". "
        "Drop --to/--msg.",
        ctx=click.get_current_context(silent=True),
    )


def _maybe_warn_long_body(body: str, *, command: str) -> None:
    from .runtime_state import body_warning_hint, format_body_warning

    hint = body_warning_hint(body)
    if hint is None:
        return
    click.echo(format_body_warning(command=command, hint=hint), err=True)


def _validate_root_send_protocol(body: str, artifact: str) -> None:
    from .runtime_state import body_warning_hint

    summary = body.strip()
    if not summary:
        _fail("new root send requires a short body summary")
    # artifact is not mandatory — short confirmations like "ack" or "已就位"
    # legitimately don't need one. The length/structure gate below already
    # forces bulky or structured content into --artifact.
    if body_warning_hint(summary) is not None:
        _fail(
            "new root send body must stay short and unstructured; move details into --artifact "
            "(prefer `--artifact -` unless you already have a file)"
        )


def _fail(msg: str) -> None:
    click.echo(f"Error: {msg}", err=True)
    sys.exit(1)


def _json_default_options(f):
    """JSON-default output contract: visible --plain plus hidden no-op --json.

    Default (and the legacy --json flag) emit JSON; --plain renders the human
    form. --plain wins when both are passed: --json is parsed but never
    reaches the callback (expose_value=False), it only exists so old
    invocations keep working.
    """
    f = click.option(
        "--json",
        "legacy_json",
        is_flag=True,
        hidden=True,
        expose_value=False,
        help="Deprecated no-op (JSON is the default output)",
    )(f)
    f = click.option("--plain", is_flag=True, help="Human-readable output instead of the default JSON")(f)
    return f


def _resolve_workspace(team: Team | None = None, required: bool = False) -> str:
    if team and team.workspace:
        return team.workspace
    current_context = hive_context.load_current_context()
    if current_context.get("workspace"):
        return current_context["workspace"]
    if required:
        _fail("workspace not found (create a team with --workspace, or run `hive init`)")
    return ""


def _add_runtime_location_fields(
    payload: dict[str, object],
    *,
    workspace_key: str = "workspace",
) -> dict[str, object]:
    if "runtimeWorkspace" not in payload and workspace_key in payload:
        payload["runtimeWorkspace"] = payload.pop(workspace_key)
    payload["cwd"] = os.getcwd()
    return payload


def _window_id_slug(window_id: str, fallback_index: str = "0") -> str:
    """Stable per-window slug. Uses the tmux window id (``@42`` → ``w42``),
    which is never reused within a session; falls back to the mutable window
    index only when no id is available.
    """
    raw = (window_id or "").lstrip("@") or str(fallback_index or "0")
    return f"w{raw}"


def _default_auto_workspace_path(session_name: str, window_id: str, fallback_index: str = "0") -> Path:
    return Path(f"/tmp/hive-{session_name}-{_window_id_slug(window_id, fallback_index)}")


def _default_team_name_for_window(session_name: str, window_id: str, window_index: str = "0") -> str:
    """Default team name derived from a window's stable id.

    Team name is a routing key, not a display title, so it must stay unique and
    stable. tmux window ids are never reused within a session, which avoids the
    cross-window collisions that window-index-derived names hit after break-out
    or window reorder (Bug A).
    """
    return f"{session_name}-{_window_id_slug(window_id, window_index)}"


def _team_default_auto_workspace_path(team: Team) -> Path | None:
    if not team.tmux_session:
        return None
    window_id = getattr(team, "tmux_window_id", "") or ""
    if not window_id and team.tmux_window and ":" in team.tmux_window:
        window_id = team.tmux_window.rsplit(":", 1)[-1]
    if not window_id:
        return None
    return _default_auto_workspace_path(team.tmux_session, window_id)


def _team_uses_default_auto_workspace(team: Team) -> bool:
    expected = _team_default_auto_workspace_path(team)
    if expected is None or not team.workspace:
        return False
    return Path(team.workspace).expanduser() == expected


def _remember_context(*, team: str = "", workspace: str = "", agent: str = "") -> None:
    current = hive_context.load_current_context()
    hive_context.save_current_context(
        team=team or current.get("team", ""),
        workspace=workspace or current.get("workspace", ""),
        agent=agent or current.get("agent", ""),
    )


def _parse_entries(entries: tuple[str, ...]) -> dict[str, str]:
    try:
        return bus.parse_key_value(entries)
    except ValueError as e:
        _fail(str(e))
    return {}


def _read_state(workspace: str, key: str, required: bool = True) -> str:
    path = Path(workspace) / "state" / key
    if not path.exists():
        if required:
            _fail(f"missing state file: {path}")
        return ""
    return path.read_text().strip()


def _team_window_identity(t: Team) -> tuple[str, str]:
    window_target = getattr(t, "tmux_window", "") or tmux.get_current_window_target() or ""
    window_id = getattr(t, "tmux_window_id", "") or ""
    if not window_id and window_target:
        window_id = tmux.get_window_id(window_target) or ""
    if not window_id:
        window_id = tmux.get_current_window_id() or ""
    if window_target and not getattr(t, "tmux_window", ""):
        t.tmux_window = window_target
    if window_id and not getattr(t, "tmux_window_id", ""):
        t.tmux_window_id = window_id
    return window_target, window_id


def _ensure_team_sidecar(t: Team, workspace: str | Path) -> int | None:
    from .sidecar import ensure_sidecar

    window_target, window_id = _team_window_identity(t)
    return ensure_sidecar(str(workspace), t.name, window_target, window_id)


def _augment_team_payload_with_runtime(t: Team, payload: dict[str, object]) -> dict[str, object]:
    from .sidecar import request_team_runtime

    ws = _resolve_workspace(t, required=False)
    if not ws:
        return payload
    _ensure_team_sidecar(t, ws)
    runtime = request_team_runtime(str(ws), team=t.name)
    if not runtime or runtime.get("ok") is False:
        return payload
    members_runtime = runtime.get("members")
    if not isinstance(members_runtime, dict):
        return payload
    for member in list(payload.get("members", [])):
        name = str(member.get("name", ""))
        runtime_fields = members_runtime.get(name)
        if not isinstance(runtime_fields, dict):
            continue
        for key in (
            "alive",
            "cliAlive",
            "busy",
            "model",
            "sessionId",
            "inputState",
            "inputReason",
            "turnPhase",
        ):
            value = runtime_fields.get(key)
            if value in ("", None):
                continue
            member[key] = value
    needs_answer = runtime.get("needsAnswer")
    if isinstance(needs_answer, list) and needs_answer:
        payload["needsAnswer"] = needs_answer
    return payload


def _should_show_description(desc: object) -> bool:
    if not isinstance(desc, str) or not desc:
        return False
    if desc.startswith("auto-init from "):
        return False
    return True


def _team_status_payload(t: Team) -> dict[str, object]:
    payload = _augment_team_payload_with_runtime(t, t.status())
    if not _should_show_description(payload.get("description")):
        payload.pop("description", None)
    discovered = _discover_tmux_binding() if tmux.is_inside_tmux() else {}
    if discovered.get("team") == t.name and discovered.get("agent"):
        payload["self"] = str(discovered["agent"])
    else:
        ctx = hive_context.load_current_context()
        if ctx.get("team") == t.name and ctx.get("agent"):
            payload["self"] = str(ctx["agent"])

    return _add_runtime_location_fields(payload)


def _resolve_live_agent(t: Team | None, agent_name: str):
    if t is None:
        _fail("team is required for tmux-based Hive messaging")
    try:
        agent = t.get(agent_name)
    except KeyError:
        _fail(f"agent '{agent_name}' is not registered in team '{t.name}'")
    _ensure_pane_in_scope(t, getattr(agent, "pane_id", "") or "")
    if not agent.is_alive():
        _fail(f"agent '{agent_name}' is not alive")
    return agent


def _resolve_target_pane() -> str:
    current = tmux.get_current_pane_id()
    if current:
        return current
    _fail("cannot determine target pane (run inside tmux)")
    return ""


def _resolve_artifact_path(artifact: str, workspace: str | Path = "") -> str:
    if not artifact:
        return ""
    if artifact == "-":
        # Read from stdin, save to workspace artifacts
        if not workspace:
            _fail("--artifact - requires a workspace (run inside a team)")
        _heredoc_recipe = (
            "  hive <cmd> <args> --artifact - <<'EOF'\n"
            "  # details\n"
            "  EOF"
        )
        if sys.stdin.isatty():
            _fail(
                "--artifact - expects piped stdin but a terminal is attached; "
                "use a heredoc instead:\n" + _heredoc_recipe
            )
        content = sys.stdin.read()
        if not content:
            _fail(
                "--artifact - received empty stdin; pipe content in or use a heredoc:\n"
                + _heredoc_recipe
            )
        ws_artifacts = Path(workspace) / "artifacts"
        ws_artifacts.mkdir(parents=True, exist_ok=True)
        # Short random id — file name is never parsed by downstream code,
        # so no timestamp / sortable prefix is needed.
        filename = f"{secrets.token_urlsafe(4)}.md"
        path = ws_artifacts / filename
        path.write_text(content)
        return str(path)
    resolved_artifact = str(Path(artifact).expanduser())
    if not Path(resolved_artifact).exists():
        _fail(f"artifact not found: {resolved_artifact}")
    return resolved_artifact


def _status_migration_failure(command_name: str) -> None:
    _fail(
        f"`hive {command_name}` was removed; use `hive send` to send messages "
        "and `hive team` to inspect runtime input state"
    )


def _resolve_spawn_cli_name(cli_name: str | None) -> str:
    if cli_name in AGENT_CLI_NAMES:
        return cli_name
    current_pane = tmux.get_current_pane_id()
    option_cli = normalize_command(tmux.get_pane_option(current_pane, "hive-cli") or "") if current_pane else ""
    if option_cli in AGENT_CLI_NAMES:
        return option_cli
    profile = detect_profile_for_pane(current_pane) if current_pane else None
    return profile.name if profile else "claude"


def _request_send_payload(
    *,
    workspace: str,
    team: Team,
    sender_agent: str,
    target_agent: str,
    body: str,
    artifact: str = "",
    reply_to: str = "",
    command_name: str = "send",
    warn_on_long_body: bool = True,
) -> dict[str, object]:
    from .sidecar import request_send

    if warn_on_long_body:
        _maybe_warn_long_body(body, command=command_name)
    _ensure_team_sidecar(team, workspace)
    payload = request_send(
        str(workspace),
        team=team.name,
        sender_agent=sender_agent,
        sender_pane=tmux.get_current_pane_id() or "",
        target_agent=target_agent,
        body=body,
        artifact=artifact,
        reply_to=reply_to,
    )
    if not payload:
        raise RuntimeError("sidecar unavailable")
    if payload.get("ok") is False:
        raise RuntimeError(str(payload.get("error", f"{command_name} failed")))
    normalized = dict(payload)
    normalized.pop("ok", None)
    return normalized


def _stderr_is_interactive() -> bool:
    return sys.stderr.isatty()


_CODEX_NATIVE_REQUIRED_BYPASS_COMMANDS = {
    "claude",
    "codex",
    "config",
    "current",
    "doctor",
    "inject",
    "plugin",
    "resume-hint",
    "shell-init",
    "skills",
    "status",
    "status-set",
    "status-show",
    "statuses",
    "wait-status",
}


def _codex_native_pane_from_env() -> str:
    return os.environ.get("HIVE_CODEX_PANE", "").strip()


def _is_codex_tool_env() -> bool:
    return bool(os.environ.get("CODEX_THREAD_ID", "").strip())


def _codex_relaunch_message() -> str:
    return (
        "this codex isn't daemon-backed — hive runtime is degraded.\n"
        "make every future codex native (run once, any shell):\n"
        "  grep -q 'hive shell-init' ~/.zshrc || "
        "echo 'eval \"$(hive shell-init zsh)\"' >> ~/.zshrc\n"
        "then exit this codex (Ctrl-C twice) and run: hive codex resume"
    )


def _require_codex_native(invoked: str | None) -> None:
    if invoked in _CODEX_NATIVE_REQUIRED_BYPASS_COMMANDS:
        return
    if _codex_native_pane_from_env() or not _is_codex_tool_env():
        return
    _fail(_codex_relaunch_message())




def _hive_version() -> str:
    """Installed distribution version, falling back to pyproject in a source checkout."""
    import importlib.metadata as _md

    try:
        return _md.version("hive")
    except _md.PackageNotFoundError:
        try:
            import tomllib

            pyproject = Path(__file__).resolve().parents[2] / "pyproject.toml"
            return str(tomllib.loads(pyproject.read_text())["project"]["version"])
        except Exception:
            return "unknown"


@click.group(cls=SectionedHelpGroup, context_settings={"help_option_names": ["-h", "--help"]})
@click.version_option(version=_hive_version(), prog_name="hive")
@click.pass_context
def cli(ctx: click.Context):
    """Hive - tmux-first multi-agent collaboration runtime."""
    if ctx.resilient_parsing:
        return
    if any(arg in {"-h", "--help"} for arg in sys.argv[1:]):
        return
    _require_codex_native(ctx.invoked_subcommand)
    if ctx.invoked_subcommand not in _TMUX_OPTIONAL_ROOT_COMMANDS and ctx.invoked_subcommand is not None and not tmux.is_inside_tmux():
        # Desktop-led exception: `init`/`duo` form the binding themselves, and
        # once a saved default context names a team this external session IS a
        # member (the anchor-pane duo) — its commands resolve against the tmux
        # server without the caller living inside tmux.
        if ctx.invoked_subcommand not in ("init", "duo") and not hive_context.load_current_context().get("team"):
            _fail(_TMUX_REQUIRED_MESSAGE)


def _gc_dead_teams() -> None:
    """Clean up workspaces for teams whose tmux window no longer exists.

    With tmux-only storage, team state dies with the window. This only
    handles leftover workspace directories and persisted context files.
    """
    from .team import list_teams
    live_names = {t["name"] for t in list_teams()}
    root = HIVE_HOME / "teams"
    if root.is_dir():
        for path in sorted(root.iterdir()):
            if not path.is_dir():
                continue
            if path.name not in live_names:
                shutil.rmtree(path, ignore_errors=True)
    ctx = hive_context.load_current_context()
    if ctx.get("team") and ctx["team"] not in live_names:
        hive_context.clear_current_context()


_FORK_MIN_COLS = 80
_FORK_MIN_ROWS = 20


def _choose_fork_split(width: int, height: int) -> bool:
    """Return True for horizontal (left/right) split, False for vertical (top/bottom).

    Accounts for the 1-cell tmux separator consumed by the split.
    """
    h_half = (width - 1) // 2
    v_half = (height - 1) // 2
    can_h = h_half >= _FORK_MIN_COLS
    can_v = v_half >= _FORK_MIN_ROWS
    if can_h and can_v:
        return width >= height * 2.5
    if can_h:
        return True
    if can_v:
        return False
    h_score = min(h_half / _FORK_MIN_COLS, height / _FORK_MIN_ROWS)
    v_score = min(width / _FORK_MIN_COLS, v_half / _FORK_MIN_ROWS)
    return h_score >= v_score


@cli.command("fork")
@click.option("--pane", "pane_id", default="", help="Source pane ID (default: auto-detect)")
@click.option("--split", "-s", type=click.Choice(["auto", "h", "v"]), default="auto", help="Split direction (default: auto-detect from pane dimensions)")
@click.option("--join-as", default="", help="Register the forked pane into the current team as this agent name")
@click.option("--prompt", default="", help="Prompt to send to the forked agent after it is ready")
def fork_cmd(pane_id: str, split: str, join_as: str, prompt: str):
    """Fork the current agent session into a new split pane.

    Humans typically bind this to a keyboard shortcut (terminal + tmux).
    Agents also invoke it during handoff to create a clone that can pick
    up work without interrupting the current turn.

    Pass `--join-as <name>` to register the new pane as a team member;
    `--prompt` then sends an initial message after the fork is ready.

    On a pane not bound to any Hive team, fork still works: it produces a bare,
    independent clone (no team registration, no `@hive-*` tags) and returns
    `registered: null`, `team: null`. `--join-as` requires a team-bound pane.

    \b
    Examples:
      hive fork                                  # auto-detect split direction
      hive fork --split h                        # force horizontal split
      hive fork --join-as dodo-c1 --prompt "continue the thread"
    """
    target = _resolve_pane_target(pane_id)
    if not target.is_team_bound:
        # Non-team pane: clone it bare — no member registration, no @hive-* tags.
        # The clone is an independent agent that belongs to no Hive team.
        if join_as:
            _fail("--join-as requires a team-bound pane")
        new_pane = _fork_orphan_clone(target.pane_id, split, prompt)
        click.echo(json.dumps({
            "pane": new_pane,
            "registered": None,
            "team": None,
        }, indent=2, ensure_ascii=False))
        return

    # Team-bound fork: register the clone as a new team member (unchanged).
    if pane_id:
        target_team = _load_team(target.team_name)
    else:
        _, target_team = _resolve_scoped_team(None, required=True)

    if not join_as:
        window_target = target_team.tmux_window or tmux.get_current_window_target() or ""
        panes = tmux.list_panes_full(window_target) if window_target else []
        seen_names = _window_seen_names(target_team, panes)
        join_as = _derive_agent_name(seen_names)
        source_pane = pane_id or (tmux.get_current_pane_id() or "")
        group = tmux.get_pane_option(source_pane, "hive-group") if source_pane else ""
        if group and group != "duo":
            join_as = f"{group}.{join_as}"

    registered_agent, new_pane = _fork_registered_agent(
        t=target_team,
        pane_id=pane_id,
        split=split,
        join_as=join_as,
        prompt=prompt,
    )
    del registered_agent
    click.echo(json.dumps({
        "pane": new_pane,
        "registered": join_as,
        "team": target_team.name,
    }, indent=2, ensure_ascii=False))


@cli.command("current", hidden=True)
def current_cmd():
    _fail("`hive current` was removed; use `hive team` to inspect team overview + self")


_RANDOM_AGENT_NAMES = (
    "yoyo", "lulu", "nini", "bobo", "kiki",
    "dodo", "pipi", "toto", "momo", "coco",
)


def _names_used_in_window(panes: list[tmux.PaneInfo]) -> set[str]:
    return {pane.agent.strip() for pane in panes if pane.agent.strip()}


def _derive_agent_name(seen: set[str]) -> str:
    """Pick a short random peer name while avoiding collisions in this window."""
    available = [name for name in _RANDOM_AGENT_NAMES if name not in seen]
    if available:
        candidate = secrets.choice(available)
    else:
        suffix = 1
        candidate = f"agent-{suffix}"
        while candidate in seen:
            suffix += 1
            candidate = f"agent-{suffix}"
    seen.add(candidate)
    return candidate


def _window_seen_names(t: Team, panes: list[tmux.PaneInfo]) -> set[str]:
    seen_names = _names_used_in_window(panes)
    seen_names.add(t.lead_name or LEAD_AGENT_NAME)
    return seen_names


def _claim_member_name(name_override: str, seen_names: set[str]) -> None:
    if not name_override:
        return
    if name_override in seen_names:
        _fail(f"name '{name_override}' is already taken in this window")
    seen_names.add(name_override)


def _resolve_pane_cli(pane: tmux.PaneInfo) -> str:
    pane_cli = normalize_command(pane.cli or pane.command)
    if pane_cli not in AGENT_CLI_NAMES:
        profile = detect_profile_for_pane(pane.pane_id)
        if profile:
            pane_cli = profile.name
    return pane_cli


def _classify_pane(pane: tmux.PaneInfo) -> tuple[str, str]:
    pane_cli = _resolve_pane_cli(pane)
    return ("agent" if pane_cli in AGENT_CLI_NAMES else "terminal", pane_cli)


def _hive_join_message(agent_name: str, team_name: str) -> str:
    return (
        f"You are '{agent_name}' in hive team '{team_name}'. "
        "Context is pre-bound. Run `hive skills get core` first and follow "
        "that protocol. Hive messages will arrive inline as "
        "<HIVE ...> ... </HIVE> blocks. "
        "Use `hive team` to inspect the team; reply on an existing thread with "
        "`hive reply <name> \"...\"`; open a new thread with "
        "`hive send <name> \"<summary>\" --artifact -`."
    )


def _register_agent_member(
    t: Team,
    *,
    pane_id: str,
    team_name: str,
    agent_name: str,
    pane_cli: str,
    cwd: str,
    notify: bool,
    group: str = "",
) -> Agent:
    agent = Agent(
        name=agent_name,
        team_name=team_name,
        pane_id=pane_id,
        cwd=cwd,
        cli=pane_cli,
    )
    t.agents[agent_name] = agent
    tmux.tag_pane(pane_id, "agent", agent_name, team_name, cli=pane_cli, group=group)
    ws = _resolve_workspace(t, required=False)
    if ws:
        hive_context.save_context_for_pane(pane_id, team=team_name, workspace=ws, agent=agent_name)
    if notify:
        from .agent import DeliveryError

        try:
            agent.send(_hive_join_message(agent_name, team_name))
        except DeliveryError as e:
            # Registration is transactional: a pane whose native transport
            # refused the join must not linger half-registered (tagged and
            # routable but proven undeliverable). Roll every mutation back so
            # a later retry starts clean.
            t.agents.pop(agent_name, None)
            tmux.clear_pane_tags(pane_id)
            if ws:
                hive_context.clear_context_for_pane(pane_id)
            _fail(
                f"pane {pane_id} is not reachable over its native transport ({e}); "
                "nothing was registered. Fix the channel/daemon and retry, "
                "or use --no-notify to register without a reachability check."
            )
    return agent


def _spawn_team_agent(
    t: Team,
    *,
    team_name: str,
    agent_name: str,
    model: str = "",
    prompt: str = "",
    cwd: str = "",
    skill: str = "hive",
    workflow: str = "",
    env_entries: tuple[str, ...] = (),
    cli_name: str | None = None,
) -> Agent:
    resolved_cli_name = _resolve_spawn_cli_name(cli_name)
    extra_env = _parse_entries(env_entries) if env_entries else {}
    agent = t.spawn(
        agent_name,
        model=model,
        prompt=prompt,
        cwd=cwd,
        skill=skill,
        workflow=workflow,
        extra_env=extra_env or None,
        cli=resolved_cli_name,
    )
    hive_context.save_context_for_pane(
        agent.pane_id,
        team=team_name,
        workspace=_resolve_workspace(t, required=False),
        agent=agent_name,
    )
    _remember_context(team=team_name, workspace=_resolve_workspace(t, required=False), agent=LEAD_AGENT_NAME)
    return agent


def _fork_source_details(pane_id: str, split: str, *, workspace: str = "") -> tuple[str, object, str, bool, str]:
    if not tmux.is_inside_tmux():
        _fail("hive fork requires tmux")

    current_pane = pane_id or tmux.get_current_pane_id()
    if not current_pane:
        _fail("cannot determine current pane (pass --pane explicitly)")

    profile = detect_profile_for_pane(current_pane)
    if not profile:
        _fail(f"unsupported agent pane '{current_pane}'")

    if split == "auto":
        width = int(tmux.display_value(current_pane, "#{pane_width}") or "80")
        height = int(tmux.display_value(current_pane, "#{pane_height}") or "24")
        horizontal = _choose_fork_split(width, height)
    else:
        horizontal = split == "h"

    session_id: str | None = None
    if workspace:
        from .sidecar import request_runtime_snapshot
        payload = request_runtime_snapshot(workspace, pane_id=current_pane) or {}
        snapshot = payload.get("snapshot")
        if isinstance(snapshot, dict) and snapshot.get("_sessionIdFresh", True):
            sid = snapshot.get("sessionId")
            if sid and sid != "unresolved":
                session_id = str(sid)
    if not session_id:
        session_id = resolve_session_id_for_pane(current_pane, profile=profile)
    if not session_id:
        _fail(f"cannot determine session id for pane '{current_pane}'")

    source_cwd = tmux.display_value(current_pane, "#{pane_current_path}") or ""
    return current_pane, profile, session_id, horizontal, source_cwd


_FORK_NEW_TASK_MARKER = "NEW TASK FOR THIS FORK:"
_FORK_BOUNDARY_TEXT = (
    "FORK BOUNDARY: you are a freshly forked agent. Run `hive team` to find your "
    "own identity (the `self` field).\n\n"
    "Everything before this boundary is read-only inherited context for the "
    "original agent. This includes the user's most recent instruction, any "
    "unfinished request, and any pending tool/bash/action from the prior "
    "transcript. Treat all of it as already owned by the original agent. Do NOT "
    "continue, retry, or re-execute any task from before this boundary.\n\n"
    f"After `hive team`, act only on instructions explicitly provided after the "
    f"marker `{_FORK_NEW_TASK_MARKER}` in this message, or on future messages "
    f"that arrive after this boundary. If no `{_FORK_NEW_TASK_MARKER}` section "
    f"is present, stop after identifying yourself and wait for new input."
)
# Orphan variant: a non-team fork has no team and no `self`, so it must NOT be
# told to run `hive team` to find an identity. The anti-re-execution core is
# preserved verbatim — that is what stops the clone from re-running the parent's
# in-flight work regardless of team membership.
_FORK_ORPHAN_BOUNDARY_TEXT = (
    "FORK BOUNDARY: you are a freshly forked, independent clone. You are NOT "
    "bound to any Hive team — running `hive team` only confirms you have no team "
    "binding, and there is no `self` identity to look up.\n\n"
    "Everything before this boundary is read-only inherited context for the "
    "original agent. This includes the user's most recent instruction, any "
    "unfinished request, and any pending tool/bash/action from the prior "
    "transcript. Treat all of it as already owned by the original agent. Do NOT "
    "continue, retry, or re-execute any task from before this boundary.\n\n"
    f"Act only on instructions explicitly provided after the marker "
    f"`{_FORK_NEW_TASK_MARKER}` in this message, or on future messages that "
    f"arrive after this boundary. If no `{_FORK_NEW_TASK_MARKER}` section is "
    f"present, stop and wait for new human input."
)


def _fork_boundary_prompt(*, team_bound: bool = True) -> str:
    """The boundary message every fork receives as its first user input.

    Static across workspaces and forks: the new pane resumes the parent's session
    so its transcript starts populated with mid-flight tool calls and intended
    actions. Without a boundary the child happily re-executes those (e.g.
    triggering another `hive fork` and recursing). A team-bound fork discovers its
    own name via `hive team`; an orphan (``team_bound=False``) has no team or
    `self` and is told so instead.
    """
    return _FORK_BOUNDARY_TEXT if team_bound else _FORK_ORPHAN_BOUNDARY_TEXT


def _fork_boundary_file(*, team_bound: bool = True) -> Path:
    """Cached static boundary file under ``$HIVE_HOME``.

    Lets the resume command stay short (``cat <path>`` rather than the full
    several-line text inline). Rewritten when the cached content drifts from the
    current code so updates take effect without manual cleanup. Team-bound and
    orphan boundaries live in separate files so neither clobbers the other.
    """
    text = _fork_boundary_prompt(team_bound=team_bound)
    filename = "fork-boundary.txt" if team_bound else "fork-boundary-orphan.txt"
    path = HIVE_HOME / filename
    if not path.exists() or path.read_text() != text:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    return path


def _fork_registered_agent(
    *,
    t: Team,
    pane_id: str,
    split: str,
    join_as: str,
    prompt: str = "",
) -> tuple[Agent, str]:
    _ensure_pane_in_scope(t, pane_id)
    window_target = t.tmux_window or tmux.get_current_window_target() or ""
    panes = tmux.list_panes_full(window_target) if window_target else []
    seen_names = _window_seen_names(t, panes)
    _claim_member_name(join_as, seen_names)

    current_pane, profile, session_id, horizontal, source_cwd = _fork_source_details(
        pane_id, split, workspace=getattr(t, "workspace", ""),
    )

    # `codex fork <sid>` launches an embedded codex (no per-pane daemon), and
    # embedded codex is unsupported as a team member — the forked pane would
    # join the team with no session id and no runtime state. Refuse before any
    # pane is split or registered. Non-team forks (_fork_orphan_clone) stay
    # allowed: a bare clone belongs to no team and makes no runtime promises.
    if profile.name == "codex":
        _fail(
            "codex team fork requires daemon-backed fork support; "
            "embedded codex is unsupported as a team member "
            "(fork it without a team, or start a fresh daemon-backed codex instead)"
        )

    # Boundary text is static across workspaces and forks, so cache it under
    # $HIVE_HOME and expand via shell command substitution when there is no
    # prompt. With --prompt we inline boundary + marker + prompt together so
    # the fork sees both in one user message.
    cmd_base = profile.fork_cmd.format(session_id=session_id)
    if prompt:
        composed = f"{_fork_boundary_prompt(team_bound=True)}\n\n{_FORK_NEW_TASK_MARKER}\n{prompt}"
        launch_cmd = f"{cmd_base} {shlex.quote(composed)}"
    else:
        launch_cmd = f"{cmd_base} \"$(cat {shlex.quote(str(_fork_boundary_file(team_bound=True)))})\""
    new_pane = tmux.split_window(current_pane, horizontal=horizontal, cwd=source_cwd or None, detach=False)
    tmux.send_keys(new_pane, launch_cmd)
    group = join_as.partition(".")[0] if "." in join_as else ""
    registered_agent = _register_agent_member(
        t,
        pane_id=new_pane,
        team_name=t.name,
        agent_name=join_as,
        pane_cli=profile.name,
        cwd=source_cwd or os.getcwd(),
        notify=False,
        group=group,
    )
    return registered_agent, new_pane


def _fork_orphan_clone(pane_id: str, split: str, prompt: str = "") -> str:
    """Fork a non-team agent pane into a bare, independent clone.

    Mirrors a registered fork — split the pane, fork the parent session via the
    CLI's fork command (``profile.fork_cmd``: ``codex fork`` / ``claude
    --fork-session``), then send the boundary — but skips
    member registration and writes no ``@hive-*`` pane tags: the clone belongs to
    no team. Uses the orphan boundary so the clone is not told to look up a
    `self` it does not have. Returns the new pane id.
    """
    current_pane, profile, session_id, horizontal, source_cwd = _fork_source_details(pane_id, split)
    cmd_base = profile.fork_cmd.format(session_id=session_id)
    if prompt:
        composed = f"{_fork_boundary_prompt(team_bound=False)}\n\n{_FORK_NEW_TASK_MARKER}\n{prompt}"
        launch_cmd = f"{cmd_base} {shlex.quote(composed)}"
    else:
        launch_cmd = f"{cmd_base} \"$(cat {shlex.quote(str(_fork_boundary_file(team_bound=False)))})\""
    new_pane = tmux.split_window(current_pane, horizontal=horizontal, cwd=source_cwd or None, detach=False)
    tmux.send_keys(new_pane, launch_cmd)
    return new_pane


def _resolve_handoff_anchor_event(
    workspace: str,
    *,
    current_agent: str,
    reply_to_override: str,
) -> dict[str, object]:
    if reply_to_override:
        event = bus.find_send_event(workspace, reply_to_override)
        if event is None or str(event.get("to") or "") != current_agent:
            _fail(
                f"msgId '{reply_to_override}' is not an inbound send event for '{current_agent}'"
            )
        return event

    latest = bus.latest_unanswered_inbound_send_event(workspace, recipient=current_agent)
    if latest is None:
        _fail(
            f"no unanswered inbound message for '{current_agent}'; "
            "pass --reply-to explicitly to hand off a different thread"
        )
    return latest


def _find_qualified_agent_target(qualified: str) -> tuple[str, str] | None:
    """Locate a pane by qualified agent name ``<prefix>.<name>``.

    Scans every hive-tagged pane across all sessions.  Returns
    ``(team_name, agent_name)`` on unique match or ``None`` if no match.

    A pane matches when ``p.agent == qualified`` and ``p.team`` is non-empty.
    A missing ``@hive-group`` is tolerated (fork/register paths may not always
    set it), but a *conflicting* group (present and differs from the qualified
    prefix) is rejected with ``ValueError`` — it signals a tagging mistake,
    not a legitimate target.

    Raises ``ValueError`` on ambiguous duplicates or conflicting group tags.
    """
    if "." not in qualified:
        return None
    prefix, _, _ = qualified.partition(".")
    if not prefix:
        return None
    candidates = [
        p for p in tmux.list_panes_all()
        if p.agent == qualified and p.team
    ]
    if not candidates:
        return None
    for p in candidates:
        if p.group and p.group != prefix:
            raise ValueError(
                f"agent '{qualified}' on pane {p.pane_id} has conflicting "
                f"@hive-group '{p.group}' (expected '{prefix}' or empty)"
            )
    if len(candidates) > 1:
        raise ValueError(
            f"agent '{qualified}' matches {len(candidates)} panes; "
            "qualified agent names must be unique"
        )
    return candidates[0].team, candidates[0].agent


def _resolve_send_target_team(to_agent: str) -> tuple[str, Team]:
    """Resolve the team that owns *to_agent* for send/reply.

    Qualified names (`<group>.<name>`) bypass the current-window check
    and load the target pane's team directly, so cross-team sends work
    across tmux windows. Bare names fall back to the caller's scoped
    team (same behaviour as before).
    """
    if "." in to_agent:
        try:
            resolved = _find_qualified_agent_target(to_agent)
        except ValueError as exc:
            _fail(str(exc))
            raise  # unreachable — _fail exits
        if resolved is None:
            _fail(
                f"agent '{to_agent}' not found in any team "
                f"(check @hive-agent tag on the target pane)"
            )
            raise AssertionError("unreachable")
        target_team_name, _ = resolved
        return target_team_name, _load_team(target_team_name)
    team_name, t = _resolve_scoped_team(None, required=True)
    assert team_name is not None and t is not None
    return team_name, t


def _existing_team_agent(t: Team, agent_name: str) -> Agent | None:
    try:
        return t.get(agent_name)
    except KeyError:
        return None


def _handoff_delegate_body(
    *,
    sender_agent: str,
    original_sender: str,
    anchor_msg_id: str,
    note: str,
) -> str:
    lines = [
        f"Handoff from {sender_agent}.",
        f"Original sender: {original_sender}",
        f"Anchor msgId: {anchor_msg_id}",
        f"First step: hive thread {anchor_msg_id}",
        f"First reply: hive reply {original_sender} --reply-to {anchor_msg_id} \"<takeover>\"",
        f"(--reply-to is required on the first reply because you never received {anchor_msg_id} yourself.)",
        f"Once {original_sender} replies back, continue with plain 'hive reply {original_sender} \"...\"' — autoReply picks the thread.",
    ]
    if note.strip():
        lines.append(f"Note: {note.strip()}")
    return "\n".join(lines)


def _handoff_announce_body(*, target_agent: str) -> str:
    return (
        f"Delegating this thread to {target_agent}. "
        "Their handoff message is in flight."
    )


def _pane_last_activity(pane_id: str) -> int:
    try:
        return int(tmux.display_value(pane_id, "#{pane_last_activity}") or "0")
    except (ValueError, TypeError):
        return 0


def _pane_is_idle_for_pairing(pane_id: str) -> bool:
    """Return True when *pane_id* is an agent pane safe to pair with.

    Uses sidecar runtime inspection (turnPhase) with a graceful fallback:
    freshly-opened CLIs without a session yet count as idle, turn_closed
    counts as idle, everything else is treated as 'busy'.
    """
    try:
        from .sidecar import _agent_runtime_payload
        runtime = _agent_runtime_payload(pane_id)
    except Exception:
        return False
    if not runtime.get("alive", True):
        return False
    phase = str(runtime.get("turnPhase") or "")
    if phase == "turn_closed":
        return True
    if runtime.get("inputReason") == "no_session":
        return True
    return False


def _require_codex_daemon_backed(pane: str) -> None:
    """Refuse to let an embedded (non-daemon) codex join; point to the fix.

    A manually-launched bare codex runs its app-server embedded, so hive can
    only reverse-engineer state from the transcript — never read native runtime.
    Rather than register a degraded member, stop here and tell the user how to
    relaunch it daemon-backed; ``hive codex resume`` preserves the session.
    """
    native_pane = _codex_native_pane_from_env()
    if native_pane:
        pane = native_pane
        from .adapters import codex_app_server

        sock = codex_app_server.pane_socket_path(pane)
        if sock.exists() and codex_app_server.probe_socket(str(sock)):
            return
        _fail(_codex_relaunch_message())
    if _is_codex_tool_env():
        _fail(_codex_relaunch_message())
    if not pane:
        return
    profile = detect_profile_for_pane(pane)
    if not profile or profile.name != "codex":
        return
    from .adapters import codex_app_server

    sock = codex_app_server.pane_socket_path(pane)
    if sock.exists() and codex_app_server.probe_socket(str(sock)):
        return  # already daemon-backed (born-connected / hive-spawned) — fine
    from .adapters.codex import CodexAdapter

    sid = CodexAdapter().resolve_current_session_id(pane) or ""
    resume = f"hive codex resume {sid}" if sid else "hive codex resume"
    _fail(
        "this codex is running embedded; hive needs it daemon-backed for native "
        "runtime, so it can't join yet.\n"
        "make every future codex native (run once, any shell):\n"
        "  grep -q 'hive shell-init' ~/.zshrc || "
        "echo 'eval \"$(hive shell-init zsh)\"' >> ~/.zshrc\n"
        "for this session now (your session is preserved):\n"
        "  1) exit codex: press Ctrl-C (twice)\n"
        f"  2) run: {resume}\n"
        "then re-run /hive."
    )


def _run_duo_init(validator_cli: str | None, channel: str = "") -> None:
    """Shared callback body for the equivalent `hive init` / `hive duo init`."""
    if not tmux.is_inside_tmux():
        # Desktop-led duo: this (external) Claude session is the worker,
        # reachable over its channel socket; hive owns a detached session
        # for the anchor + validator panes.
        sock = channel or _discover_client_channel_socket()
        result = _create_ccd_duo(channel_socket=sock, validator_cli=validator_cli)
        click.echo(json.dumps(result, indent=2, ensure_ascii=False))
        return
    current_pane = tmux.get_current_pane_id() or ""
    if not current_pane:
        _fail("cannot determine current pane")
    if detect_profile_for_pane(current_pane) is None:
        _fail("current pane must be running claude / codex (this becomes the worker)")
    _require_codex_daemon_backed(current_pane)

    result = _create_standalone_duo(current_pane=current_pane, validator_cli=validator_cli)
    click.echo(json.dumps(result, indent=2, ensure_ascii=False))


@cli.command("init")
@click.option(
    "--validator-cli",
    type=click.Choice(["claude", "codex"]),
    default=None,
    help="CLI for validator (default: anti-family of current pane's CLI)",
)
@click.option(
    "--channel",
    default="",
    help="Outside tmux only: this Claude session's channel socket "
    "(hive-client-<pid>.sock, printed in the hive-channel MCP instructions)",
)
def init_cmd(validator_cli: str | None, channel: str):
    """Initialize a duo in this window (equivalent to `hive duo init`).

    Worker = this pane, validator = anti-family spawn. Team name and
    workspace derive from the final window. Idempotent: re-running in a
    bound window reports the existing binding.
    """
    _run_duo_init(validator_cli, channel=channel)


@cli.command("register")
@click.argument("pane_id")
@click.option("--as", "name_override", default="", help="Name for the new member (default: auto-derived)")
@click.option("--notify/--no-notify", default=True, help="Deliver the join message over the native transport (doubles as a reachability check; --no-notify registers without proving the pane deliverable)")
@click.option("--group", "group_name", default="", help="Cross-team group tag for display and namespace reservation (optional; qualified-name routing works without it).")
def register_cmd(pane_id: str, name_override: str, notify: bool, group_name: str):
    """Register an external pane into the current team."""
    if not tmux.is_inside_tmux():
        _fail("hive register requires a tmux session.")

    binding = _discover_tmux_binding()
    team_name = binding.get("team")
    if not team_name:
        _fail("no team bound to the current window. Run `hive duo init` or `hive squad init` first.")

    t = Team.load(team_name, prefer_pane=tmux.get_current_pane_id() or "")
    window_target = t.tmux_window or tmux.get_current_window_target() or ""
    panes = tmux.list_panes_full(window_target) if window_target else []

    target_pane = None
    for pane in panes:
        if pane.pane_id == pane_id:
            target_pane = pane
            break
    if target_pane is None:
        _fail(f"pane '{pane_id}' not found in window '{window_target}'")

    if target_pane.team == team_name and target_pane.agent:
        _fail(f"pane '{pane_id}' is already registered as '{target_pane.agent}'")

    seen_names = _window_seen_names(t, panes)
    _claim_member_name(name_override, seen_names)

    role, pane_cli = _classify_pane(target_pane)
    if role != "agent":
        _fail(f"pane '{pane_id}' is not running an agent CLI; only agent panes can be registered")
    agent_name = name_override or _derive_agent_name(seen_names)
    _register_agent_member(
        t,
        pane_id=pane_id,
        team_name=team_name,
        agent_name=agent_name,
        pane_cli=pane_cli,
        cwd=tmux.display_value(pane_id, "#{pane_current_path}") or os.getcwd(),
        notify=notify,
        group=group_name,
    )
    member_name = agent_name

    result_payload = {
        "registered": member_name,
        "role": role,
        "pane": pane_id,
        "team": team_name,
    }
    if group_name:
        result_payload["group"] = group_name
    click.echo(json.dumps(result_payload, indent=2, ensure_ascii=False))


@cli.command()
@click.argument("name")
@click.option("--desc", "-d", default="", help="Team description")
@click.option("--workspace", "-w", default="", help="Workspace path to initialize")
@click.option("--reset-workspace", is_flag=True, help="Remove existing workspace before initialization")
@click.option("--state", "state_entries", multiple=True, help="Initial state KEY=VALUE (repeatable)")
def create(name: str, desc: str, workspace: str, reset_workspace: bool, state_entries: tuple[str, ...]):
    """Create a team."""
    if state_entries and not workspace:
        _fail("--state requires --workspace")
    if reset_workspace and not workspace:
        _fail("--reset-workspace requires --workspace")
    try:
        ws_str = str(Path(workspace).expanduser()) if workspace else ""
        t = Team.create(name, description=desc, workspace=ws_str)
        if workspace:
            ws = Path(workspace).expanduser()
            if ws.exists() and reset_workspace:
                shutil.rmtree(ws)
            bus.init_workspace(ws)
            for key, value in _parse_entries(state_entries).items():
                (ws / "state" / key).write_text(value)
            _remember_context(team=name, workspace=str(ws), agent=LEAD_AGENT_NAME)
        else:
            _remember_context(team=name, agent=LEAD_AGENT_NAME)
        click.echo(f"Team '{name}' created.")
        if workspace:
            click.echo(f"Workspace initialized: {Path(workspace).expanduser()}")
    except ValueError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)


@cli.command()
@click.argument("name")
@click.option("--workspace", "-w", default="", help="Workspace path to remove")
@click.option("--keep-workspace", is_flag=True, hidden=True, help="Deprecated no-op (workspace is now kept by default)")
@click.option("--delete-workspace", is_flag=True, help="Also delete the workspace directory")
def delete(name: str, workspace: str, keep_workspace: bool, delete_workspace: bool):
    """Delete a team and clean up."""
    team_workspace = ""
    team_window = ""
    try:
        t = Team.load(name)
        team_workspace = t.workspace
        team_window = t.tmux_window
        t.cleanup()
    except FileNotFoundError:
        pass

    if team_window:
        for key in ("hive-team", "hive-workspace", "hive-desc", "hive-created", "hive-peers"):
            tmux.clear_window_option(team_window, f"@{key}")

    legacy_team_dir = HIVE_HOME / "teams" / name
    if legacy_team_dir.exists():
        shutil.rmtree(legacy_team_dir)
    legacy_tasks_dir = HIVE_HOME / "tasks" / name
    if legacy_tasks_dir.exists():
        shutil.rmtree(legacy_tasks_dir)

    resolved_workspace = workspace or team_workspace or os.environ.get("HIVE_WORKSPACE", "") or os.environ.get("CR_WORKSPACE", "")

    # Stop sidecar before workspace cleanup.
    if resolved_workspace:
        from .sidecar import stop_sidecar
        stop_sidecar(resolved_workspace)

    if resolved_workspace and delete_workspace:
        ws = Path(resolved_workspace).expanduser()
        if ws.exists():
            shutil.rmtree(ws)
            click.echo(f"Workspace removed: {ws}")

    current = hive_context.load_current_context()
    if current.get("team") == name:
        hive_context.clear_current_context()

    click.echo(f"Team '{name}' deleted.")


@cli.command()
@click.argument("agent_name")
@click.option("--model", "-m", default="", help="Model ID")
@click.option("--prompt", "-p", default="", help="Initial prompt (typed into TUI after startup)")
@click.option("--cwd", default="", help="Working directory")
@click.option("--skill", default="hive", help="Base skill to load after startup ('none' to skip)")
@click.option("--workflow", default="", help="Workflow skill to load after the base skill")
@click.option("--env", "-e", multiple=True, help="Extra env vars (KEY=VALUE, repeatable)")
@click.option("--cli", "cli_name", type=click.Choice(["claude", "codex"]), default=None, help="Agent CLI to spawn (default: same as current pane)")
def spawn(agent_name: str, model: str, prompt: str,
          cwd: str, skill: str, workflow: str, env: tuple[str, ...], cli_name: str | None):
    """Spawn an agent pane.

    Creates a new tmux pane in the current window and starts the chosen
    agent CLI. By default spawns the same CLI as the current pane; use
    `--cli claude|codex` to pick a specific one. `--skill` loads
    a base skill on startup (`hive` by default), `--workflow` stacks a
    workflow skill on top, and `--prompt` sends an initial message.

    \b
    Examples:
      hive spawn dodo --cli codex
      hive spawn worker1 --prompt "start on task X" --workflow demo-review
      hive spawn claude -m claude-opus-4-7 --skill none
    """
    team_name, t = _resolve_scoped_team(None, required=True)
    assert team_name is not None and t is not None
    try:
        agent = _spawn_team_agent(
            t,
            team_name=team_name,
            agent_name=agent_name,
            model=model,
            prompt=prompt,
            cwd=cwd,
            skill=skill,
            workflow=workflow,
            env_entries=env,
            cli_name=cli_name,
        )
        click.echo(f"Agent '{agent_name}' spawned in pane {agent.pane_id}")
    except ValueError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)


@cli.command()
@click.argument("target_agent")
@click.option("--artifact", default="", help="Artifact path for handoff context")
@click.option("--note", default="", help="Short note appended to the standard handoff message")
@click.option("--reply-to", "reply_to_override", default="", help="Anchor msgId to delegate (default: latest unanswered inbound)")
@click.option("--spawn", "spawn_target", is_flag=True, help="Create a fresh worker before sending the handoff")
@click.option("--fork", "fork_target", is_flag=True, help="Fork the current session into a new worker before sending the handoff")
def handoff(
    target_agent: str,
    artifact: str,
    note: str,
    reply_to_override: str,
    spawn_target: bool,
    fork_target: bool,
):
    """Delegate a thread via send / spawn / fork wrapper.

    \b
    Three modes, chosen by flags:
      direct  (no flag)  target agent already exists in the team;
                         the anchor inbound is forwarded to them.
      --spawn            spin up a fresh agent pane first, then hand off.
      --fork             fork the current session into a new clone, then
                         hand off (preserves model + context).

    By default the anchor is the latest unanswered inbound to you; pass
    `--reply-to <msgId>` to pick a specific thread. `--note` appends a
    short comment to the standard handoff message; `--artifact` attaches
    a file.

    \b
    Examples:
      hive handoff dodo --artifact /tmp/task.md       # direct, dodo already there
      hive handoff worker1 --spawn --artifact /tmp/task.md
      hive handoff dodo-c1 --fork --note "continue this"
    """
    if spawn_target and fork_target:
        _fail("choose at most one of --spawn or --fork")

    team_name, t = _resolve_scoped_team(None, required=True)
    assert team_name is not None and t is not None
    sender = _resolve_sender(None)
    ws = _resolve_workspace(t, required=True)

    existing_target = _existing_team_agent(t, target_agent)
    if existing_target is not None:
        if spawn_target or fork_target:
            _fail(f"agent '{target_agent}' already exists; direct handoff does not accept --spawn/--fork")
        if target_agent == sender:
            _fail("cannot hand off to yourself; use --spawn or --fork with a new agent name")
    else:
        if not spawn_target and not fork_target:
            _fail(f"agent '{target_agent}' does not exist; pass --spawn or --fork explicitly")

    resolved_artifact = _resolve_artifact_path(artifact, workspace=ws)
    anchor_event = _resolve_handoff_anchor_event(
        ws,
        current_agent=sender,
        reply_to_override=reply_to_override,
    )
    anchor_msg_id = str(anchor_event.get("msgId") or "")
    original_sender = str(anchor_event.get("from") or "")
    if not anchor_msg_id or not original_sender:
        _fail("invalid anchor event for handoff")

    if existing_target is not None:
        mode = "direct"
        target_member = existing_target
    else:
        if spawn_target:
            mode = "spawn"
            target_member = _spawn_team_agent(
                t,
                team_name=team_name,
                agent_name=target_agent,
                cwd=os.getcwd(),
            )
        else:
            mode = "fork"
            target_member, _ = _fork_registered_agent(
                t=t,
                pane_id="",
                split="auto",
                join_as=target_agent,
            )

    delegate_body = _handoff_delegate_body(
        sender_agent=sender,
        original_sender=original_sender,
        anchor_msg_id=anchor_msg_id,
        note=note,
    )
    try:
        delegate_payload = _request_send_payload(
            workspace=ws,
            team=t,
            sender_agent=sender,
            target_agent=target_agent,
            body=delegate_body,
            artifact=resolved_artifact,
            command_name="handoff",
            warn_on_long_body=False,
        )
    except RuntimeError as exc:
        _fail(str(exc))
        return

    announce_msg_id = ""
    if original_sender == target_agent:
        announce_payload: dict[str, object] = {
            "delivery": "skipped",
            "reason": "target_is_original_sender",
        }
    else:
        try:
            announce_payload = _request_send_payload(
                workspace=ws,
                team=t,
                sender_agent=sender,
                target_agent=original_sender,
                body=_handoff_announce_body(target_agent=target_agent),
                reply_to=anchor_msg_id,
                command_name="handoff",
                warn_on_long_body=False,
            )
            announce_msg_id = str(announce_payload.get("msgId") or "")
        except RuntimeError as exc:
            announce_payload = {
                "delivery": "failed",
                "error": str(exc),
            }

    handoff_id = f"hf_{secrets.token_hex(4)}"
    bus.write_event(
        ws,
        from_agent=sender,
        to_agent=target_agent,
        intent="handoff",
        message_id=handoff_id,
        metadata={
            "anchorMsgId": anchor_msg_id,
            "mode": mode,
            "delegateMsgId": str(delegate_payload.get("msgId") or ""),
            "announceMsgId": announce_msg_id,
        },
    )
    payload = {
        "handoffId": handoff_id,
        "mode": mode,
        "target": target_agent,
        "targetPane": target_member.pane_id,
        "originalSender": original_sender,
        "anchorMsgId": anchor_msg_id,
        "delegate": delegate_payload,
        "announce": announce_payload,
    }
    click.echo(json.dumps(payload, indent=2, ensure_ascii=False))


@cli.group("config")
def config_cmd():
    """Read / write user-level settings (~/.hive/settings.json)."""
    pass


def _parse_config_value(raw: str):
    lowered = raw.strip().lower()
    if lowered == "true":
        return True
    if lowered == "false":
        return False
    try:
        return int(raw)
    except ValueError:
        pass
    try:
        return float(raw)
    except ValueError:
        pass
    return raw


@config_cmd.command("get")
@click.argument("key")
def config_get(key: str):
    """Print the value at KEY (dot-path). Exit 1 when unset."""
    from . import settings as user_settings
    value = user_settings.get_setting(key, _SENTINEL_CONFIG)
    if value is _SENTINEL_CONFIG:
        sys.exit(1)
    if isinstance(value, (dict, list)):
        click.echo(json.dumps(value, indent=2, sort_keys=True))
    else:
        click.echo(json.dumps(value))


@config_cmd.command("set")
@click.argument("key")
@click.argument("value")
def config_set(key: str, value: str):
    """Set KEY to VALUE (true/false/int/float/string)."""
    from . import settings as user_settings
    parsed = _parse_config_value(value)
    user_settings.set_setting(key, parsed)
    click.echo(json.dumps(parsed))


@config_cmd.command("unset")
@click.argument("key")
def config_unset(key: str):
    """Remove KEY. Exit 1 when KEY was not set."""
    from . import settings as user_settings
    if not user_settings.unset_setting(key):
        sys.exit(1)


_SENTINEL_CONFIG = object()


@config_cmd.command("roles")
@_json_default_options
def config_roles(plain: bool):
    """Show or configure per-role CLI + model overrides.

    Default output is the current role config as JSON (never interactive).
    `--plain` opens the interactive picker and requires a terminal.
    """
    from . import settings as user_settings

    if plain:
        if not (sys.stdin.isatty() and sys.stdout.isatty()):
            _fail("config roles --plain opens the interactive picker and requires a terminal; use the default JSON output when piped")
        _interactive_role_config()
        return

    result: dict[str, dict[str, object]] = {}
    for role in sorted(user_settings.CONFIGURABLE_ROLES):
        cli, model = user_settings.resolve_role_config(role)
        result[role] = {
            "cli": cli,
            "model": model,
            "applied": role in user_settings.APPLIED_ROLES,
        }
    click.echo(json.dumps(result, indent=2, sort_keys=True))


def _show_current_roles() -> None:
    from . import settings as user_settings

    rows = []
    for role in ("worker", "validator", "challenger", "orch"):
        cli, model = user_settings.resolve_role_config(role)
        note = "orch is always your current pane, not spawned" if role not in user_settings.APPLIED_ROLES else ""
        rows.append((role, cli or "—", model or "—", note))

    w_role = max(len(r[0]) for r in rows)
    w_cli = max(len(r[1]) for r in rows)
    w_model = max(len(r[2]) for r in rows)
    w_role = max(w_role, 4)
    w_cli = max(w_cli, 3)
    w_model = max(w_model, 5)

    header = f"  {'Role':<{w_role}}  {'CLI':<{w_cli}}  {'Model':<{w_model}}"
    sep = f"  {'─' * w_role}  {'─' * w_cli}  {'─' * w_model}"
    click.echo()
    click.echo(header)
    click.echo(sep)
    for role, cli, model, note in rows:
        line = f"  {role:<{w_role}}  {cli:<{w_cli}}  {model:<{w_model}}"
        if note:
            line += f"  ({note})"
        click.echo(line)
    click.echo()


def _term_menu(entries: list[str], title: str, *, cursor_index: int = 0) -> int | None:
    from simple_term_menu import TerminalMenu

    menu = TerminalMenu(
        entries,
        title=title,
        cursor_index=cursor_index,
        menu_cursor_style=("fg_cyan", "bold"),
        menu_highlight_style=("fg_cyan", "bold"),
    )
    idx = menu.show()
    return idx


def _interactive_role_config() -> None:
    from . import settings as user_settings
    from .agent_cli import AGENT_CLI_NAMES, MODEL_SUGGESTIONS

    applied = sorted(user_settings.APPLIED_ROLES)

    while True:
        _show_current_roles()

        role_entries = [*applied, "done"]
        idx = _term_menu(role_entries, "Configure which role?")
        if idx is None or role_entries[idx] == "done":
            break
        role = role_entries[idx]

        current_cli, current_model = user_settings.resolve_role_config(role)

        cli_action, model_action = _collect_role_choices(
            role, current_cli, current_model, AGENT_CLI_NAMES, MODEL_SUGGESTIONS,
        )

        _apply_role_action(f"roles.{role}.cli", cli_action)
        _apply_role_action(f"roles.{role}.model", model_action)

        final_cli, final_model = user_settings.resolve_role_config(role)
        click.echo(f"  ✓ {role}: cli={final_cli or 'default'}  model={final_model or 'default'}\n")


def _collect_role_choices(
    role: str,
    current_cli: str,
    current_model: str,
    agent_cli_names: frozenset[str],
    model_suggestions: dict[str, list[str]],
) -> tuple[tuple[str, str], tuple[str, str]]:
    """Prompt for CLI and model choices via terminal menus.

    Each action is ``("set", value)``, ``("keep", "")``, or ``("clear", "")``.
    Abort (Escape/q) at any point raises ``click.Abort``.
    """
    # --- CLI ---
    cli_sorted = sorted(agent_cli_names)
    cli_entries = []
    cli_cursor = 0
    for i, c in enumerate(cli_sorted):
        if c == current_cli:
            cli_entries.append(f"{c}  ← current")
            cli_cursor = i
        else:
            cli_entries.append(c)
    cli_entries += ["(keep)", "(clear)"]

    idx = _term_menu(cli_entries, f"  CLI for {role}:", cursor_index=cli_cursor)
    if idx is None:
        raise click.Abort()

    chosen = cli_entries[idx].split("  ←")[0].strip("()")
    if chosen == "clear":
        cli_action: tuple[str, str] = ("clear", "")
        effective_cli = ""
    elif chosen == "keep":
        cli_action = ("keep", "")
        effective_cli = current_cli
    else:
        cli_action = ("set", chosen)
        effective_cli = chosen

    # --- Model ---
    suggestions = model_suggestions.get(effective_cli, []) if effective_cli else []
    model_entries: list[str] = []
    if suggestions:
        for m in suggestions:
            model_entries.append(f"{m}  ← current" if m == current_model else m)
    model_entries += ["(custom)", "(keep)", "(clear)"]

    cursor = 0
    if current_model:
        if current_model in suggestions:
            cursor = suggestions.index(current_model)
        else:
            cursor = model_entries.index("(keep)")

    idx = _term_menu(model_entries, f"  Model for {role}:", cursor_index=cursor)
    if idx is None:
        raise click.Abort()

    chosen_model = model_entries[idx].split("  ←")[0].strip("()")
    model_action: tuple[str, str]
    if chosen_model == "clear":
        model_action = ("clear", "")
    elif chosen_model == "keep":
        model_action = ("keep", "")
    elif chosen_model == "custom":
        custom = click.prompt("  Enter model value")
        model_action = ("set", custom.strip()) if custom.strip() else ("keep", "")
    else:
        model_action = ("set", chosen_model)

    return cli_action, model_action


def _apply_role_action(key: str, action: tuple[str, str]) -> None:
    from . import settings as user_settings

    op, value = action
    if op == "set":
        user_settings.set_setting(key, value)
    elif op == "clear":
        user_settings.unset_setting(key)


@cli.command("wait-status", hidden=True, context_settings={"ignore_unknown_options": True, "allow_extra_args": True, "help_option_names": ["--help"]})
@click.argument("legacy_args", nargs=-1, type=click.UNPROCESSED)
def wait_status(legacy_args: tuple[str, ...]):
    """Removed legacy status polling command."""
    del legacy_args
    _status_migration_failure("wait-status")


@cli.command("inject")
@click.argument("agent_name")
@click.argument("text")
def inject_cmd(agent_name: str, text: str):
    """Debug: inject raw input into an agent pane.

    Writes text directly into the target pane without the `<HIVE>`
    envelope or delivery tracking. Use only when bypassing the message
    protocol for low-level debugging.

    \b
    Example:
      hive inject dodo "plain ping"
    """
    team_name, t = _resolve_scoped_team(None, required=True)
    assert team_name is not None and t is not None
    agent = t.get(agent_name)
    # Documented low-level bypass: raw composer keystrokes for every CLI, so
    # delivery paths (channel/RPC) can be debugged from outside themselves.
    _submit_interactive_text(agent.pane_id, text, agent.cli)
    result = {
        "member": agent_name,
        "action": "inject",
        "pane": getattr(agent, "pane_id", "") or "",
        "success": True,
    }
    click.echo(json.dumps(result, indent=2, ensure_ascii=False))


def _compact_target(target: _PaneTarget) -> str:
    """Run ``/compact`` on the literal pane. Returns the compaction status.

    Acts purely on ``target``'s pane facts — never re-resolves through Team
    state — so a pane shared by two same-named agents (Bug A) still compacts the
    pane in hand.
    """
    if target.cli == "codex":
        # codex: an idle agent compacts via the dedicated RPC
        # (thread/compact/start) — never a turn/start prompt, which only feeds
        # the model literal "/compact". When the agent is busy (compact_pane
        # returns non-"compacted") we do NOT queue or silently defer: a Compact
        # turn would abort the running turn, so instead we keystroke `/compact`
        # into codex's own TUI, which then shows its native "disabled while a
        # task is in progress." That is an explicit refusal the agent can see,
        # not a silent background compaction it never learns about.
        from .adapters import codex_app_server
        status = codex_app_server.compact_pane(target.pane_id)
        if status != "compacted":
            _submit_interactive_text(target.pane_id, "/compact", "codex")
        return status
    # claude (and embedded codex without a daemon): `/compact` is a TUI
    # slash command, so it must go through the composer. A channel message
    # would arrive as content, not as a command — this is startup/control
    # keystroke driving, not message delivery.
    _submit_interactive_text(target.pane_id, "/compact", target.cli)
    return "compacted"


@cli.command("compact")
@click.option("--pane", "pane_id", default="", help="Target pane ID (default: current pane via TMUX_PANE)")
def compact_cmd(pane_id: str):
    """Trigger /compact on your own pane.

    Works on any agent pane, team-bound or not: a pane with no Hive team is
    compacted by its literal pane facts, and the response carries `member` =
    the pane id with `team: null`.

    When wired into a tmux key binding, pass `--pane "#{pane_id}"` so the
    triggering pane is captured by tmux at keypress time rather than read
    from the (potentially stale) TMUX_PANE env in a detached subprocess.

    \b
    Examples:
      hive compact
      hive compact --pane %21
    """
    # Resolve the pane straight from its tmux options — both with an explicit
    # --pane and from the current pane. We never re-resolve through Team state:
    # that is the cross-window same-name bug PR #8 fixed for --pane, and routing
    # the no---pane path through pane facts too lets a non-team agent compact
    # itself (it has no team to resolve against).
    target = _resolve_pane_target(pane_id)
    status = _compact_target(target)
    result = {
        "member": target.member_label,
        "action": "compact",
        "pane": target.pane_id,
        "status": status,
        "success": status == "compacted",
    }
    if not target.is_team_bound:
        # Pane-only compact has no team identity; `member` is the pane id. Flag
        # the absent team explicitly so callers can tell it apart from a member.
        result["team"] = None
    click.echo(json.dumps(result, indent=2, ensure_ascii=False))


@cli.command("team")
def team_cmd():
    """Show team overview.

    Returns a JSON payload with `members[]`, `self` (your own name), the
    bound `tmuxSession` / `tmuxWindow`, `runtimeWorkspace`, and `cwd`.

    Each member row carries the runtime fields `busy`, `inputState`, and
    `turnPhase` — see docs/runtime-model.md for semantics. `self` is a
    string pointer: look yourself up in `members[]` for your own state.

    If the current tmux window has no team bound, returns a bootstrap
    payload instead: `team=null`, a pane list, and a `hint` telling you
    to run `hive init`.

    \b
    Examples:
      hive team                                # full payload when a team is bound
      hive team | jq '.members[] | select(.name=="dodo")'
    """
    _gc_dead_teams()
    discovered = _discover_tmux_binding()
    team_name = discovered.get("team")
    if not team_name and not tmux.is_inside_tmux():
        team_name = hive_context.load_current_context().get("team") or None
    if team_name:
        _, t = _resolve_scoped_team(str(team_name), required=False)
        if t is not None:
            click.echo(json.dumps(_team_status_payload(t), indent=2, ensure_ascii=False))
            return
    result: dict[str, object] = {"team": None}
    session_name = tmux.get_current_session_name()
    window_target = tmux.get_current_window_target()
    current_pane = tmux.get_current_pane_id()
    panes = tmux.list_panes_full(window_target) if window_target else []
    result["tmux"] = {
        "session": session_name,
        "window": window_target,
        "currentPane": current_pane,
        "panes": [
            {
                "id": p.pane_id,
                "command": p.command,
                "role": p.role or member_role_for_pane(p.pane_id),
                "agent": p.agent,
                "team": p.team,
            }
            for p in panes
        ],
        "paneCount": len(panes),
    }
    result["hint"] = "No team bound. The duo-vs-squad choice is the user's — ask them with the blocking question tool, then init accordingly. Don't pick or init on your own."
    window_id = tmux.get_current_window_id() or ""
    if session_name and window_id:
        result["runtimeWorkspace"] = str(_default_auto_workspace_path(session_name, window_id))
    click.echo(json.dumps(_add_runtime_location_fields(result), indent=2, ensure_ascii=False))


@cli.command(hidden=True)
def who():
    """Backward-compatible alias for `hive team`."""
    team_cmd.callback()  # type: ignore[attr-defined]


_LAYOUT_PRESETS = ("auto", "main-vertical", "main-horizontal", "tiled", "even-horizontal", "even-vertical")


@cli.command("layout")
@click.argument("preset", type=click.Choice(_LAYOUT_PRESETS, case_sensitive=False))
def layout_cmd(preset: str):
    """Apply a tmux layout preset to the current team window.

    Use ``auto`` to pick a preset adaptively from the window's aspect ratio.
    """
    _, t = _resolve_scoped_team(None, required=True)
    assert t is not None
    window_target = t.tmux_window or tmux.get_current_window_target() or ""
    if not window_target:
        _fail("Cannot determine tmux window target")
    if preset == "auto":
        from . import layout as layout_mod
        choice = layout_mod.apply_adaptive(window_target)
        if choice is None:
            click.echo(json.dumps({"layout": "", "window": window_target, "reason": "no-op"}))
            return
        click.echo(json.dumps({
            "layout": choice.preset,
            "orientation": choice.orientation,
            "window": window_target,
        }))
        return
    if preset in ("main-vertical", "main-horizontal"):
        dim = "main-pane-width" if preset == "main-vertical" else "main-pane-height"
        tmux.set_window_option(window_target, dim, "50%")
    tmux.select_layout(window_target, preset)
    click.echo(json.dumps({"layout": preset, "window": window_target}))


# --- CLI-shipped skill specs ---------------------------------------------
# Thin discovery stub (skills/hive/SKILL.md) points here; the volatile
# protocol/topology guidance ships inside the package and is fetched on
# demand, so it can never drift from the installed CLI version.

_SPEC_NAME_RE = re.compile(r"^[a-z0-9][a-z0-9-]*$")


def _spec_repo_dir() -> Path | None:
    """Repo specs dir when running from a checkout; else None (packaged)."""
    candidate = Path(__file__).resolve().parents[2] / "src" / "hive" / "core_assets" / "specs"
    return candidate if candidate.is_dir() else None


def _read_spec(name: str) -> str | None:
    repo = _spec_repo_dir()
    if repo is not None:
        path = repo / f"{name}.md"
        return path.read_text(encoding="utf-8") if path.is_file() else None
    from importlib import resources

    resource = resources.files("hive.core_assets").joinpath("specs", f"{name}.md")
    try:
        return resource.read_text(encoding="utf-8")
    except (FileNotFoundError, OSError):
        return None


def _list_specs() -> list[str]:
    repo = _spec_repo_dir()
    if repo is not None:
        return sorted(p.stem for p in repo.glob("*.md"))
    from importlib import resources

    names: list[str] = []
    try:
        for entry in resources.files("hive.core_assets").joinpath("specs").iterdir():
            if entry.name.endswith(".md"):
                names.append(entry.name[:-3])
    except (FileNotFoundError, OSError, NotADirectoryError):
        pass
    return sorted(names)


@cli.group("skills")
def skills_cmd():
    """CLI-shipped skill specs (version-locked, never drift). Start: `hive skills get core`."""


@skills_cmd.command("get")
@click.argument("name")
def skills_get_cmd(name: str):
    """Print spec NAME (e.g. `core`). Content always matches the installed CLI version."""
    if not _SPEC_NAME_RE.match(name):
        _fail(f"invalid spec name '{name}' (lowercase letters, digits, dashes only)")
    text = _read_spec(name)
    if text is None:
        available = ", ".join(_list_specs()) or "(none)"
        _fail(f"unknown spec '{name}'. available: {available}")
    click.echo(text)


@skills_cmd.command("list")
def skills_list_cmd():
    """List spec names available on this installed version."""
    click.echo(json.dumps({"specs": _list_specs()}, ensure_ascii=False, indent=2))


@skills_cmd.command("ls", hidden=True)
def skills_ls_cmd():
    """Hidden alias of `hive skills list`."""
    skills_list_cmd.callback()


def _inject_role_bootstrap(pane: str, role: str) -> bool:
    """Deliver the role bootstrap prompt to *pane* over its native transport.

    Same text a spawned pane gets as its launch prompt (identity +
    ``hive skills get <role>`` + idle discipline) — adoption only changes the
    delivery channel, never the wording. Returns True when the pane's
    transport accepted it; False when the pane runs no known agent CLI or the
    transport refused (delivery is native-only, no keystroke fallback).
    """
    from .agent import Agent, DeliveryError

    profile = detect_profile_for_pane(pane)
    if profile is None:
        return False
    adopter = Agent(name=role, team_name="", pane_id=pane, cli=profile.name)
    try:
        adopter.send(_role_bootstrap_prompt(role))
    except DeliveryError:
        return False
    return True


def _role_bootstrap_prompt(role: str) -> str:
    """Spawn first-message for a no-human role pane: identity + the one command
    that loads the role. The spec itself stays CLI-served — the spawned pane
    runs ``hive skills get <role>`` exactly like a dispatched pane does
    (`_inject_role_bootstrap`), so there is no inlined spec snapshot to keep in
    sync and the prompt stays short enough to inline into the launch command.
    """
    return (
        f"你是这个 team 的 {role}。先跑 `hive skills get {role}` 取你的角色协议 "
        f"—— 照它做。没有待办时结束当前 turn,让 pane 开着接收第一条任务消息"
        f"(orch / peer 会发来);在那之前别自己找活、别翻库、别 `sleep` 轮询。"
    )


@cli.group("duo")
def duo_cmd():
    """DUO atom (worker + anti-family validator) management.

    \b
    Examples:
      hive duo init                          # worker (= this pane) + anti-family validator
      hive duo init --validator-cli codex    # pick the validator CLI explicitly
      hive duo set-pr 57                     # pin PR 57 on the window status
    """


def _duo_neighbor_for_pairing(
    current_pane: str, panes: list[tmux.PaneInfo], my_family: str
) -> tmux.PaneInfo | None:
    """The sole other pane in the *panes* snapshot, if it qualifies as validator.

    Qualifies = an idle, unowned, anti-family agent. Returns None otherwise.
    Duo only ever conscripts here — the 2-pane case. 3+ panes break out
    rather than guess which neighbor to grab. Consumes the caller's already
    successful window snapshot instead of re-listing, so an unknown listing
    can never masquerade as "no neighbor".
    """
    others = [p for p in panes if p.pane_id != current_pane]
    if len(others) != 1:
        return None
    neighbor = others[0]
    if neighbor.team or neighbor.group:
        return None
    # Process evidence only: a retained shell with a stale CLI title must
    # not be conscripted as a validator.
    from .agent_cli import detect_cli_process_for_pane

    if detect_cli_process_for_pane(neighbor.pane_id) is None:
        return None
    other_family = family_for_pane(neighbor.pane_id)
    if my_family != "unknown" and other_family != "unknown" and my_family == other_family:
        return None
    if not _pane_is_idle_for_pairing(neighbor.pane_id):
        return None
    return neighbor


@dataclass
class _DuoPlacement:
    """Where a duo's worker lands and who its validator will be, decided
    *before* the team is created so team identity can derive from the final
    window (Bug A).
    """
    window: str
    worker_pane: str
    worker_cli: str
    worker_cwd: str
    validator_cli: str
    validator_model: str
    adopt_pane: str = ""
    adopt_cli: str = ""
    window_name: str = "duo"


_DUO_NAME_NOISE_PREFIXES = ("feat/", "fix/", "feature/", "bugfix/", "chore/", "hotfix/", "worktree-")


def _git_branch_for_cwd(cwd: str) -> str:
    """Current git branch of *cwd*, or "" if it is not a git repo / is detached."""
    if not cwd:
        return ""
    try:
        r = subprocess.run(
            ["git", "-C", cwd, "branch", "--show-current"],
            capture_output=True,
            text=True,
            timeout=2,
        )
    except (OSError, subprocess.SubprocessError):
        return ""
    return r.stdout.strip() if r.returncode == 0 else ""


def _duo_window_name(worker_cwd: str) -> str:
    """A meaningful tmux window label for a duo.

    The worker cwd's git branch is what the duo is actually working on (Hive's
    per-feature worktree workflow makes branch == feature), so use it — minus
    noise prefixes (``feat/``, ``worktree-``, ...). On a default branch
    (main/master) or outside git, fall back to the project (cwd basename); a
    bare "duo" tells you nothing.
    """
    branch = _git_branch_for_cwd(worker_cwd)
    if branch and branch not in ("main", "master"):
        for prefix in _DUO_NAME_NOISE_PREFIXES:
            if branch.startswith(prefix):
                branch = branch[len(prefix):]
                break
        if branch:
            return branch
    base = os.path.basename(worker_cwd.rstrip("/")) if worker_cwd else ""
    return base or "duo"


def _unique_duo_window_name(base: str, this_window: str) -> str:
    """Disambiguate *base* against other live windows.

    Same-repo, same-branch duos compute the same label, so on a collision append
    ``-2``, ``-3``, .... *this_window* is excluded so the duo never collides with
    its own (already-set) name.
    """
    taken = {name for target, name in tmux.list_window_names() if target != this_window}
    if base not in taken:
        return base
    n = 2
    while f"{base}-{n}" in taken:
        n += 1
    return f"{base}-{n}"


def _resolve_validator_cli_model(my_family: str, explicit_cli: str | None) -> tuple[str, str]:
    """Validator ``(cli, model)``: explicit flag > role settings > anti-family."""
    from . import settings as user_settings

    role_cli, role_model = user_settings.resolve_role_config("validator")
    if explicit_cli:
        return explicit_cli, role_model
    if role_cli:
        return role_cli, role_model
    return peer_cli_for_family(my_family), role_model


def _prepare_duo_placement(
    current_pane: str, *, validator_cli: str | None = None
) -> _DuoPlacement:
    """Decide duo placement without creating the team or tagging anything.

    Realized by the current window's pane count — 1: validator splits beside the
    worker; 2: adopt an idle/unowned/anti-family neighbor, else break out; 3+:
    break the worker to a fresh window. Breaking out here, before team creation,
    is what lets the team name/workspace follow the final window.
    """
    worker_cli = _resolve_spawn_cli_name(None)
    my_family = family_for_pane(current_pane)
    v_cli, v_model = _resolve_validator_cli_model(my_family, validator_cli)

    window = tmux.get_pane_window_target(current_pane) or ""
    if not window:
        _fail("cannot determine current window")
    # One successful snapshot drives both the pane count and the neighbor
    # choice: placement mutates windows (break_pane / spawn), so an unknown
    # listing must abort here instead of masquerading as a 1-pane window.
    panes = tmux.list_panes_full_or_none(window)
    if panes is None:
        _fail(f"tmux did not answer the pane listing for {window}; rerun init")
    if not any(p.pane_id == current_pane for p in panes):
        _fail(f"current pane {current_pane} missing from {window} listing; rerun init")
    count = len(panes)
    worker_cwd = tmux.display_value(current_pane, "#{pane_current_path}") or ""
    window_name = _duo_window_name(worker_cwd)

    # Decide adopt-vs-spawn before mutating any windows.
    adopt = _duo_neighbor_for_pairing(current_pane, panes, my_family) if count == 2 else None

    worker_pane = current_pane
    adopt_pane, adopt_cli = "", ""
    if adopt is not None:
        v_profile = detect_profile_for_pane(adopt.pane_id)
        adopt_pane = adopt.pane_id
        adopt_cli = v_profile.name if v_profile else "claude"
    elif count >= 2:
        # Crowded / unpairable window — isolate the worker, then spawn clean.
        new_window, worker_pane = tmux.break_pane(current_pane, name=window_name)
        if not new_window:
            _fail("failed to break out into a new window")
        window = new_window

    return _DuoPlacement(
        window=window,
        worker_pane=worker_pane,
        worker_cli=worker_cli,
        worker_cwd=worker_cwd,
        validator_cli=v_cli,
        validator_model=v_model,
        adopt_pane=adopt_pane,
        adopt_cli=adopt_cli,
        window_name=window_name,
    )


def _spawn_duo_validator(
    t: Team,
    *,
    window: str,
    worker_pane: str,
    worker_cwd: str,
    ws: str,
    cli: str,
    model: str,
    pane_count_after: int,
    allow_outside_tmux: bool = False,
) -> Agent:
    """Spawn a duo validator beside *worker_pane* and wire its identity.

    Shared by fresh duo formation and the init-time revive of a dead
    validator, so the spawn parameters can't drift between the two paths.
    Peer declaration and layout stay with the caller.
    """
    from . import layout as layout_mod

    validator_agent = Agent.spawn(
        name="validator",
        team_name=t.name,
        target_pane=worker_pane,
        cwd=worker_cwd,
        split_horizontal=layout_mod.split_horizontal(window, pane_count_after),
        split_size="50%",
        cli=cli,
        model=model,
        skill="none",
        prompt=_role_bootstrap_prompt("duo-validator"),
        workspace=ws,
        allow_outside_tmux=allow_outside_tmux,
    )
    t.agents["validator"] = validator_agent
    tmux.set_pane_option(validator_agent.pane_id, "hive-group", "duo")
    hive_context.save_context_for_pane(
        validator_agent.pane_id, team=t.name, workspace=ws, agent="validator"
    )
    return validator_agent


def _revive_missing_duo_validator(
    existing: dict[str, str], *, validator_cli: str | None = None
) -> dict[str, object] | None:
    """Respawn a duo's validator when the binding survived but its pane died.

    Runs on the idempotent init path. Conservative by contract: only a
    successful pane listing may prove the validator missing — a tmux failure
    is unknown and never spawns — and the loaded team must actually contain
    this worker. Returns the respawned validator descriptor, or None when
    nothing was (or could safely be) done.
    """
    if existing.get("group") != "duo" or existing.get("agent") != "worker":
        return None
    team_name = existing.get("team") or ""
    window = existing.get("tmuxWindow") or ""
    worker_pane = existing.get("pane") or ""
    if not team_name or not window or not worker_pane:
        return None
    panes = tmux.list_panes_full_or_none(window)
    if panes is None:
        return None  # tmux didn't answer: unknown is not missing
    me = next((p for p in panes if p.pane_id == worker_pane), None)
    if me is None or me.team != team_name or me.agent != "worker" or me.group != "duo":
        return None  # binding out of sync with the listing — don't guess
    if any(p.team == team_name and p.agent == "validator" for p in panes):
        return None
    try:
        t = Team.load(team_name, prefer_pane=worker_pane)
    except (FileNotFoundError, KeyError, ValueError):
        return None
    loaded_worker = t.agents.get("worker")
    if loaded_worker is None or loaded_worker.pane_id != worker_pane or t.tmux_window != window:
        return None  # stale team state — never spawn into the wrong pane/window
    # Workspace comes from the identity-verified team, not the binding payload
    # (whose window-option read folds tmux failures into ""); a validator
    # spawned with an unknown workspace would join with a broken context.
    ws = t.workspace or ""
    if not ws:
        return None
    worker_cwd = tmux.display_value(worker_pane, "#{pane_current_path}") or ws
    v_cli, v_model = _resolve_validator_cli_model(family_for_pane(worker_pane), validator_cli)
    validator_agent = _spawn_duo_validator(
        t,
        window=window,
        worker_pane=worker_pane,
        worker_cwd=worker_cwd,
        ws=ws,
        cli=v_cli,
        model=v_model,
        pane_count_after=len(panes) + 1,
    )
    # t was identity-checked above and _spawn_duo_validator registered the
    # validator in t.agents, so peer restoration must succeed — let it raise.
    t.set_peer("worker", "validator")
    from . import layout as layout_mod

    layout_mod.apply_adaptive(window)
    return {
        "pane": validator_agent.pane_id,
        "name": "validator",
        "cli": v_cli,
        "mode": "respawned",
    }


def _attach_duo_to_team(t: Team, *, placement: _DuoPlacement, ws: str) -> dict[str, object]:
    """Form a duo on the already-created (final-window) team *t*.

    Tags the worker, spawns or adopts the anti-family validator, declares the
    worker↔validator pair, and dispatches each role spec. The window's
    ``@hive-team`` / ``@hive-workspace`` options were already written by
    ``Team.create_for_window``; this only owns the member panes. Returns a
    descriptor for the caller to echo. Shared by `hive duo init` and `hive init`.
    """
    window = placement.window
    worker_pane = placement.worker_pane
    worker_cli = placement.worker_cli
    worker_cwd = placement.worker_cwd or ws

    # Label the window after what the duo is working on, not a generic "duo";
    # disambiguate same-branch siblings with a -N suffix.
    tmux.rename_window(window, _unique_duo_window_name(placement.window_name, window))
    tmux.configure_hive_window(window)
    tmux.set_pane_option(worker_pane, "hive-role", "agent")
    tmux.set_pane_option(worker_pane, "hive-agent", "worker")
    tmux.set_pane_option(worker_pane, "hive-team", t.name)
    tmux.set_pane_option(worker_pane, "hive-group", "duo")
    tmux.set_pane_option(worker_pane, "hive-cli", worker_cli)
    hive_context.save_context_for_pane(worker_pane, team=t.name, workspace=ws, agent="worker")
    _remember_context(team=t.name, workspace=ws, agent="worker")

    from . import layout as layout_mod

    if placement.adopt_pane:
        adopt_cwd = tmux.display_value(placement.adopt_pane, "#{pane_current_path}") or worker_cwd
        _register_agent_member(
            t,
            pane_id=placement.adopt_pane,
            team_name=t.name,
            agent_name="validator",
            pane_cli=placement.adopt_cli,
            cwd=adopt_cwd,
            notify=False,  # role loaded via /duo-validator dispatch below, not the generic hive join
            group="duo",
        )
        validator_pane, validator_cli_used, mode = placement.adopt_pane, placement.adopt_cli, "paired"
    else:
        validator_agent = _spawn_duo_validator(
            t,
            window=window,
            worker_pane=worker_pane,
            worker_cwd=worker_cwd,
            ws=ws,
            cli=placement.validator_cli,
            model=placement.validator_model,
            pane_count_after=2,
        )
        validator_pane, validator_cli_used, mode = validator_agent.pane_id, placement.validator_cli, "spawned"

    # Declare the worker ↔ validator pair (reload so both names are visible).
    try:
        reloaded = Team.load(t.name, prefer_pane=worker_pane)
        reloaded.set_peer("worker", "validator")
    except (FileNotFoundError, KeyError, ValueError):
        pass

    layout_mod.apply_adaptive(window)

    # Hand the validator its role: a spawned validator already got `hive
    # skills get duo-validator` as its startup prompt; an adopted idle
    # neighbor gets it injected here. The worker pane is the agent running
    # this very command — its role load is returned as `next` for it to run
    # in-turn, never injected into its input box as a fake user message.
    if mode == "paired":
        _inject_role_bootstrap(validator_pane, "duo-validator")
    dispatched: list[str] = ["validator"]

    tmux.select_window(window)

    return {
        "team": t.name,
        "window": window,
        "group": "duo",
        "worker": {"pane": worker_pane, "name": "worker", "cli": worker_cli},
        "validator": {
            "pane": validator_pane,
            "name": "validator",
            "cli": validator_cli_used,
            "mode": mode,
        },
        "dispatched": dispatched,
        "next": "hive skills get duo-worker",
    }


def _prepare_window_for_new_team(window_target: str, *, current_pane: str) -> None:
    """Clear a stale ``@hive-team`` tag on *window_target* so a new team can
    bind.

    Fails (rather than clobbering) when the window still hosts live members that
    the current pane isn't part of — that window owns a real team.
    """
    from .team import _window_has_live_team_members

    existing = tmux.get_window_option(window_target, "hive-team")
    if not existing:
        return
    if _window_has_live_team_members(window_target, existing):
        cur_team = tmux.get_pane_option(current_pane, "hive-team") if current_pane else None
        if cur_team != existing:
            _fail(
                f"tmux window '{window_target}' already hosts live Hive team "
                f"'{existing}' — run from a team pane, or start the duo elsewhere."
            )
        return
    for key in ("hive-team", "hive-workspace", "hive-desc", "hive-created", "hive-peers"):
        tmux.clear_window_option(window_target, f"@{key}")


def _claim_team_name(team_name: str, *, this_window: str, explicit: bool) -> None:
    """Guard a default/explicit team name that another window already owns.

    A stale duplicate (no live member panes) is cleared so the name can be
    claimed; a live duplicate is a hard error — names are never silently
    suffixed or clobbered.
    """
    from .team import _find_team_window, _gc_stale_team_windows, _window_has_live_team_members

    existing_wt, _ = _find_team_window(team_name)
    if not existing_wt or existing_wt == this_window:
        return
    if _window_has_live_team_members(existing_wt, team_name):
        hint = "choose a different --name" if explicit else "rerun from that window, or run `hive doctor`"
        _fail(f"team '{team_name}' already lives in tmux window '{existing_wt}' — {hint}.")
    _gc_stale_team_windows(team_name, keep=this_window, all_windows=[existing_wt])


def _create_standalone_duo(
    *,
    current_pane: str,
    validator_cli: str | None = None,
) -> dict[str, object]:
    """Shared duo bring-up for `hive init` and `hive duo init`.

    Decides placement (which may break the worker out to a fresh window),
    derives the team name + workspace from the *final* window, creates the team
    there, then forms the duo. Idempotent: if the current pane is already bound
    the existing binding is returned — reviving a dead validator first, so a
    duo worker can heal its pair by re-running init.
    """
    _gc_dead_teams()
    plugin_manager.cleanup_retired_plugins()

    existing = _discover_tmux_binding()
    if existing.get("team"):
        result: dict[str, object] = dict(existing)
        revived = _revive_missing_duo_validator(existing, validator_cli=validator_cli)
        if revived:
            result["validator"] = revived
        return result

    session_name = tmux.get_current_session_name() or "hive"
    placement = _prepare_duo_placement(current_pane, validator_cli=validator_cli)
    final_window = placement.window
    final_window_id = tmux.get_window_id(final_window) or ""
    final_index = final_window.rsplit(":", 1)[-1] if ":" in final_window else "0"

    team_name = _default_team_name_for_window(session_name, final_window_id, final_index)
    _prepare_window_for_new_team(final_window, current_pane=placement.worker_pane)
    _claim_team_name(team_name, this_window=final_window, explicit=False)

    ws_path = _default_auto_workspace_path(session_name, final_window_id, final_index)
    # A fresh duo on a reused window must not inherit the previous team's
    # event log or artifacts from the default auto workspace.
    from .sidecar import stop_sidecar

    stop_sidecar(str(ws_path))
    bus.reset_workspace(ws_path)

    try:
        t = Team.create_for_window(
            team_name,
            window_target=final_window,
            lead_pane_id=placement.worker_pane,
            lead_name="worker",
            description=f"auto-init from tmux {session_name} ({final_window})",
            workspace=str(ws_path),
            tag_lead=False,
        )
    except ValueError as e:
        _fail(str(e))
        raise AssertionError("unreachable")

    _remember_context(team=team_name, workspace=str(ws_path), agent="worker")
    result = _attach_duo_to_team(t, placement=placement, ws=str(ws_path))
    _ensure_team_sidecar(t, ws_path)
    return result


_CCD_SESSION_NAME = "hive-ccd"


def _probe_unix_socket(path: str) -> bool:
    """True when something is listening on *path* (connect succeeds)."""
    import socket as _socket

    s = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
    s.settimeout(1.0)
    try:
        s.connect(path)
        return True
    except OSError:
        return False
    finally:
        s.close()


def _discover_client_channel_socket() -> str:
    """The single live ``hive-client-*.sock``, or fail with guidance.

    Liveness is a real connect probe — a socket file surviving a kill -9'd
    session would otherwise shadow the live one forever. Ambiguity (several
    live desktop sessions) requires an explicit ``--channel``.
    """
    from .adapters import claude_channel

    channel_dir = claude_channel.channel_socket_path("").parent
    candidates = [
        str(sock)
        for sock in sorted(channel_dir.glob("hive-client-*.sock"))
        if _probe_unix_socket(str(sock))
    ]
    if len(candidates) == 1:
        return candidates[0]
    if not candidates:
        _fail(
            "no live client channel socket found; pass --channel <socket> "
            "(the path is in this session's hive-channel MCP instructions), "
            "or run inside a tmux session"
        )
    _fail(
        "multiple live client channel sockets found; pass --channel <socket>:\n  "
        + "\n  ".join(candidates)
    )
    raise AssertionError("unreachable")


def _existing_ccd_duo(channel_socket: str) -> dict[str, object] | None:
    """Idempotency for the desktop-led path: the saved default context names a
    live team whose anchor socket still links to *this* channel socket."""
    ctx = hive_context.load_current_context()
    team_name = ctx.get("team", "")
    if not team_name:
        return None
    try:
        t = Team.load(team_name)
    except (FileNotFoundError, ValueError):
        return None
    worker = t.agents.get("worker")
    if worker is None or tmux.get_pane_option(worker.pane_id, "hive-remote") != "channel":
        return None
    from .adapters import claude_channel

    link = claude_channel.channel_socket_path(worker.pane_id)
    try:
        if os.readlink(link) != channel_socket:
            return None
    except OSError:
        return None
    validator = t.agents.get("validator")
    return {
        "team": t.name,
        "window": t.tmux_window,
        "group": "duo",
        "worker": {"pane": worker.pane_id, "name": "worker", "cli": "claude", "remote": "channel"},
        "validator": (
            {"pane": validator.pane_id, "name": "validator", "cli": validator.cli}
            if validator
            else None
        ),
        "watch": f"tmux attach -t {_CCD_SESSION_NAME}",
    }


def _create_ccd_duo(*, channel_socket: str, validator_cli: str | None) -> dict[str, object]:
    """Desktop-led duo: an external Claude session (outside tmux) is the worker.

    The worker's tmux presence is an *anchor pane* — a plain shell in a
    detached ``hive-ccd`` session carrying the member tags plus
    ``@hive-remote=channel``, its channel socket/marker symlinked to the
    external session's ``hive-client-<pid>.sock``. Everything downstream
    (identity, routing, delivery, reaping, doctor) keeps working on pane
    authority; the validator is a normal spawned pane beside the anchor.
    """
    from .adapters import claude_channel

    _gc_dead_teams()
    plugin_manager.cleanup_retired_plugins()

    existing = _existing_ccd_duo(channel_socket)
    if existing is not None:
        return existing

    cwd = os.getcwd()
    if not tmux.has_session(_CCD_SESSION_NAME):
        tmux.new_session(_CCD_SESSION_NAME)
    window_name = _duo_window_name(cwd)
    window, anchor_pane = tmux.new_window(_CCD_SESSION_NAME, name=window_name, cwd=cwd)
    if not window or not anchor_pane:
        _fail(f"could not create a window in tmux session '{_CCD_SESSION_NAME}'")
    try:
        window_id = tmux.get_window_id(window) or ""
        final_index = window.rsplit(":", 1)[-1] if ":" in window else "0"
        team_name = _default_team_name_for_window(_CCD_SESSION_NAME, window_id, final_index)
        _claim_team_name(team_name, this_window=window, explicit=False)

        ws_path = _default_auto_workspace_path(_CCD_SESSION_NAME, window_id, final_index)
        from .sidecar import stop_sidecar

        stop_sidecar(str(ws_path))
        bus.reset_workspace(ws_path)

        t = Team.create_for_window(
            team_name,
            window_target=window,
            lead_pane_id=anchor_pane,
            lead_name="worker",
            description=f"desktop-led duo ({cwd})",
            workspace=str(ws_path),
            tag_lead=False,
            allow_outside_tmux=True,
        )

        tmux.rename_window(window, _unique_duo_window_name(window_name, window))
        tmux.configure_hive_window(window)
        tmux.set_pane_title(anchor_pane, "[worker]")
        tmux.set_pane_option(anchor_pane, "hive-role", "agent")
        tmux.set_pane_option(anchor_pane, "hive-agent", "worker")
        tmux.set_pane_option(anchor_pane, "hive-team", team_name)
        tmux.set_pane_option(anchor_pane, "hive-group", "duo")
        tmux.set_pane_option(anchor_pane, "hive-cli", "claude")
        tmux.set_pane_option(anchor_pane, "hive-remote", "channel")
        hive_context.save_context_for_pane(
            anchor_pane, team=team_name, workspace=str(ws_path), agent="worker"
        )
        _remember_context(team=team_name, workspace=str(ws_path), agent="worker")

        err = claude_channel.link_client_socket(anchor_pane, channel_socket)
        if err:
            raise RuntimeError(f"cannot link channel socket: {err}")

        v_cli, v_model = _resolve_validator_cli_model("anthropic", validator_cli)
        validator_agent = _spawn_duo_validator(
            t,
            window=window,
            worker_pane=anchor_pane,
            worker_cwd=cwd,
            ws=str(ws_path),
            cli=v_cli,
            model=v_model,
            pane_count_after=2,
            allow_outside_tmux=True,
        )
    except Exception as e:  # noqa: BLE001 — undo the half-built window
        tmux.kill_window(window)
        _fail(str(e))
        raise AssertionError("unreachable")

    try:
        reloaded = Team.load(team_name, prefer_pane=anchor_pane)
        reloaded.set_peer("worker", "validator")
    except (FileNotFoundError, KeyError, ValueError):
        pass

    from . import layout as layout_mod

    layout_mod.apply_adaptive(window)
    _ensure_team_sidecar(t, ws_path)

    return {
        "team": team_name,
        "window": window,
        "group": "duo",
        "worker": {"pane": anchor_pane, "name": "worker", "cli": "claude", "remote": "channel"},
        "validator": {
            "pane": validator_agent.pane_id,
            "name": "validator",
            "cli": v_cli,
            "mode": "spawned",
        },
        "dispatched": ["validator"],
        "watch": f"tmux attach -t {_CCD_SESSION_NAME}",
        "next": "hive skills get duo-worker",
    }


@duo_cmd.command("init")
@click.option(
    "--validator-cli",
    type=click.Choice(["claude", "codex"]),
    default=None,
    help="CLI for validator (default: anti-family of current pane's CLI)",
)
@click.option(
    "--channel",
    default="",
    help="Outside tmux only: this Claude session's channel socket "
    "(hive-client-<pid>.sock, printed in the hive-channel MCP instructions)",
)
def duo_init_cmd(validator_cli: str | None, channel: str):
    """Set up a duo from the current pane: worker (=this pane) + anti-family validator.

    Equivalent to `hive init`. The current pane must be running
    an agent CLI (claude / codex); it becomes the worker. Realized by
    the current window's pane count:

      1 pane   → split-spawn the validator beside the worker
      2 panes  → adopt the neighbor as validator if it's an idle, unowned,
                 anti-family agent; otherwise treat as 3+
      3+ panes → break the worker out to a fresh window, then spawn

    The validator runs the anti-family CLI (claude↔codex) so review stays
    independent.

    Outside tmux (Claude Code desktop) this forms a desktop-led duo instead:
    this session becomes the worker over its channel socket, the validator
    spawns in a background detached tmux session (`tmux attach -t hive-ccd`
    to watch).
    """
    _run_duo_init(validator_cli, channel=channel)


# Replaces the bare index token in a window-status format with a conditional
# that renders `PR<n>` for windows carrying `@hive-pr`. `##I` is tmux's escaped
# literal `#I`, not the index token — the lookbehind leaves it alone (the
# pathological `###I` triple is intentionally unsupported: a conservative
# no-replace beats corrupting a user's format).
_WINDOW_INDEX_TOKEN_RE = re.compile(r"(?<!#)#I")
_PR_INDEX_TOKEN = "#{?#{@hive-pr},PR#{@hive-pr},#I}"


def _derive_pr_window_status(global_format: str | None) -> str | None:
    """Per-window status format derived from the *global* value; None = skip.

    Skips when the global format already references ``@hive-pr`` (the user
    wired the display themselves) and when it has no replaceable ``#I``.
    Deriving from the global value — never the window-local one — keeps
    repeated ``set-pr`` calls idempotent instead of recursively wrapping
    prior derived output.
    """
    if not global_format:
        return None
    if "@hive-pr" in global_format:
        return None
    if not _WINDOW_INDEX_TOKEN_RE.search(global_format):
        return None
    return _WINDOW_INDEX_TOKEN_RE.sub(_PR_INDEX_TOKEN, global_format)


def _feature_title_for_cwd(cwd: str) -> str:
    """Feature branch of *cwd* when it is a hive-started worktree, else "".

    Only branches carrying real ``hive-*`` metadata qualify — a bare
    ``gh-merge-base`` (left behind after ``worktree done``) is not evidence,
    and neither is an arbitrary user branch. Best-effort: any probe failure
    means "no title", never a failed set-pr.
    """
    branch = _git_branch_for_cwd(cwd)
    if not branch:
        return ""
    from . import worktree as wt_mod

    try:
        meta = wt_mod.read_meta(wt_mod.repo_anchor(cwd), branch)
    except wt_mod.WorktreeError:
        return ""
    return branch if any(key in meta for key in wt_mod.META_KEYS) else ""


@duo_cmd.command("set-pr")
@click.argument("number", type=int)
@click.argument("title", required=False, default=None)
@_json_default_options
def duo_set_pr_cmd(number: int, title: str | None, plain: bool):
    """Label the current duo window with its PR number and rename it to the feature.

    Run right after ``gh pr create --draft`` — writes ``@hive-pr`` on the
    current tmux window and installs a per-window status-bar display derived
    from the global ``window-status-format`` / ``window-status-current-format``
    (the index position renders ``PR<n>``; user styling and padding are
    preserved). The window is renamed to TITLE when provided (short kebab-case
    recommended — this is a tmux tab, not a PR description); without TITLE it
    is renamed to the cwd's hive feature branch when there is one, else left
    alone. Idempotent — re-running replaces the stamp and re-derives the
    display.
    """
    if not tmux.is_inside_tmux():
        _fail("must run inside tmux")
    if number <= 0:
        _fail(f"PR number must be a positive integer, got {number}")
    window = tmux.get_current_window_target() or ""
    if not window:
        _fail("cannot determine current window")
    if not tmux.get_window_option(window, "hive-team"):
        _fail(
            "current window is not a hive team window (no @hive-team); "
            "run set-pr from your duo window"
        )
    tmux.set_window_option(window, "@hive-pr", str(number))
    if not title:
        title = _feature_title_for_cwd(os.getcwd()) or None
    if title:
        tmux.rename_window(window, title)
    display: dict[str, str] = {}
    for option in ("window-status-format", "window-status-current-format"):
        global_format = tmux.get_global_window_option(option)
        derived = _derive_pr_window_status(global_format)
        if derived is None:
            already = bool(global_format and "@hive-pr" in global_format)
            display[option] = "already-global" if already else "skipped-no-index-token"
            continue
        tmux.set_window_option(window, option, derived)
        display[option] = "derived"
    if plain:
        summary = ", ".join(f"{key}={value}" for key, value in display.items())
        title_note = f", title={title}" if title else ""
        click.echo(f"window {window} labeled @hive-pr={number}{title_note} ({summary})")
    else:
        result: dict[str, object] = {"window": window, "pr": number, "display": display}
        if title:
            result["title"] = title
        click.echo(json.dumps(result, indent=2))


@duo_cmd.command("clear-pr")
@_json_default_options
def duo_clear_pr_cmd(plain: bool):
    """Clear the current duo window's PR number stamp."""
    if not tmux.is_inside_tmux():
        _fail("must run inside tmux")
    window = tmux.get_current_window_target() or ""
    if not window:
        _fail("cannot determine current window")
    if not tmux.get_window_option(window, "hive-team"):
        _fail(
            "current window is not a hive team window (no @hive-team); "
            "run clear-pr from your duo window"
        )
    previous = tmux.get_window_option(window, "hive-pr")
    tmux.clear_window_option(window, "@hive-pr")
    if not plain:
        click.echo(json.dumps({"window": window, "previous": previous}, indent=2))
    elif previous:
        click.echo(f"window {window} cleared @hive-pr={previous}")
    else:
        click.echo(f"window {window} had no @hive-pr stamp to clear")


# --- hive ls / hive resume: durable team snapshots ---


def _live_anchor_pane(members: dict[str, tmux.PaneInfo]) -> tmux.PaneInfo:
    """The pane whose cwd best represents what a live team is working on."""
    for pick in ("worker", "orch"):
        if pick in members:
            return members[pick]
    return members[sorted(members)[0]]


def _live_team_context(members: dict[str, tmux.PaneInfo]) -> dict[str, str]:
    """repo/branch context for a live team, resolved once from its anchor pane."""
    from . import resume as resume_store

    cwd = tmux.display_value(_live_anchor_pane(members).pane_id, "#{pane_current_path}") or ""
    return {
        "repoCwd": cwd,
        "repo": resume_store.repo_label(cwd),
        "branch": resume_store.git_branch(cwd),
    }


def _sorted_member_rows(rows: list[dict[str, object]]) -> list[dict[str, object]]:
    return sorted(rows, key=lambda m: (m.get("name") != "worker", str(m.get("name"))))


def _build_ls_payload() -> dict[str, object]:
    """Merged view of live team windows and persisted resume snapshots."""
    from . import resume as resume_store

    snapshots = resume_store.list_snapshots()
    panes, pane_status = tmux.list_panes_all_status()
    windows, win_status = tmux.list_team_windows_status()
    statuses = {pane_status, win_status}
    if statuses == {"ok"}:
        tmux_status = "ok"
    elif "unknown" in statuses:
        tmux_status = "unknown"
    else:
        tmux_status = "no-server"

    live_members: dict[str, dict[str, tmux.PaneInfo]] = {}
    win_by_team: dict[str, dict[str, str]] = {}
    if tmux_status == "ok":
        for p in panes or []:
            if p.team and p.agent:
                live_members.setdefault(p.team, {})[p.agent] = p
        for w in windows or []:
            win_by_team.setdefault(w["team"], w)

    teams: list[dict[str, object]] = []
    consumed_live: set[str] = set()
    for snap in snapshots:
        handle = str(snap.get("handle", ""))
        if snap.get("corrupt"):
            teams.append({"handle": handle, "state": "corrupt"})
            continue
        team_name = str(snap.get("team", ""))
        entry: dict[str, object] = {
            "handle": handle,
            "shortId": resume_store.short_id(snap),
            "team": team_name,
            "savedAt": snap.get("savedAt", ""),
            "branch": snap.get("branch", ""),
            "repoCwd": snap.get("repoCwd", ""),
            "repo": snap.get("repo", "") or resume_store.repo_label(str(snap.get("repoCwd", ""))),
            "pr": snap.get("pr", ""),
            "windowName": snap.get("windowName", ""),
            "members": _sorted_member_rows([
                {
                    "name": m.get("name", ""),
                    "cli": m.get("cli", ""),
                    "model": m.get("model", ""),
                    "session": bool(m.get("sessionId")),
                }
                for m in snap.get("members", [])
            ]),
        }
        if tmux_status == "unknown":
            # A failed listing can't distinguish live from dead — never
            # report a possibly-live team as restorable.
            entry["state"] = "unknown"
        else:
            live = live_members.get(team_name, {})
            win = win_by_team.get(team_name, {})
            same_instance = not win.get("created") or not snap.get("createdAt") or str(
                win.get("created")
            ) == str(snap.get("createdAt"))
            if live and same_instance:
                for m in entry["members"]:  # type: ignore[union-attr]
                    m["live"] = m["name"] in live
                missing = [m.get("name") for m in snap.get("members", []) if m.get("name") not in live]
                entry["window"] = win.get("window", "")
                entry["state"] = "live-incomplete" if missing else "live-complete"
                # Live truth beats whatever the snapshot recorded back then.
                # For pr, present-and-empty IS the truth (hive duo clear-pr);
                # only a row lacking the key (old fixture/data) keeps the
                # snapshot value.
                entry["windowName"] = win.get("windowName", "") or entry["windowName"]
                if "pr" in win:
                    entry["pr"] = win["pr"]
                entry.update(_live_team_context(live))
                consumed_live.add(team_name)
            elif live:
                # A different live instance owns the name: the old snapshot is
                # superseded, but the live team itself still gets its own row.
                entry["state"] = "superseded"
            else:
                entry["state"] = "restorable"
        teams.append(entry)

    if tmux_status == "ok":
        for team_name, members in sorted(live_members.items()):
            if team_name in consumed_live:
                continue
            win = win_by_team.get(team_name, {})
            row: dict[str, object] = {
                "handle": "",
                "team": team_name,
                "state": "live-complete",
                "window": win.get("window", ""),
                "windowName": win.get("windowName", ""),
                "pr": win.get("pr", ""),
                "members": _sorted_member_rows([
                    {"name": n, "cli": p.cli, "live": True}
                    for n, p in members.items()
                ]),
            }
            row.update(_live_team_context(members))
            teams.append(row)

    return {"tmux": tmux_status, "teams": teams}


@cli.command("ls")
@_json_default_options
def ls_cmd(plain: bool):
    """List hive teams: live windows plus resumable snapshots.

    Live teams show their window; dead ones show the handle to pass to
    ``hive resume``. Works outside tmux too (everything persisted is
    listed; nothing can be live without a server).
    """
    payload = _build_ls_payload()
    if not plain:
        click.echo(json.dumps(payload, indent=2, ensure_ascii=False))
        return
    for line in _format_ls_human(payload):
        click.echo(line)


def _ls_row_label(entry: dict[str, object]) -> str:
    """`repo @ branch` as the row's identity, degrading to what exists."""
    repo = str(entry.get("repo") or "")
    branch = str(entry.get("branch") or "")
    if repo and branch:
        label = f"{repo} @ {branch}"
    else:
        label = repo or branch or str(entry.get("team") or entry.get("handle") or "?")
    window_name = str(entry.get("windowName") or "")
    if window_name and window_name != repo:
        label = f"{label} ({window_name})"
    if entry.get("pr"):
        label = f"{label}  PR#{entry['pr']}"
    return label


def _ls_roster(entry: dict[str, object]) -> str:
    members = entry.get("members") or []
    return "+".join(
        str(m.get("cli") or m.get("name") or "?") for m in members  # worker-first already
    )


def _resume_ref_hint(entry: dict[str, object], teams: list[dict[str, object]]) -> str:
    """The copyable resume command for a row — short id only when it resolves.

    The short id is shown only if it is unique among valid snapshots and does
    not collide with another row's exact handle (exact handles win in the
    resolver, so such a command would resume something else).
    """
    handle = str(entry.get("handle") or "")
    sid = str(entry.get("shortId") or "")
    if sid:
        others = [e for e in teams if e is not entry]
        unique = all(str(e.get("shortId") or "") != sid for e in others)
        shadowed = any(str(e.get("handle") or "") == sid for e in others)
        if unique and not shadowed:
            return f"hive resume {sid}"
    return f"hive resume {handle}"


def _resolve_short_resume_ref(ref: str) -> dict[str, object] | None:
    """Snapshot whose short id is *ref*, or None; ambiguity fails loudly."""
    from . import resume as resume_store

    matches = [
        s for s in resume_store.list_snapshots()
        if not s.get("corrupt") and resume_store.short_id(s) == ref
    ]
    if len(matches) > 1:
        cmds = "; ".join(f"hive resume {s['handle']}" for s in matches)
        _fail(f"short id '{ref}' is ambiguous — use a full handle: {cmds}")
    return matches[0] if matches else None


def _format_ls_human(payload: dict[str, object]) -> list[str]:
    teams = list(payload.get("teams") or [])
    if not teams:
        return ["no hive teams (live or resumable)"]
    lines: list[str] = []
    if payload.get("tmux") == "unknown":
        lines.append("! tmux did not answer — live/dead state unknown this pass")

    live = [e for e in teams if e.get("state") in ("live-complete", "live-incomplete")]
    restorable = [e for e in teams if e.get("state") == "restorable"]
    other = [e for e in teams if e not in live and e not in restorable]

    from . import resume as resume_store

    if live:
        lines.append("LIVE")
        for e in sorted(live, key=lambda e: str(e.get("window", ""))):
            row = f"  {e.get('window') or '?'}  {_ls_row_label(e)}  {e.get('team')} · {_ls_roster(e)}"
            if e.get("state") == "live-incomplete":
                missing = [
                    str(m.get("name")) for m in e.get("members", []) if not m.get("live")
                ]
                row += f"  ! missing {'+'.join(missing)} → {_resume_ref_hint(e, teams)}"
            lines.append(row)
    if restorable:
        if lines and lines[-1] != "":
            lines.append("")
        lines.append("RESTORABLE  — hive resume <handle>")
        for e in sorted(restorable, key=lambda e: str(e.get("savedAt", "")), reverse=True):
            lines.append(
                f"  {e.get('handle')}  {_ls_row_label(e)}  "
                f"saved {resume_store.age(str(e.get('savedAt') or ''))} · {_ls_roster(e)}"
                f"  → {_resume_ref_hint(e, teams)}"
            )
    if other:
        if lines and lines[-1] != "":
            lines.append("")
        lines.append("OTHER")
        for e in other:
            what = "unreadable snapshot" if e.get("state") == "corrupt" else str(e.get("state"))
            lines.append(f"  {e.get('handle') or e.get('team')}  {what}  {_ls_roster(e)}")
    return lines


def _resume_progress(message: str) -> None:
    """Stage feedback for `hive resume` on stderr — stdout stays JSON-only.

    Resuming replays whole agent sessions (a long claude transcript alone can
    take tens of seconds, members start serially), so a silent prompt reads
    as a hang.
    """
    click.echo(message, err=True)


def _resume_member_order(members: list[dict[str, str]]) -> list[dict[str, str]]:
    return sorted(members, key=lambda m: m.get("name") != "worker")


def _resume_members_into_live_team(
    snap: dict[str, object],
    win: dict[str, str],
    live: dict[str, tmux.PaneInfo],
    missing: list[dict[str, str]],
    fresh_members: frozenset[str] = frozenset(),
) -> dict[str, object]:
    """Spawn *missing* members back into a live team.

    Members with a saved session resume it; a member in *fresh_members*
    (validator without a sessionId) is spawned fresh with its role bootstrap,
    following the live worker's current cwd.
    """
    from . import layout as layout_mod

    team_name = str(snap["team"])
    window = win["window"]
    ws = win.get("workspace") or ""
    if not ws:
        _fail(f"live team window {window} has no workspace binding; not guessing")
    anchor_pane = next(iter(live.values())).pane_id
    snap_worker_cwd = next(
        (str(m.get("cwd") or "") for m in snap.get("members", []) if m.get("name") == "worker"), ""  # type: ignore[union-attr]
    )
    spawned: list[tuple[str, str]] = []
    peer_team = None
    prior_peers: dict[str, str] | None = None
    try:
        # Same reason as the full restore: revive happens where the team
        # lives, so put the human in front of it.
        tmux.select_window(window)
        count = len(live)
        for m in _resume_member_order(missing):
            count += 1
            name = str(m["name"])
            fresh = name in fresh_members
            if fresh:
                _resume_progress(
                    f"spawning fresh {name} ({m['cli']}) — no saved session, starting clean…"
                )
                live_worker = live.get("worker")
                live_cwd = (
                    tmux.display_value(live_worker.pane_id, "#{pane_current_path}") if live_worker else ""
                )
                cwd = live_cwd or snap_worker_cwd
                session_kwargs: dict[str, str] = {"prompt": _role_bootstrap_prompt("duo-validator")}
            else:
                _resume_progress(
                    f"resuming {name} ({m['cli']}) — replaying its session, this can take a while…"
                )
                cwd = str(m["cwd"])
                session_kwargs = {"session_id": str(m["sessionId"]), "session_mode": "resume"}
            agent = Agent.spawn(
                name=name,
                team_name=team_name,
                target_pane=anchor_pane,
                cwd=cwd,
                split_horizontal=layout_mod.split_horizontal(window, count),
                split_size="50%",
                cli=str(m["cli"]),
                model=str(m.get("model", "")),
                skill="none",
                workspace=ws,
                **session_kwargs,
            )
            # Track the pane the moment it exists: every later failure —
            # tagging, context, peer, layout — must be able to kill it.
            spawned.append((str(m["name"]), agent.pane_id))
            _resume_progress(f"{m['name']} ready in {agent.pane_id}")
            tmux.set_pane_option(agent.pane_id, "hive-group", "duo")
            hive_context.save_context_for_pane(
                agent.pane_id, team=team_name, workspace=ws, agent=str(m["name"])
            )
        peer_team = Team.load(team_name, prefer_pane=anchor_pane)
        prior_peers = dict(peer_team.peer_map)
        if "worker" in peer_team.agents and "validator" in peer_team.agents:
            peer_team.set_peer("worker", "validator")
        layout_mod.apply_adaptive(window)
    except Exception as e:  # noqa: BLE001 — any failure must clean up the new panes only
        for _name, pane in spawned:
            tmux.kill_pane(pane)
        if peer_team is not None and prior_peers is not None and peer_team.peer_map != prior_peers:
            try:
                peer_team.peer_map = prior_peers
                peer_team.save()
            except Exception:
                pass
        _fail(f"resume failed while reviving members: {e} (survivors untouched, snapshot kept)")
    return {
        "resumed": "members",
        "team": team_name,
        "window": window,
        "members": [
            {"name": name, "pane": pane, "session": "fresh" if name in fresh_members else "resumed"}
            for name, pane in spawned
        ],
    }


def _resume_full_team(
    handle: str, snap: dict[str, object], fresh_members: frozenset[str] = frozenset()
) -> dict[str, object]:
    """Rebuild a dead team in a fresh window; transactional per VAL.

    Members with a saved session resume it; a member in *fresh_members*
    (validator without a sessionId) is spawned fresh with its role bootstrap
    in the snapshot worker's cwd.
    """
    from . import layout as layout_mod
    from . import resume as resume_store

    team_name = str(snap["team"])
    ws = str(snap.get("workspace") or "")
    if not ws:
        _fail("snapshot has no workspace; cannot resume")
    members = _resume_member_order(list(snap.get("members", [])))  # type: ignore[arg-type]
    worker_cwd = str(members[0]["cwd"])  # worker-first order; preflight proved it exists
    session_name = tmux.get_current_session_name() or "hive"
    window_name = str(snap.get("windowName") or "duo")

    window, first_pane = tmux.new_window(session_name, name=window_name, cwd=str(members[0]["cwd"]), detach=True)
    if not window or not first_pane:
        _fail("failed to create a window for the resumed team")
    _resume_progress(f"window {window} created — switching there")
    try:
        # The human asked for this team: take them to it and let them watch
        # the members come up instead of staring at a silent prompt.
        tmux.select_window(window)
        _prepare_window_for_new_team(window, current_pane=first_pane)
        _claim_team_name(team_name, this_window=window, explicit=True)
        bus.init_workspace(Path(ws))
        t = Team.create_for_window(
            team_name,
            window_target=window,
            lead_pane_id=first_pane,
            lead_name=str(members[0]["name"]),
            description=f"resumed from snapshot {handle}",
            workspace=ws,
            tag_lead=False,
        )
        results: list[tuple[str, str]] = []
        for i, m in enumerate(members):
            name = str(m["name"])
            fresh = name in fresh_members
            if fresh:
                _resume_progress(
                    f"spawning fresh {name} ({m['cli']}) — no saved session, starting clean…"
                )
                session_kwargs: dict[str, str] = {"prompt": _role_bootstrap_prompt("duo-validator")}
            else:
                _resume_progress(
                    f"resuming {name} ({m['cli']}) — replaying its session, this can take a while…"
                )
                session_kwargs = {"session_id": str(m["sessionId"]), "session_mode": "resume"}
            agent = Agent.spawn(
                name=name,
                team_name=team_name,
                target_pane=first_pane,
                cwd=(worker_cwd if fresh else str(m["cwd"])),
                split_window=i > 0,
                split_horizontal=layout_mod.split_horizontal(window, i + 1),
                split_size="50%",
                cli=str(m["cli"]),
                model=str(m.get("model", "")),
                skill="none",
                workspace=ws,
                **session_kwargs,
            )
            tmux.set_pane_option(agent.pane_id, "hive-group", "duo")
            hive_context.save_context_for_pane(
                agent.pane_id, team=team_name, workspace=ws, agent=name
            )
            results.append((name, agent.pane_id))
            _resume_progress(f"{name} ready in {agent.pane_id}")
        reloaded = Team.load(team_name, prefer_pane=first_pane)
        if "worker" in reloaded.agents and "validator" in reloaded.agents:
            reloaded.set_peer("worker", "validator")
        layout_mod.apply_adaptive(window)
        # Commit the continuation identity BEFORE the sidecar starts: its
        # writer fires immediately and would otherwise see the old createdAt
        # and archive the very snapshot being continued.
        continued = dict(snap)
        continued["createdAt"] = str(t.created_at)
        continued["windowName"] = window_name
        saved = resume_store.save_snapshot(continued, now=_utc_now_iso(), archive_on_new_instance=False)
        if saved == "rejected":
            raise RuntimeError("continuation snapshot rejected by the store")
        _ensure_team_sidecar(t, Path(ws))
    except Exception as e:  # noqa: BLE001 — roll the whole window back, keep the snapshot
        tmux.kill_window(window)
        _fail(f"resume failed: {e} — new window removed, snapshot kept for retry")
    return {
        "resumed": "full",
        "team": team_name,
        "window": window,
        "workspace": ws,
        "members": [
            {"name": name, "pane": pane, "session": "fresh" if name in fresh_members else "resumed"}
            for name, pane in results
        ],
    }


def _utc_now_iso() -> str:
    import datetime as _dt

    return _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


@cli.command("resume")
@click.argument("handle", required=False, default="")
def resume_cmd(handle: str):
    """Rebuild a dead team — or revive its missing members — from a snapshot.

    The worker comes back with its original agent session (claude ``-r``,
    codex ``resume``) in its original working directory. A validator whose
    snapshot has no saved session is respawned fresh with its role bootstrap,
    following the worker's cwd. Run ``hive ls`` to see resumable handles.
    """
    if not handle:
        _fail("missing handle — run `hive ls` to see resumable teams")
    if not tmux.is_inside_tmux():
        _fail("hive resume requires a tmux session")
    from . import resume as resume_store
    from .agent import CLI_BINS

    snap = resume_store.load_snapshot(handle)
    if snap is None:
        # Not an exact handle: try the 4-char short id shown by `hive ls`.
        snap = _resolve_short_resume_ref(handle)
    if snap is None:
        _fail(f"no usable snapshot for '{handle}' — run `hive ls` (missing or corrupt)")
    handle = str(snap["handle"])
    if snap.get("group") != "duo":
        _fail(f"snapshot group '{snap.get('group')}' is not resumable yet (duo only)")
    members = list(snap.get("members", []))
    names = [str(m.get("name") or "") for m in members]
    if sorted(names) != ["validator", "worker"]:
        _fail("snapshot roster must be exactly worker + validator (duo contract)")
    roster = dict(zip(names, members))
    if not roster["worker"].get("sessionId"):
        _fail(
            "cannot resume with original context: missing sessionId for worker "
            "— start a fresh duo instead (hive duo init)"
        )
    # A validator without a saved session cannot be resumed, but the worker's
    # context is the whole point of resume: bring the validator back fresh.
    fresh_members = frozenset(n for n, m in zip(names, members) if not m.get("sessionId"))
    bad_cli = [n for n, m in zip(names, members) if m.get("cli") not in CLI_BINS]
    if bad_cli:
        _fail(f"unsupported cli for member(s): {', '.join(bad_cli)}")
    gone_cwd = [
        n
        for n, m in zip(names, members)
        if n not in fresh_members and not os.path.isdir(str(m.get("cwd") or ""))
    ]
    if gone_cwd:
        _fail(
            f"working directory missing on disk for: {', '.join(gone_cwd)} "
            "— restore it first (deleted worktree?)"
        )

    panes, pane_status = tmux.list_panes_all_status()
    windows, win_status = tmux.list_team_windows_status()
    if pane_status != "ok" or win_status != "ok":
        _fail("tmux did not answer the pane/window listing; rerun hive resume")

    team_name = str(snap["team"])
    live = {p.agent: p for p in panes or [] if p.team == team_name and p.agent}
    win = next((w for w in windows or [] if w["team"] == team_name), None)

    if live:
        if win is None:
            _fail(f"live members of '{team_name}' found but no team window — inconsistent tmux state; not guessing")
        if (
            win.get("created")
            and snap.get("createdAt")
            and str(win["created"]) != str(snap["createdAt"])
        ):
            _fail(
                f"a different live team already owns '{team_name}' — this snapshot is superseded; nothing to resume"
            )
        extras = sorted(n for n in live if n not in roster)
        if extras:
            _fail(f"live team has members not in the snapshot ({', '.join(extras)}); not guessing")
        cli_mismatch = sorted(
            n for n, p in live.items() if p.cli and roster[n].get("cli") and p.cli != roster[n]["cli"]
        )
        if cli_mismatch:
            _fail(f"live member cli differs from snapshot for: {', '.join(cli_mismatch)}; not guessing")
        missing = [roster[n] for n in roster if n not in live]
        if not missing:
            _fail(f"team '{team_name}' is live and complete — nothing to resume")
        result = _resume_members_into_live_team(snap, win, live, missing, fresh_members)
    else:
        result = _resume_full_team(handle, snap, fresh_members)
    click.echo(json.dumps(result, indent=2, ensure_ascii=False))


@cli.group("squad")
def squad_cmd():
    """Squad (orch + challenger + on-demand duos) management.

    \b
    Examples:
      hive squad init                        # orch (= this pane) + challenger
      hive squad spawn-duo --feature-id login-flow --task /tmp/task.md
      hive squad layout                      # re-apply the squad window layout
    """


def _wait_for_peer_ready(
    workspace: str,
    *,
    team_name: str,
    agents: set[str],
    timeout_seconds: float = 30.0,
    poll_interval: float = 0.5,
) -> set[str]:
    """Poll sidecar team-runtime until every agent's first skill turn completes.

    An agent is considered ready when ``inputState == 'ready'`` — i.e. the
    sidecar's input gate sees the transcript in a "clear" state, which
    happens after the dispatched skill has finished its bootstrap turn (the
    `hive team` self-identification call returns + assistant replies + CLI
    waits for next input). Returns the set of agents still not ready when
    the deadline expires (empty set = all ready).
    """
    from .sidecar import request_team_runtime

    deadline = time.monotonic() + timeout_seconds
    waiting = set(agents)
    while waiting and time.monotonic() < deadline:
        runtime_payload = request_team_runtime(workspace, team=team_name) or {}
        members = runtime_payload.get("members") if isinstance(runtime_payload, dict) else None
        if isinstance(members, dict):
            still: set[str] = set()
            for name in waiting:
                member = members.get(name) or {}
                if isinstance(member, dict) and member.get("inputState") == "ready":
                    continue
                still.add(name)
            waiting = still
        if waiting:
            time.sleep(poll_interval)
    return waiting


def _apply_squad_layout(window_target: str) -> str:
    """Apply the canonical SQUAD layout via the shared adaptive picker.

    Returns the orientation (``horizontal``/``vertical``/``""``) so the
    squad JSON payloads can keep exposing it.
    """
    from . import layout as layout_mod
    choice = layout_mod.apply_adaptive(window_target)
    return choice.orientation if choice is not None else ""


def _create_squad_main_team(*, window_target: str, lead_pane: str) -> Team:
    """Create a squad's internal main team bound to the *final* squad window.

    Standalone-friendly (mirrors `hive init`): derives the team name + workspace
    from ``window_target``'s stable id, resets the auto workspace, and starts the
    sidecar. The caller decides the final window (rename/break) first so identity
    follows where the squad lives, not the origin pane (Bug A).
    """
    session_name = (
        window_target.split(":")[0] if ":" in window_target else (tmux.get_current_session_name() or "hive")
    )
    final_window_id = tmux.get_window_id(window_target) or ""
    final_index = window_target.rsplit(":", 1)[-1] if ":" in window_target else "0"

    _prepare_window_for_new_team(window_target, current_pane=lead_pane)
    team_name = _default_team_name_for_window(session_name, final_window_id, final_index)
    _claim_team_name(team_name, this_window=window_target, explicit=False)
    ws_path = _default_auto_workspace_path(session_name, final_window_id, final_index)

    from .sidecar import stop_sidecar
    stop_sidecar(str(ws_path))
    bus.reset_workspace(ws_path)

    try:
        t = Team.create_for_window(
            team_name,
            window_target=window_target,
            lead_pane_id=lead_pane,
            lead_name=LEAD_AGENT_NAME,
            description=f"squad main team ({session_name} {window_target})",
            workspace=str(ws_path),
            tag_lead=False,
        )
    except ValueError as e:
        _fail(str(e))
        raise AssertionError("unreachable")

    _remember_context(team=team_name, workspace=str(ws_path), agent=LEAD_AGENT_NAME)
    _ensure_team_sidecar(t, ws_path)
    return t


@squad_cmd.command("init")
@click.option(
    "--peer-cli",
    type=click.Choice(["claude", "codex"]),
    default=None,
    help="CLI for challenger (default: anti-family of current pane's CLI)",
)
@click.option(
    "--name",
    "squad_name",
    default=None,
    help=(
        "Squad instance name (public namespace for this squad). Picks an "
        "unused name from the canonical pool (peaky/krays/crips/jesse/triad/"
        "shelby/yakuza/bloods/dalton/bratva) when omitted."
    ),
)
@click.option(
    "--worker",
    "worker_cli",
    type=click.Choice(["claude", "codex"]),
    default=None,
    help="CLI for this squad's duo workers (default: orch's family; validator takes the anti-family review seat). e.g. --worker codex for backend-heavy squads.",
)
def squad_init_cmd(peer_cli: str | None, squad_name: str | None, worker_cli: str | None):
    """Break current pane into a dedicated squad window (orch + challenger).

    Standalone — no need to run `hive init` first. Must run from a pane that's
    already running an agent CLI (claude / codex); that CLI becomes
    orch's session. If the pane isn't yet bound to a team, one is auto-created
    (mirrors `hive init`).

    Each squad gets a public namespace name (picked from the canonical pool
    unless overridden via --name). The window is renamed to the squad name;
    agents inside are addressed as ``<squad>.orch``, ``<squad>.challenger``,
    and on-demand ``<squad>.worker-<N>`` / ``<squad>.validator-<N>`` peers.
    This lets multiple squads coexist in the same tmux session without
    qualified-name collision.

    Layout auto-picks based on window aspect ratio:
      - horizontal (wide): orch left, challenger right
      - vertical (tall): orch / challenger stacked top-to-bottom

    Focus switches to the new squad window after init.
    """
    if not tmux.is_inside_tmux():
        _fail("must run inside tmux")

    current_pane = tmux.get_current_pane_id() or ""
    if not current_pane:
        _fail("cannot determine current pane")

    profile = detect_profile_for_pane(current_pane)
    if profile is None:
        _fail("current pane must be running claude / codex (this will become orch)")

    # Idempotent: a pane already running as a squad orch returns its existing
    # binding untouched — before any squad-name claim, rename/break, retag, or a
    # second challenger spawn. Re-running `hive squad init` must be safe, and the
    # squad's own `@hive-group` must not be mistaken for a foreign name claim.
    existing = _discover_tmux_binding()
    if existing.get("team"):
        group = existing.get("group", "")
        agent = existing.get("agent", "")
        if group and group != "duo" and agent.endswith(".orch"):
            click.echo(json.dumps(existing, indent=2, ensure_ascii=False))
            return
        _fail(
            f"current pane is already bound to Hive team '{existing['team']}' as "
            f"'{agent or 'a member'}'; run `hive squad init` from an unbound pane."
        )

    if squad_name:
        ok, reason = squad_names.validate_name(squad_name)
        if not ok:
            _fail(reason)
        if squad_name in squad_names.claimed_names():
            _fail(f"squad name '{squad_name}' already in use on this tmux server")
    else:
        window_id_for_fallback = tmux.get_current_window_id() or ""
        squad_name = squad_names.pick_available_name(window_id_for_fallback)

    _gc_dead_teams()

    orch_cli = _resolve_spawn_cli_name(None)

    from . import settings as user_settings
    ch_role_cli, ch_role_model = user_settings.resolve_role_config("challenger")

    if peer_cli:
        peer_cli_name = peer_cli
        peer_model_id = ch_role_model
    elif ch_role_cli:
        peer_cli_name = ch_role_cli
        peer_model_id = ch_role_model
    else:
        peer_cli_name = peer_cli_for_family(family_for_pane(current_pane))
        peer_model_id = ch_role_model

    orch_agent_name = f"{squad_name}.orch"
    challenger_agent_name = f"{squad_name}.challenger"

    # The pane is unbound here (bound squad orch returned early above). Decide the
    # final squad window before creating the main team, so the team identity
    # derives from where the squad actually lives (Bug A).
    window_display_name = f"squad {squad_name}"
    if tmux.get_pane_count(current_pane) <= 1:
        current_window = tmux.display_value(current_pane, "#{session_name}:#{window_index}")
        if not current_window:
            _fail("cannot determine current window")
        tmux.rename_window(current_window, window_display_name)
        squad_window, orch_pane = current_window, current_pane
    else:
        squad_window, orch_pane = tmux.break_pane(current_pane, name=window_display_name)
        if not squad_window:
            _fail("failed to break-pane into new window")
    t = _create_squad_main_team(window_target=squad_window, lead_pane=orch_pane)

    ws = _resolve_workspace(t, required=True)
    orch_cwd = tmux.display_value(orch_pane, "#{pane_current_path}") or ws

    session_for_base = tmux.get_current_session_name() or ""
    range_base = squad_names.pick_range_base(
        squad_name,
        _claimed_squad_bases(session_for_base) if session_for_base else set(),
    )

    tmux.set_window_option(squad_window, "@hive-team", t.name)
    tmux.set_window_option(squad_window, "@hive-workspace", t.workspace or ws)
    tmux.set_window_option(squad_window, "@hive-squad-name", squad_name)
    tmux.set_window_option(squad_window, "@hive-squad-base", str(range_base))
    if worker_cli:
        # Per-squad worker-family override; spawn-duo reads this when picking
        # which CLI a duo's worker runs (validator takes the anti-family).
        tmux.set_window_option(squad_window, "@hive-squad-worker", worker_cli)
    tmux.configure_hive_window(squad_window)
    if t.description:
        tmux.set_window_option(squad_window, "@hive-desc", t.description)
    tmux.set_window_option(squad_window, "@hive-created", str(t.created_at or time.time()))

    tmux.set_pane_option(orch_pane, "hive-role", "agent")
    tmux.set_pane_option(orch_pane, "hive-agent", orch_agent_name)
    tmux.set_pane_option(orch_pane, "hive-team", t.name)
    tmux.set_pane_option(orch_pane, "hive-group", squad_name)
    tmux.set_pane_option(orch_pane, "hive-cli", orch_cli)

    from . import layout as layout_mod

    # Use orch's cwd (user's project dir) for the challenger, not Hive's workspace
    # state dir — challenger needs to see the same codebase orch sees.
    challenger_agent = Agent.spawn(
        name=challenger_agent_name,
        team_name=t.name,
        target_pane=orch_pane,
        cwd=orch_cwd,
        split_horizontal=layout_mod.split_horizontal(squad_window, 2),
        split_size="50%",
        skill="none",
        prompt=_role_bootstrap_prompt("squad-challenger"),
        cli=peer_cli_name,
        model=peer_model_id,
    )

    tmux.set_pane_option(challenger_agent.pane_id, "hive-group", squad_name)

    orientation = _apply_squad_layout(squad_window)

    # Declare the orch ↔ challenger pair now that both panes are tagged. Reload
    # the team so set_peer sees both names in peer_member_names.
    try:
        reloaded = Team.load(t.name, prefer_pane=orch_pane)
        reloaded.set_peer(orch_agent_name, challenger_agent_name)
    except (FileNotFoundError, KeyError, ValueError):
        pass

    # The orch pane is the agent running this very command — its role load
    # is returned as `next`, never injected as a fake user message.
    dispatched: list[str] = [challenger_agent_name]

    tmux.select_window(squad_window)

    click.echo(json.dumps({
        "team": t.name,
        "window": squad_window,
        "squadName": squad_name,
        "group": squad_name,
        "duoIndexRange": [range_base, range_base + 999],
        "orientation": orientation,
        "orch": {"pane": orch_pane, "name": orch_agent_name},
        "challenger": {"pane": challenger_agent.pane_id, "name": challenger_agent_name},
        "dispatched": dispatched,
        "next": "hive skills get squad-orch",
    }, indent=2))


def _claimed_squad_bases(session: str) -> set[int]:
    """Return every ``@hive-squad-base`` index currently claimed in *session*.

    Scans live windows for the ``@hive-squad-base`` option (set at
    ``hive squad init`` time). Used by ``pick_range_base`` to avoid
    colliding ranges across squads coexisting in the same session.
    """
    claimed: set[int] = set()
    for idx in tmux.list_window_indices(session):
        target = f"{session}:{idx}"
        base_val = tmux.get_window_option(target, "hive-squad-base")
        if not base_val:
            continue
        try:
            claimed.add(int(base_val))
        except ValueError:
            continue
    return claimed


def _next_peer_index_in_range(session: str, base: int) -> int:
    """Next unused tmux window index inside *squad*'s range ``[base, base+999]``.

    Each squad owns a 1000-wide slice of peer indices (peaky 1000-1999,
    krays 2000-2999, ...). Peer windows are placed strictly monotonically
    within the range; we never reuse a retired slot to keep the
    index-as-identity invariant stable across the peer's lifetime.

    Fails loudly when the range is exhausted — user must cleanup / retire
    before spawning more.
    """
    range_end = base + 999
    used = [i for i in tmux.list_window_indices(session) if base <= i <= range_end]
    if not used:
        return base
    nxt = max(used) + 1
    if nxt > range_end:
        _fail(
            f"squad peer index range {base}-{range_end} exhausted in session '{session}'; "
            "retire old peers or run `hive squad cleanup` before spawning more"
        )
    return nxt


# Default tmux window name for a freshly-spawned squad peer before the
# atomic dispatch rename kicks in. Full lifecycle per squad:
# ``<squad>-pending`` → ``<squad>-<feature>-running`` → ``<squad>-<feature>-done``
# / ``<squad>-<feature>-fail``. The squad-name prefix groups peer windows
# visually under their owning squad in the tmux status bar.
_SQUAD_PEER_WINDOW_NAME_INITIAL = "pending"


def _resolve_squad_worker_config(orch_pane: str, squad_window: str) -> tuple[str, str]:
    """``(cli, model)`` for a squad's duo worker.

    CLI precedence: ``@hive-squad-worker`` (set by ``squad init --worker``)
    > ``roles.worker.cli`` > legacy ``squad.duoWorker`` > orch's CLI.
    Model: ``roles.worker.model`` (independent of CLI source).
    """
    from . import settings as user_settings

    role_cli, role_model = user_settings.resolve_role_config("worker")

    tagged = tmux.get_window_option(squad_window, "hive-squad-worker") if squad_window else ""
    if tagged in AGENT_CLI_NAMES:
        return (tagged, role_model)
    if role_cli:
        return (role_cli, role_model)
    configured = user_settings.get_setting("squad.duoWorker", "")
    if configured in AGENT_CLI_NAMES:
        return (configured, role_model)
    orch_cli = tmux.get_pane_option(orch_pane, "hive-cli") or _resolve_spawn_cli_name(None)
    cli = orch_cli if orch_cli in AGENT_CLI_NAMES else "claude"
    return (cli, role_model)


def _copy_squad_integration_option(squad_window: str, peer_window: str) -> None:
    """Propagate @hive-squad-integration-branch from the squad window to a duo
    window so `hive worktree start` in the duo resolves the squad base locally.
    No-op when the squad has not declared an integration branch yet."""
    if not squad_window or not peer_window:
        return
    value = tmux.get_window_option(squad_window, "hive-squad-integration-branch")
    if value:
        tmux.set_window_option(peer_window, "@hive-squad-integration-branch", value)


@squad_cmd.command("set-integration-branch")
@click.argument("ref")
@_json_default_options
def squad_set_integration_branch_cmd(ref: str, plain: bool):
    """Declare the squad's integration branch (the base of every sub-PR).

    Run from the squad window after creating the branch; duos spawned
    afterwards inherit it and `hive worktree start` resolves base from it.
    REF must already resolve to a commit.
    """
    window = tmux.get_current_window_target() or ""
    squad_name = (tmux.get_window_option(window, "hive-squad-name") if window else None) or ""
    if not squad_name:
        _fail("not in a squad window (no @hive-squad-name); run from the squad's orch window")
    from . import worktree as wt_mod

    try:
        anchor = wt_mod.repo_anchor(os.getcwd())
        oid = wt_mod.rev_parse(anchor, ref)
    except wt_mod.WorktreeError as e:
        _fail(str(e))
        raise AssertionError("unreachable")
    tmux.set_window_option(window, "@hive-squad-integration-branch", ref)
    if plain:
        click.echo(f"squad '{squad_name}' integration branch set: {ref} ({oid[:12]})")
    else:
        click.echo(json.dumps({"squad": squad_name, "integrationBranch": ref, "oid": oid, "window": window}, indent=2))


@squad_cmd.command("spawn-duo")
@click.option(
    "--feature-id",
    "feature_id",
    required=True,
    help=(
        "Feature id — semantic kebab-case, ≤4 words (e.g. contract-usd-amount-words); "
        "becomes the branch / worktree / window / sub-PR name"
    ),
)
@click.option(
    "--task",
    "task_artifact",
    required=True,
    type=click.Path(exists=True, dir_okay=False),
    help="Task artifact path for worker dispatch (required so worker never boots into an empty inbox)",
)
@click.option(
    "--val",
    "val_artifact",
    default="",
    type=click.Path(dir_okay=False),
    help="VAL artifact path for validator bootstrap (defaults to <workspace>/val-feature-<feature-id>.md if it exists)",
)
@click.option(
    "--base",
    "integration_branch",
    default="",
    help="Integration branch for sub-PRs (e.g. peaky-integration). Sets @hive-squad-integration-branch on both orch and duo windows; future spawns inherit it automatically.",
)
def squad_spawn_duo_cmd(feature_id: str, task_artifact: str, val_artifact: str, integration_branch: str):
    """Spawn a fresh duo (worker + validator) and dispatch the task atomically.

    Must run from an orch pane inside a squad window — inherits the squad
    instance name from the caller's ``@hive-group`` tag so worker/validator
    names carry the same prefix (e.g. ``peaky.worker-1000`` when orch is
    ``peaky.orch``).

    Atomic dispatch: once both halves are ready, the command renames the
    window to ``<squad>-<feature>-running`` and sends the task artifact to
    worker + a bootstrap message to validator. This closes the window
    between spawn and first task, stopping the duo from boot-exploring
    sqlite / artifacts on its own while waiting.

    Per-squad index range: each squad owns a 1000-wide slice of tmux duo
    window indices — peaky 1000-1999, krays 2000-2999, crips 3000-3999
    (canonical pool positions), non-pool fallbacks get the next unused
    1000-block. Duos within a squad are monotonic inside that slice, so
    `$session:1000` maps to team `<main>-duo-1000` / `<squad>.worker-1000`
    / `<squad>.validator-1000`, visually grouping by squad in the status bar.

    Worker runs the squad's configured family (default: orch's; override via
    ``squad init --worker`` or the ``squad.duoWorker`` config), validator the
    anti-family. Both tagged ``@hive-group=<squad>`` and
    ``@hive-owner=<squad>.orch`` for owner-bypass routing.
    """
    ok, reason = squad_names.validate_feature_id(feature_id)
    if not ok:
        _fail(reason)

    if integration_branch:
        from . import worktree as wt_mod
        try:
            anchor = wt_mod.repo_anchor(os.getcwd())
            wt_mod.rev_parse(anchor, integration_branch)
        except wt_mod.WorktreeError as e:
            _fail(f"--base {integration_branch}: {e}")

    if not tmux.is_inside_tmux():
        _fail("must run inside tmux")

    current_pane = tmux.get_current_pane_id() or ""
    if not current_pane:
        _fail("cannot determine current pane")

    caller_group = tmux.get_pane_option(current_pane, "hive-group") or ""
    if not caller_group or caller_group == "squad":
        _fail("current pane is not part of a SQUAD; run from the orch pane after `hive squad init`")

    squad_name = caller_group
    ok, reason = squad_names.validate_name(squad_name)
    if not ok:
        _fail(f"current pane's @hive-group '{squad_name}' is not a valid squad name: {reason}")

    _, main_team = _resolve_scoped_team(None, required=True)
    if main_team is None:
        _fail("no team bound to current window")

    session = main_team.tmux_session or tmux.get_current_session_name() or ""
    if not session:
        _fail("cannot determine tmux session")

    # Read the squad's index-range base from the squad window. For squad
    # windows that pre-date the range scheme (or were tagged manually),
    # auto-compute + stamp now so future spawns are consistent.
    squad_window_target = main_team.tmux_window or ""
    range_base_val = tmux.get_window_option(squad_window_target, "hive-squad-base") if squad_window_target else None
    try:
        range_base = int(range_base_val) if range_base_val else 0
    except ValueError:
        range_base = 0
    if not range_base:
        range_base = squad_names.pick_range_base(squad_name, _claimed_squad_bases(session))
        if squad_window_target:
            tmux.set_window_option(squad_window_target, "@hive-squad-base", str(range_base))

    n = _next_peer_index_in_range(session, range_base)
    worker_name = f"{squad_name}.worker-{n}"
    validator_name = f"{squad_name}.validator-{n}"
    owner_name = f"{squad_name}.orch"
    clashes = [
        p for p in tmux.list_panes_all()
        if p.agent in {worker_name, validator_name}
    ]
    if clashes:
        _fail(
            f"auto-picked index={n} but panes already use {sorted({p.agent for p in clashes})}; "
            "stale pane naming — kill them manually"
        )

    workspace = main_team.workspace or ""
    # Ensure shared artifact dirs exist so orch/worker/validator can drop files
    # without stat'ing first. Idempotent; safe to call on every spawn-duo.
    if workspace:
        artifacts_root = Path(workspace) / "artifacts"
        for sub in ("tasks", "handoffs", "verdicts"):
            (artifacts_root / sub).mkdir(parents=True, exist_ok=True)
    peer_team_name = f"{main_team.name}-duo-{n}"
    # Window name carries the squad prefix so peer windows group visually
    # under their owning squad in tmux status bars. The `-pending` suffix
    # is momentary — the atomic dispatch block below renames to
    # `<squad>-<feature>-running` once both peers are ready.
    window_name = f"{squad_name}-{_SQUAD_PEER_WINDOW_NAME_INITIAL}"
    # Prefer orch pane's cwd (user's project dir) over Hive workspace state dir.
    cwd = tmux.display_value(current_pane, "#{pane_current_path}") or workspace or os.getcwd()

    peer_window, shell_pane = tmux.new_window(session, name=window_name, cwd=cwd, index=n)
    if not shell_pane:
        _fail(f"failed to create window {session}:{n}")

    tmux.set_window_option(peer_window, "@hive-team", peer_team_name)
    tmux.set_window_option(peer_window, "@hive-workspace", workspace)
    tmux.set_window_option(peer_window, "@hive-squad-name", squad_name)
    tmux.set_window_option(peer_window, "@hive-created", str(time.time()))
    if integration_branch:
        tmux.set_window_option(squad_window_target, "@hive-squad-integration-branch", integration_branch)
        tmux.set_window_option(peer_window, "@hive-squad-integration-branch", integration_branch)
    else:
        _copy_squad_integration_option(squad_window_target, peer_window)
    _duo_integration = tmux.get_window_option(peer_window, "hive-squad-integration-branch") or ""
    tmux.configure_hive_window(peer_window)

    peer_team = Team(
        name=peer_team_name,
        workspace=workspace,
        tmux_session=session,
        tmux_window=peer_window,
        tmux_window_id=tmux.get_window_id(peer_window) or "",
    )

    worker_cli, worker_model = _resolve_squad_worker_config(current_pane, squad_window_target)

    from . import settings as user_settings
    val_role_cli, val_role_model = user_settings.resolve_role_config("validator")
    validator_cli = val_role_cli if val_role_cli else anti_peer_cli(worker_cli)
    validator_model = val_role_model

    worker_agent = Agent.spawn(
        name=worker_name,
        team_name=peer_team_name,
        target_pane=shell_pane,
        cwd=cwd,
        split_window=False,
        skill="none",
        prompt=_role_bootstrap_prompt("squad-worker"),
        cli=worker_cli,
        model=worker_model,
    )
    tmux.set_pane_option(worker_agent.pane_id, "hive-group", squad_name)
    tmux.set_pane_option(worker_agent.pane_id, "hive-owner", owner_name)
    peer_team.agents[worker_name] = worker_agent

    from . import layout as layout_mod
    validator_pane_count_after = len(tmux.list_panes(peer_window)) + 1
    validator_agent = Agent.spawn(
        name=validator_name,
        team_name=peer_team_name,
        target_pane=worker_agent.pane_id,
        cwd=cwd,
        split_horizontal=layout_mod.split_horizontal(peer_window, validator_pane_count_after),
        split_size="50%",
        skill="none",
        prompt=_role_bootstrap_prompt("squad-validator"),
        cli=validator_cli,
        model=validator_model,
    )
    tmux.set_pane_option(validator_agent.pane_id, "hive-group", squad_name)
    tmux.set_pane_option(validator_agent.pane_id, "hive-owner", owner_name)
    peer_team.agents[validator_name] = validator_agent

    orientation = _apply_squad_layout(peer_window)

    # Declare the worker ↔ validator pair so `hive team` reflects it explicitly.
    try:
        peer_team.set_peer(worker_name, validator_name)
    except (KeyError, ValueError):
        pass

    # Block until both peer agents settle into a quiescent phase before
    # returning success. A fresh CLI pane emits the prompt (inputState=ready)
    # before the skill file has finished loading, so an immediate send after
    # spawn-duo would race the skill. Poll sidecar team-runtime until both
    # worker and validator report inputState=ready.
    _ensure_team_sidecar(peer_team, workspace)
    not_ready = _wait_for_peer_ready(
        workspace,
        team_name=peer_team_name,
        agents={worker_name, validator_name},
    )
    if not_ready:
        click.echo(json.dumps({
            "status": "spawn_ready_timeout",
            "window": peer_window,
            "notReady": sorted(not_ready),
            "hint": "panes spawned but skill did not reach ready within 30s; inspect manually",
        }, indent=2))
        sys.exit(1)

    # Atomic dispatch: rename the window to the running lifecycle state and
    # immediately hand task + val bootstrap to worker and validator. Without
    # this, the peer boots into an empty inbox and LLM-style agents tend to
    # wander off exploring sqlite / artifacts on their own (that's the
    # "spawn-without-task" anti-pattern).
    running_window_name = f"{squad_name}-{feature_id}-running"
    tmux.rename_window(peer_window, running_window_name)

    task_path = str(Path(task_artifact).resolve())
    if val_artifact:
        val_path = str(Path(val_artifact).resolve())
    else:
        val_default = Path(workspace) / f"val-feature-{feature_id}.md" if workspace else None
        val_path = str(val_default.resolve()) if val_default and val_default.is_file() else ""

    dispatch_errors: list[dict[str, str]] = []
    try:
        _request_send_payload(
            workspace=workspace,
            team=peer_team,
            sender_agent=owner_name,
            target_agent=worker_name,
            body=f"execute feature={feature_id}",
            artifact=task_path,
            command_name="squad-spawn-dispatch",
            warn_on_long_body=False,
        )
    except RuntimeError as exc:
        dispatch_errors.append({"target": worker_name, "error": str(exc)})

    try:
        _request_send_payload(
            workspace=workspace,
            team=peer_team,
            sender_agent=owner_name,
            target_agent=validator_name,
            body=f"standby for feature={feature_id} handoff",
            artifact=val_path,
            command_name="squad-spawn-dispatch",
            warn_on_long_body=False,
        )
    except RuntimeError as exc:
        dispatch_errors.append({"target": validator_name, "error": str(exc)})

    result = {
        "group": "squad",
        "duoTeam": peer_team_name,
        "window": peer_window,
        "windowName": running_window_name,
        "workspace": workspace,
        "orientation": orientation,
        "featureId": feature_id,
        "integrationBranch": _duo_integration or None,
        "dispatch": {
            "worker": {"target": worker_name, "artifact": task_path},
            "validator": {"target": validator_name, "artifact": val_path},
        },
        "panes": {
            worker_name: worker_agent.pane_id,
            validator_name: validator_agent.pane_id,
        },
    }
    if not _duo_integration:
        result["integrationWarning"] = (
            "no @hive-squad-integration-branch set; worker will fail on "
            "`hive worktree start`. Fix: `hive squad set-integration-branch <ref>` "
            "or re-spawn with --base <ref>"
        )
    if dispatch_errors:
        result["dispatchErrors"] = dispatch_errors
        result["hint"] = (
            "peer spawned and ready, but dispatch send failed. "
            "Retry manually via `hive send <agent> ... --artifact <path>`."
        )
        click.echo(json.dumps(result, indent=2))
        sys.exit(1)
    click.echo(json.dumps(result, indent=2))


@squad_cmd.command("layout")
def squad_layout_cmd():
    """Re-apply the canonical SQUAD layout to the current squad window.

    Auto-picks by aspect ratio:
      - horizontal window → orch main left (50%), challenger right
      - vertical window   → panes stacked equally

    Useful after manually dragging panes or switching between monitors.
    """
    if not tmux.is_inside_tmux():
        _fail("must run inside tmux")
    current_pane = tmux.get_current_pane_id() or ""
    window_target = tmux.get_pane_window_target(current_pane) if current_pane else ""
    if not window_target:
        _fail("cannot determine current window target")
    orientation = _apply_squad_layout(window_target)
    click.echo(json.dumps({"orientation": orientation, "window": window_target}, indent=2))


def _is_duo_team_name(name: str) -> bool:
    """True if *name* matches the `<main>-duo-<N>` pattern used by spawn-duo."""
    idx = name.rfind("-duo-")
    if idx < 0:
        return False
    suffix = name[idx + len("-duo-"):]
    return bool(suffix) and suffix.isdigit()


@squad_cmd.command("cleanup")
def squad_cleanup_cmd():
    """Kill all duo-N windows of the current squad.

    Run this only after every feature is DONE and the human has signed off —
    timing is enforced by the squad-orch skill, not the CLI. No flags, no
    `[OPEN]` safety checks. The main squad window (orch / challenger) is never
    touched.
    """
    if not tmux.is_inside_tmux():
        _fail("must run inside tmux")

    current_pane = tmux.get_current_pane_id() or ""
    if not current_pane:
        _fail("cannot determine current pane")

    caller_group = tmux.get_pane_option(current_pane, "hive-group") or ""
    if not caller_group or caller_group == "squad":
        _fail("current pane is not part of a SQUAD; run from the orch pane after `hive squad init`")
    ok, reason = squad_names.validate_name(caller_group)
    if not ok:
        _fail(f"current pane's @hive-group '{caller_group}' is not a valid squad name: {reason}")

    _, main_team = _resolve_scoped_team(None, required=True)
    assert main_team is not None

    if _is_duo_team_name(main_team.name):
        _fail(
            f"current pane is bound to duo team {main_team.name!r}; "
            "run cleanup from the main squad window (orch / challenger)"
        )

    from .team import list_teams

    prefix = f"{main_team.name}-duo-"
    peer_entries = [t for t in list_teams() if t.get("name", "").startswith(prefix)]

    killed_windows: list[str] = []
    killed_teams: list[str] = []
    for entry in peer_entries:
        peer_name = entry.get("name", "")
        window_target = entry.get("tmuxWindow", "")
        if window_target:
            tmux.kill_window(window_target)
            for key in ("hive-team", "hive-workspace", "hive-desc", "hive-created", "hive-peers"):
                tmux.clear_window_option(window_target, f"@{key}")
            killed_windows.append(window_target)
        killed_teams.append(peer_name)

    click.echo(json.dumps({
        "killedWindows": killed_windows,
        "killedTeams": killed_teams,
    }, indent=2))


@cli.command("status-set", hidden=True, context_settings={"ignore_unknown_options": True, "allow_extra_args": True, "help_option_names": ["--help"]})
@click.argument("legacy_args", nargs=-1, type=click.UNPROCESSED)
def status_set(legacy_args: tuple[str, ...]):
    """Removed legacy status publishing command."""
    del legacy_args
    _status_migration_failure("status-set")


@cli.command("status", hidden=True, context_settings={"ignore_unknown_options": True, "allow_extra_args": True, "help_option_names": ["--help"]})
@click.argument("legacy_args", nargs=-1, type=click.UNPROCESSED)
def status_cmd(legacy_args: tuple[str, ...]):
    """Removed projected-status command."""
    del legacy_args
    _status_migration_failure("status")


@cli.command("statuses", hidden=True, context_settings={"ignore_unknown_options": True, "allow_extra_args": True, "help_option_names": ["--help"]})
@click.argument("legacy_args", nargs=-1, type=click.UNPROCESSED)
def statuses_cmd(legacy_args: tuple[str, ...]):
    """Backward-compatible alias for removed `hive status`."""
    del legacy_args
    _status_migration_failure("statuses")


@cli.command("status-show", hidden=True, context_settings={"ignore_unknown_options": True, "allow_extra_args": True, "help_option_names": ["--help"]})
@click.argument("legacy_args", nargs=-1, type=click.UNPROCESSED)
def status_show(legacy_args: tuple[str, ...]):
    """Backward-compatible alias for removed `hive status`."""
    del legacy_args
    _status_migration_failure("status-show")


@cli.command()
@click.argument("to_agent", required=False, default="")
@click.argument("body", required=False, default="")
@click.option("--artifact", default="", help="Artifact path for large payloads")
@click.option("--to", "to_option", hidden=True, default=None)
@click.option("--msg", "msg_option", hidden=True, default=None)
def send(
    to_agent: str,
    body: str,
    artifact: str,
    to_option: str | None,
    msg_option: str | None,
):
    """Start a new thread to another agent (root send only).

    `hive send` always opens a root thread; it does not accept
    `--reply-to`. To reply on an existing thread, use `hive reply`.

    Root sends must keep `body` to a short summary and put details in
    `--artifact`; the body is rejected if longer than 500 chars, has
    3+ lines, contains fenced code, or starts markdown heading/list
    lines.

    \b
    Delivery is binary: the native transport (claude channel / codex daemon)
    either accepted the message — its runtime owns it from there — or the
    command fails with the transport error. Nothing to poll afterwards.

    \b
    Examples:
      hive send dodo "review this diff" --artifact /tmp/diff.md
      hive send dodo "see report" --artifact - <<'EOF'
      # Findings
      - item
      EOF
    """
    _reject_legacy_recipient_options(to_option, msg_option, command="send", to_agent=to_agent)
    team_name, t = _resolve_send_target_team(to_agent)
    sender = _resolve_sender(None)
    ws = _resolve_workspace(t, required=True)
    _validate_root_send_protocol(body, artifact)
    resolved_artifact = _resolve_artifact_path(artifact, workspace=ws)
    try:
        payload = _request_send_payload(
            workspace=ws,
            team=t,
            sender_agent=sender,
            target_agent=to_agent,
            body=body,
            artifact=resolved_artifact,
            reply_to="",
            command_name="send",
        )
    except RuntimeError as exc:
        _fail(str(exc))
        return
    click.echo(json.dumps(payload, indent=2, ensure_ascii=False))


@cli.command()
@click.argument("to_agent", required=False, default="")
@click.argument("body", required=False, default="")
@click.option("--artifact", default="", help="Artifact path for large payloads")
@click.option(
    "--reply-to",
    "reply_to_override",
    default="",
    help="Override the auto-resolved msgId. Required when the latest inbound has already been replied to.",
)
@click.option("--to", "to_option", hidden=True, default=None)
@click.option("--msg", "msg_option", hidden=True, default=None)
def reply(
    to_agent: str,
    body: str,
    artifact: str,
    reply_to_override: str,
    to_option: str | None,
    msg_option: str | None,
):
    """Reply to the latest unanswered inbound message from another agent.

    Without `--reply-to`, hive picks the most recent send event from
    `to_agent` to you that you have not already replied to. If there
    is no such message, the command fails and asks you to pass
    `--reply-to` explicitly; `hive reply` never guesses across
    competing threads.

    \b
    Examples:
      hive reply dodo "fixed"                      # auto-resolve latest inbound
      hive reply dodo "got it" --reply-to aBc1     # explicit thread anchor
      hive reply dodo "see v2" --artifact /tmp/v2.md
    """
    _reject_legacy_recipient_options(to_option, msg_option, command="reply", to_agent=to_agent)
    team_name, t = _resolve_send_target_team(to_agent)
    sender = _resolve_sender(None)
    ws = _resolve_workspace(t, required=True)

    resolved_reply_to = reply_to_override
    if not resolved_reply_to:
        latest = bus.latest_inbound_send_event(ws, sender=sender, target=to_agent)
        if latest is None:
            _fail(
                f"no recent message from '{to_agent}' to '{sender}'; "
                "pass --reply-to explicitly"
            )
        assert latest is not None
        candidate = str(latest.get("msgId") or "")
        if bus.has_send_reply_to(ws, msg_id=candidate, sender=sender, target=to_agent):
            _fail(
                f"already replied to {candidate} from '{to_agent}'; "
                "pass --reply-to explicitly to target another thread"
            )
        resolved_reply_to = candidate

    resolved_artifact = _resolve_artifact_path(artifact, workspace=ws)
    try:
        payload = _request_send_payload(
            workspace=ws,
            team=t,
            sender_agent=sender,
            target_agent=to_agent,
            body=body,
            artifact=resolved_artifact,
            reply_to=resolved_reply_to,
            command_name="reply",
        )
    except RuntimeError as exc:
        _fail(str(exc))
        return
    if not reply_to_override:
        payload["autoReplyTo"] = resolved_reply_to
    click.echo(json.dumps(payload, indent=2, ensure_ascii=False))


@cli.command()
@click.argument("message_id")
def thread(message_id: str):
    """Show a reply thread rooted at a msgId.

    Returns the chain of send/reply events linked to this msgId. Useful
    to audit conversation flow or resolve "who replied to what".

    \b
    Example:
      hive thread aBc1
    """
    _, t = _resolve_scoped_team(None, required=True)
    assert t is not None
    ws = _resolve_workspace(t, required=True)
    from .sidecar import request_thread

    _ensure_team_sidecar(t, ws)
    payload = request_thread(str(ws), message_id)
    if not payload:
        _fail("sidecar unavailable")
    if payload.get("ok") is False:
        _fail(str(payload.get("error", "thread lookup failed")))
    payload.pop("ok", None)
    click.echo(json.dumps(payload, indent=2, ensure_ascii=False))


@cli.command()
@click.argument("agent_name", required=False, default="")
def doctor(agent_name: str):
    """Diagnose agent connectivity and session state.

    With no argument, probes yourself. With an agent name, probes that
    peer — pane liveness, transcript readability, sidecar heartbeat,
    runtime input state.

    \b
    Examples:
      hive doctor                  # probe self
      hive doctor dodo             # probe a peer
    """
    _, t = _resolve_scoped_team(None, required=True)
    assert t is not None
    ws = _resolve_workspace(t, required=True)
    self_name = _resolve_sender(None)

    target_name = agent_name or self_name
    from .sidecar import request_doctor

    _ensure_team_sidecar(t, ws)
    payload = request_doctor(str(ws), team=t.name, target_agent=target_name, verbose=True)
    if not payload:
        _fail("sidecar unavailable")
    if payload.get("ok") is False:
        _fail(str(payload.get("error", "doctor failed")))
    payload.pop("ok", None)
    from .team import duplicate_team_bindings

    dupes = duplicate_team_bindings()
    if dupes:
        payload["duplicateTeams"] = dupes
    click.echo(json.dumps(payload, indent=2, ensure_ascii=False))


@cli.command()
@click.argument("member_name")
@click.option("--lines", "-n", default=30)
def capture(member_name: str, lines: int):
    """Debug: capture raw pane output from a team member's pane.

    Prints the last N lines (default 30) of the member's tmux pane.
    Use to inspect what the agent actually sees when transcript parsing
    gives unexpected results.

    \b
    Example:
      hive capture dodo -n 80
    """
    _, t = _resolve_scoped_team(None, required=True)
    assert t is not None
    try:
        agent = t.get(member_name)
        click.echo(agent.capture(lines))
    except KeyError:
        _fail(f"member '{member_name}' not found in team '{t.name}'")


@cli.command()
@click.argument("agent_name")
def interrupt(agent_name: str):
    """Interrupt an agent pane.

    Sends the agent's native interrupt keystroke (e.g. Esc for Claude
    Code) to cancel an in-progress turn. Use when a peer is stuck in a
    tool loop or you need to abort a runaway action.

    \b
    Example:
      hive interrupt dodo
    """
    _, t = _resolve_scoped_team(None, required=True)
    assert t is not None
    agent = t.get(agent_name)
    agent.interrupt()
    click.echo(json.dumps({
        "member": agent_name,
        "action": "interrupt",
        "pane": getattr(agent, "pane_id", "") or "",
        "success": True,
    }, indent=2, ensure_ascii=False))


@cli.command()
@click.argument("agent_name")
def kill(agent_name: str):
    """Kill an agent pane and remove it from the team.

    Qualified names (`<group>.<name>`) resolve across teams so you can
    kill a peer-team agent from the main group pane. Bare names resolve
    against the caller's scoped team.

    \b
    Example:
      hive kill worker1
    """
    _, t = _resolve_send_target_team(agent_name)
    try:
        agent = t.get(agent_name)
    except KeyError:
        _fail(f"agent '{agent_name}' not found")
        return
    removed_from_team = agent_name in t.agents
    agent.kill()
    if removed_from_team:
        del t.agents[agent_name]
    layout_window = getattr(t, "tmux_window", "") or tmux.get_current_window_target() or ""
    if layout_window:
        from . import layout as layout_mod
        layout_mod.apply_adaptive(layout_window)
    click.echo(json.dumps({
        "member": agent_name,
        "action": "kill",
        "pane": getattr(agent, "pane_id", "") or "",
        "removedFromTeam": removed_from_team,
        "success": True,
    }, indent=2, ensure_ascii=False))


_CVIM_BINARY = Path(__file__).parent / "core_assets" / "cvim" / "bin" / "cvim-command"


def _exec_cvim(mode: str, args: tuple[str, ...]) -> None:
    os.execvp("bash", ["bash", str(_CVIM_BINARY), mode, *args])


@cli.command("cvim", context_settings={"ignore_unknown_options": True, "allow_extra_args": True, "help_option_names": ["--help"]})
@click.argument("args", nargs=-1, type=click.UNPROCESSED)
def cvim_cmd(args: tuple[str, ...]) -> None:
    """Human-only: edit the last assistant message in vim, send it back.

    Opens a popup vim seeded with the previous assistant message and sends the
    edited result back to the agent pane. Intended to be typed by the human via
    the agent's shell escape (e.g. `!hive cvim`) in Claude Code or Codex. Not
    meant for the model to invoke on its own.
    """
    _exec_cvim("cvim", args)


@cli.command("vim", context_settings={"ignore_unknown_options": True, "allow_extra_args": True, "help_option_names": ["--help"]})
@click.argument("args", nargs=-1, type=click.UNPROCESSED)
def vim_cmd(args: tuple[str, ...]) -> None:
    """Human-only: compose in a blank vim buffer, send it to the agent pane.

    Intended to be typed by the human via the agent's shell escape (e.g. `!hive vim`)
    in Claude Code or Codex. Not meant for the model to invoke on its own.
    """
    _exec_cvim("vim", args)


def _exec_fork_split(split: str, args: tuple[str, ...]) -> None:
    reply_pane = os.environ.get("TMUX_PANE", "")
    subprocess.Popen(
        ["hive", "fork", "-s", split, *args],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    if reply_pane:
        tmux.run_shell_detached(f"sleep 0.2 && tmux send-keys -t {shlex.quote(reply_pane)} Escape")


@cli.command("vfork", context_settings={"ignore_unknown_options": True, "allow_extra_args": True, "help_option_names": ["--help"]})
@click.argument("args", nargs=-1, type=click.UNPROCESSED)
def vfork_cmd(args: tuple[str, ...]) -> None:
    """Human-only: fork the current Hive session into a vertical split.

    Intended to be typed by the human via the agent's shell escape (e.g. `!hive vfork`)
    in Claude Code or Codex. Not meant for the model to invoke on its own.
    """
    _exec_fork_split("v", args)


@cli.command("hfork", context_settings={"ignore_unknown_options": True, "allow_extra_args": True, "help_option_names": ["--help"]})
@click.argument("args", nargs=-1, type=click.UNPROCESSED)
def hfork_cmd(args: tuple[str, ...]) -> None:
    """Human-only: fork the current Hive session into a horizontal split.

    Intended to be typed by the human via the agent's shell escape (e.g. `!hive hfork`)
    in Claude Code or Codex. Not meant for the model to invoke on its own.
    """
    _exec_fork_split("h", args)


@cli.command("notify")
@click.argument("message")
def notify_cmd(message: str):
    """Notify the user for the current pane.

    Flashes the tmux window status line, renames the tab, and rings the
    terminal bell so the user can spot the pending pane at a glance. The
    flash persists until the user focuses the target window (no
    timeout). Use this only when you are blocked and need the human
    back — not for progress updates. Message structure should cover:
    what happened, why you need them now, what to do on return.

    \b
    Examples:
      hive notify "press Space to come back and confirm migration"
    """
    target_pane = _resolve_target_pane()
    payload = notify_ui.notify(message, target_pane)
    click.echo(json.dumps(payload))


@cli.group()
def plugin():
    """Manage first-party Hive plugins."""
    pass


def _render_plugin_mutation_result(action: str, payload: dict[str, object]) -> str:
    name = str(payload.get("name", ""))
    lines = [f"Plugin '{name}' {action}."]
    install_root = str(payload.get("installRoot", "") or "")
    commands = [str(item) for item in payload.get("commands", [])]
    command_names = list(
        dict.fromkeys(
            path.stem if path.suffix == ".md" else path.name
            for path in (Path(item) for item in commands)
        )
    )

    if install_root:
        lines.append(f"  install root: {install_root}")
    if command_names:
        lines.append(f"  commands: {', '.join(command_names)}")
    lines.append(
        "  note: existing Codex panes may not reload plugin settings dynamically; "
        "restart them if old hooks or commands still run."
    )
    return "\n".join(lines)


@plugin.command("list")
@_json_default_options
def plugin_list(plain: bool) -> None:
    """List available plugins and whether they are enabled."""
    rows = plugin_manager.list_plugins()
    if not plain:
        click.echo(json.dumps(rows, ensure_ascii=False))
        return

    enabled_count = sum(1 for row in rows if row.get("enabled"))
    click.echo(f"Plugins ({enabled_count}/{len(rows)} enabled)")
    if not rows:
        return

    name_width = max(len(str(row.get("name", ""))) for row in rows)
    for row in rows:
        status = "enabled" if row.get("enabled") else "disabled"
        click.echo(f"  {str(row.get('name', '')):<{name_width}}  {status:<8}  {row.get('description', '')}")


@plugin.command("ls", hidden=True)
@_json_default_options
def plugin_ls(plain: bool) -> None:
    """Hidden alias of `hive plugin list`."""
    plugin_list.callback(plain=plain)


@plugin.command("enable")
@click.argument("name")
@_json_default_options
def plugin_enable(name: str, plain: bool) -> None:
    """Enable a plugin and materialize its commands."""
    try:
        payload = plugin_manager.enable_plugin(name)
        if not plain:
            click.echo(json.dumps(payload, ensure_ascii=False))
            return
        click.echo(_render_plugin_mutation_result("enabled", payload))
    except ValueError as e:
        _fail(str(e))


@plugin.command("disable")
@click.argument("name")
@_json_default_options
def plugin_disable(name: str, plain: bool) -> None:
    """Disable a plugin and remove its commands."""
    try:
        payload = plugin_manager.disable_plugin(name)
        if not plain:
            click.echo(json.dumps(payload, ensure_ascii=False))
            return
        click.echo(_render_plugin_mutation_result("disabled", payload))
    except ValueError as e:
        _fail(str(e))


# --- codex managed launch ---

# codex subcommands that are not an interactive TUI launch: hive leaves these
# completely untouched (raw codex). Everything else (no subcommand, a bare
# [PROMPT], or `resume`/`fork`) is an interactive launch we bind to a per-pane
# daemon so hive can read its native runtime. Kept in sync with `codex --help`.
_CODEX_PASSTHROUGH_SUBCOMMANDS = (
    "exec", "e", "review", "login", "logout", "mcp", "plugin", "mcp-server",
    "app-server", "remote-control", "app", "completion", "update", "doctor",
    "sandbox", "debug", "apply", "a", "cloud", "exec-server", "features", "help",
)

# Global codex options that consume the following token as their value, so the
# subcommand scan does not mistake that value for the subcommand. `--opt=value`
# and `-Cvalue` are self-contained and handled separately.
_CODEX_VALUE_OPTS = frozenset({
    "-c", "--config", "-m", "--model", "-C", "--cd", "--remote",
    "--remote-auth-token-env", "--enable", "--disable", "-p", "--profile",
    "-a", "--ask-for-approval", "-s", "--sandbox",
})


def _codex_subcommand(args: list[str]) -> str | None:
    """First non-option token in `args` — codex's subcommand, if any.

    codex accepts global options before the subcommand (`codex [OPTIONS]
    <COMMAND>`), so checking only `args[0]` misses e.g. `codex -c k=v exec …`.
    Skip option tokens (and the value of value-taking options) to find the real
    subcommand / prompt. Conservative: an unknown option is treated as a flag,
    which at worst leaves an interactive launch managed (the safe default).
    """
    i = 0
    while i < len(args):
        a = args[i]
        if a == "--":
            return args[i + 1] if i + 1 < len(args) else None
        if a.startswith("-"):
            i += 2 if (a in _CODEX_VALUE_OPTS and "=" not in a) else 1
            continue
        return a
    return None


def _exec_codex_managed(args: list[str]) -> None:
    """Replace this process with codex, bound to a per-pane app-server daemon.

    Born-connected path for a user-launched codex: start (or reuse) the pane's
    daemon, then exec ``codex --remote unix://<sock> --cd <cwd> <args>`` so the
    TUI talks to the daemon from the first thread — hive reads native runtime
    over the same socket, no restart and no transcript reverse-engineering.

    Degrades to raw ``codex`` (embedded, status quo) whenever the managed path
    cannot apply: outside tmux, an explicit ``--remote`` already given, or the
    daemon failing to bind. The caller never ends up worse than plain codex.
    """
    from .adapters import codex_app_server

    def _raw() -> None:
        os.execvp("codex", ["codex", *args])

    pane = os.environ.get("TMUX_PANE") or (tmux.get_current_pane_id() or "")
    if not pane or not tmux.is_inside_tmux():
        _raw()  # hive needs a tmux pane to bind a daemon to
    if _codex_subcommand(args) in _CODEX_PASSTHROUGH_SUBCOMMANDS:
        _raw()  # a management subcommand, not an interactive TUI launch
    if any(a == "--remote" or a.startswith("--remote=") for a in args):
        _raw()  # caller already chose an endpoint
    if not codex_app_server.spawn_daemon(pane):
        _raw()  # daemon would not bind — fall back to embedded codex
    sock = codex_app_server.pane_socket_path(pane)
    # -c check_for_update_on_startup=false mirrors the hive-spawned path so a
    # managed launch never drops the user into codex's npm self-update prompt.
    argv = ["codex", "-c", "check_for_update_on_startup=false", "--remote", f"unix://{sock}"]
    if not _codex_args_set_cwd(args):
        argv += ["--cd", os.getcwd()]
    argv += args
    os.execvp("codex", argv)


def _codex_args_set_cwd(args: list[str]) -> bool:
    """True when the user already passed codex's cwd flag (-C / --cd, any form)."""
    return any(
        a == "--cd" or a.startswith("--cd=") or a.startswith("-C") for a in args
    )


@cli.command(
    "codex",
    context_settings={"ignore_unknown_options": True, "allow_extra_args": True, "help_option_names": ["--help"]},
)
@click.pass_context
def codex_cmd(ctx: click.Context):
    """Launch codex bound to a per-pane app-server daemon (hive-managed).

    Usually invoked through the `hive shell-init` shell function rather than by
    hand; all arguments are forwarded to codex. Replaces the current process
    with codex and never returns on success.
    """
    _exec_codex_managed(list(ctx.args))


# claude subcommands that are not an interactive TUI launch: hive leaves these
# completely untouched (raw claude). Includes subcommands hidden from
# `claude --help` (attach, daemon, logs, remote-control, stop — verified on
# 2.1.198); a missing entry here routes the invocation into `hive claude`,
# where claude's parser turns it into a prompt and spawns a ghost session.
_CLAUDE_PASSTHROUGH_SUBCOMMANDS = (
    "agents", "attach", "auth", "auto-mode", "daemon", "doctor", "gateway",
    "install", "logs", "mcp", "plugin", "plugins", "project", "remote-control",
    "setup-token", "stop", "ultrareview", "update", "upgrade",
)

# Non-interactive surfaces: a -p/--print run has no interactive session for
# hive to message, and --help/--version never start a session.
_CLAUDE_PASSTHROUGH_FLAGS = frozenset({"-p", "--print", "--help", "--version"})

# Global claude options that consume the following token as their value, so
# the subcommand scan does not mistake that value for the subcommand.
# `--opt=value` is self-contained and handled by the flag branch.
_CLAUDE_VALUE_OPTS = frozenset({
    "--model", "--agent", "--session-id", "--settings", "--permission-mode",
    "--effort", "-n", "--name", "--add-dir", "--plugin-dir", "--mcp-config",
    "--setting-sources", "--output-format", "--input-format", "--max-turns",
    "--allowed-tools", "--disallowed-tools", "--append-system-prompt",
    "--append-system-prompt-file", "--system-prompt", "--system-prompt-file",
})


def _claude_subcommand(args: list[str]) -> str | None:
    """First non-option token in `args` — claude's subcommand, if any.

    claude accepts global options before a subcommand, and a user alias like
    ``alias claude='claude --verbose'`` prepends one unconditionally, so
    checking only ``args[0]`` misses e.g. ``claude --verbose daemon status``.
    Worse, a leading flag also defeats claude's own subcommand dispatch: the
    managed exec would turn "daemon status" into a prompt and spawn a ghost
    interactive session. Skip option tokens (and the value of value-taking
    options); ``--`` ends the scan since everything after it is a prompt.
    """
    i = 0
    while i < len(args):
        a = args[i]
        if a == "--":
            return None
        if a.startswith("-"):
            i += 2 if (a in _CLAUDE_VALUE_OPTS and "=" not in a) else 1
            continue
        return a
    return None


def _exec_claude_managed(args: list[str]) -> None:
    """Replace this process with claude, hive channel registered.

    A user-launched interactive claude gets the same per-pane MCP channel as a
    hive-spawned one, so the pane can receive ``<HIVE>`` messages (claude
    delivery is channel-only). The channel flags are appended *after* the
    user's args: both flags are variadic in claude's parser, so this ordering
    keeps a user positional prompt out of their reach without rewriting the
    user's argv.

    Degrades to raw ``claude`` outside tmux, for non-interactive surfaces
    (subcommands, -p/--print, --help/--version), and when the channel plugin
    cannot be converged. Always ends in an exec: no exit code of this
    process is a fallback signal, because after the exec the status belongs
    to claude and any value 0-255 is legitimate — the shell function only
    special-cases a missing ``hive`` binary.
    """
    def _raw() -> None:
        os.execvp("claude", ["claude", *args])

    if not tmux.is_inside_tmux():
        _raw()
    if _claude_subcommand(args) in _CLAUDE_PASSTHROUGH_SUBCOMMANDS:
        _raw()
    if any(a in _CLAUDE_PASSTHROUGH_FLAGS for a in args):
        _raw()
    from .adapters import claude_channel

    # A marker left by a previous claude in this pane must not count as the
    # new launch's readiness — the server it belongs to is gone. Clear before
    # exec (and before the raw fallback on a failed prepare) so readiness can
    # only come from the server this launch actually starts.
    pane = os.environ.get("TMUX_PANE") or (tmux.get_current_pane_id() or "")
    if pane:
        claude_channel.clear_ready(pane)
    flags = claude_channel.prepare_pane(os.getcwd())
    if not flags:
        _raw()  # channel unavailable: a raw claude beats no claude
    os.execvp("claude", ["claude", *args, *flags])


@cli.command(
    "claude",
    context_settings={"ignore_unknown_options": True, "allow_extra_args": True, "help_option_names": ["--help"]},
    add_help_option=False,
)
@click.pass_context
def claude_cmd(ctx: click.Context):
    """Launch claude with the hive channel registered (hive-managed).

    Usually invoked through the `hive shell-init` shell function rather than by
    hand; all arguments are forwarded to claude. Replaces the current process
    with claude and never returns on success.

    Internal seam: the exact argv `channel-server` runs the per-pane channel
    MCP server in-process instead of launching claude. Published plugin
    manifests point at this entry (`hive claude channel-server`) so they stay
    free of machine-specific interpreter paths.
    """
    args = list(ctx.args)
    if args == ["channel-server"]:
        from .adapters.claude_channel_server import main as _channel_server_main

        _channel_server_main()
        return
    _exec_claude_managed(args)


@cli.command("resume-hint", hidden=True)
@click.argument("cli_name", type=click.Choice(["claude", "codex"]))
def resume_hint_cmd(cli_name: str):
    """Print a cd-ready resume command for the session this pane just ran.

    Called by the shell-init claude/codex functions after a managed launch
    exits: claude's own "Resume this session with" line omits the directory
    and codex prints none at all. Resolution rides hive's existing session
    truth only — codex asks the pane's detached app-server daemon (it
    outlives the TUI), claude reads this pane's team-member entry in the
    resume snapshot (the sidecar keeps recording it and the entry survives
    the process). A pane outside a hive team gets no hint; tracking
    arbitrary user panes is not this feature's job. Prints nothing and
    exits 0 on any failure: a hint must never break the wrapper.
    """
    try:
        hint = _resume_hint(cli_name, os.getcwd())
    except Exception:
        return
    if hint:
        click.echo(hint)


def _resume_hint(cli_name: str, cwd: str) -> str | None:
    identity = _pane_team_identity()
    if identity is None:
        return None
    pane, team, agent = identity
    if cli_name == "codex":
        session_id = _pane_codex_session_id(pane)
        resume_cmd = "codex resume"
    else:
        session_id = _member_snapshot_session_id(team, agent)
        resume_cmd = "claude --resume"
    if not session_id:
        return None
    # Both fields are untrusted content headed for automatic terminal output:
    # shlex.quote protects a later shell parse, not the print itself, so
    # control/non-printable bytes (ESC/OSC/BEL/newline) silence the hint. So
    # does a leading "-", which would parse as a CLI option instead of a
    # session id when pasted (`claude --resume` takes an *optional* value,
    # codex `resume [SESSION_ID]` likewise) — quoting cannot demote an
    # option token to data.
    if (
        not cwd.isprintable()
        or not session_id.isprintable()
        or session_id.startswith("-")
    ):
        return None
    command = f"cd {shlex.quote(cwd)} && {resume_cmd} {shlex.quote(session_id)}"
    # cyan matches the CLI's own resume line; click strips the styling
    # whenever stdout is not a real terminal (pipes, tests, logs)
    return f"Resume from anywhere:\n  {click.style(command, fg='cyan')}"


def _pane_team_identity() -> tuple[str, str, str] | None:
    """(pane, team, agent) when this pane is a tagged team member, else None.

    The team gate is shared by both CLIs: any tmux user gets a per-pane codex
    daemon from the managed launch, so daemon reachability alone must not
    qualify a pane for a hint — only hive team member panes are in scope.
    """
    pane = os.environ.get("TMUX_PANE", "").strip()
    if not pane:
        return None
    team = tmux.get_pane_option(pane, "hive-team") or ""
    agent = tmux.get_pane_option(pane, "hive-agent") or ""
    if not team or not agent:
        return None
    return pane, team, agent


def _pane_codex_session_id(pane: str) -> str | None:
    """This pane's codex session id, from the runtime's existing authority.

    The per-pane app-server daemon is detached and outlives the TUI, so right
    after exit it still answers with its thread's session id — the same
    ``codex_app_server.session_id_for_pane`` the sidecar uses. No daemon or
    unresolved → None: no answer means no hint.
    """
    from .adapters import codex_app_server

    return codex_app_server.session_id_for_pane(pane)


def _member_snapshot_session_id(team: str, agent: str) -> str | None:
    """The member's claude session, from the team resume snapshot.

    The sidecar keeps recording every member's sessionId into the resume
    store, and a member entry survives its pane's process exiting — that
    store is hive's own answer to "which session did this pane run". No
    snapshot, no member, or no recorded session → None.
    """
    from . import resume as resume_store

    snap = resume_store.load_snapshot(team)
    if not snap:
        return None
    for member in snap.get("members", []):
        if member.get("name") == agent:
            return str(member.get("sessionId") or "") or None
    return None


_SHELL_INIT_POSIX = """\
# hive agent integration — bind interactive codex/claude launches to hive
# (per-pane daemon for codex, per-pane message channel for claude).
# Bypass anytime with `command codex` / `command claude`.
# Subcommand detection scans for the first non-flag argument: a user alias
# (e.g. `alias claude='claude --verbose'`) injects flags before it, and a
# `--` ends the scan because everything after it is a prompt.
function codex {
  if [ -z "$TMUX" ]; then command codex "$@"; return; fi
  _hive_sub=
  for _hive_a in "$@"; do
    case "$_hive_a" in
      --) break ;;
      -*) continue ;;
      *) _hive_sub="$_hive_a"; break ;;
    esac
  done
  case "$_hive_sub" in
    %(passthrough)s)
      command codex "$@"; return ;;
  esac
  for _hive_a in "$@"; do
    case "$_hive_a" in
      --help|-h|--version|-V)
        command codex "$@"; return ;;
    esac
  done
  if ! command -v hive >/dev/null 2>&1; then command codex "$@"; return; fi
  # The launcher always ends in an exec (managed or raw), so the status here
  # is codex's own — never a fallback signal. The if-condition keeps errexit
  # shells from bailing before the status is saved.
  if hive codex "$@"; then _hive_rc=0; else _hive_rc=$?; fi
  # print a cd-ready resume hint for the session that just ended.
  hive resume-hint codex 2>/dev/null || true
  return $_hive_rc
}

function claude {
  if [ -z "$TMUX" ]; then command claude "$@"; return; fi
  _hive_sub=
  for _hive_a in "$@"; do
    case "$_hive_a" in
      --) break ;;
      -*) continue ;;
      *) _hive_sub="$_hive_a"; break ;;
    esac
  done
  case "$_hive_sub" in
    %(claude_passthrough)s)
      command claude "$@"; return ;;
  esac
  for _hive_a in "$@"; do
    case "$_hive_a" in
      %(claude_flag_passthrough)s)
        command claude "$@"; return ;;
    esac
  done
  if ! command -v hive >/dev/null 2>&1; then command claude "$@"; return; fi
  # The launcher always ends in an exec (managed or raw), so the status here
  # is claude's own — never a fallback signal. The if-condition keeps errexit
  # shells from bailing before the status is saved.
  if hive claude "$@"; then _hive_rc=0; else _hive_rc=$?; fi
  # claude's own resume hint omits the directory; print a cd-ready one.
  hive resume-hint claude 2>/dev/null || true
  return $_hive_rc
}
"""

_SHELL_INIT_FISH = """\
# hive agent integration — bind interactive codex/claude launches to hive
# (per-pane daemon for codex, per-pane message channel for claude).
# Bypass anytime with `command codex` / `command claude`.
# Subcommand detection scans for the first non-flag argument: a user alias
# (e.g. `alias claude='claude --verbose'`) injects flags before it, and a
# `--` ends the scan because everything after it is a prompt.
function codex
    if test -z "$TMUX"
        command codex $argv
        return
    end
    set -l _hive_sub ""
    for a in $argv
        switch "$a"
            case --
                break
            case '-*'
                continue
            case '*'
                set _hive_sub "$a"
                break
        end
    end
    switch "$_hive_sub"
        case %(passthrough)s
            command codex $argv
            return
    end
    for a in $argv
        switch "$a"
            case --help -h --version -V
                command codex $argv
                return
        end
    end
    if not type -q hive
        command codex $argv
        return
    end
    # the launcher always ends in an exec (managed or raw): the status is
    # codex's own, never a fallback signal
    hive codex $argv
    set -l _hive_rc $status
    # print a cd-ready resume hint for the session that just ended.
    hive resume-hint codex 2>/dev/null
    return $_hive_rc
end

function claude
    if test -z "$TMUX"
        command claude $argv
        return
    end
    set -l _hive_sub ""
    for a in $argv
        switch "$a"
            case --
                break
            case '-*'
                continue
            case '*'
                set _hive_sub "$a"
                break
        end
    end
    switch "$_hive_sub"
        case %(claude_passthrough)s
            command claude $argv
            return
    end
    for a in $argv
        switch "$a"
            case %(claude_flag_passthrough)s
                command claude $argv
                return
        end
    end
    if not type -q hive
        command claude $argv
        return
    end
    # the launcher always ends in an exec (managed or raw): the status is
    # claude's own, never a fallback signal
    hive claude $argv
    set -l _hive_rc $status
    # claude's own resume hint omits the directory; print a cd-ready one.
    hive resume-hint claude 2>/dev/null
    return $_hive_rc
end
"""


@cli.command("shell-init")
@click.argument("shell", required=False, default="")
def shell_init_cmd(shell: str):
    """Print the codex + claude shell integration for your shell.

    Add to your shell rc to make interactive `codex` / `claude` launches
    hive-managed:

    \b
      # ~/.zshrc or ~/.bashrc
      eval "$(hive shell-init zsh)"
      # ~/.config/fish/config.fish
      hive shell-init fish | source

    The functions only act inside tmux on interactive launches; management
    subcommands, non-interactive flags, and `command codex` / `command claude`
    pass straight through to the real binaries.
    """
    shell = (shell or os.path.basename(os.environ.get("SHELL", "") or "zsh")).strip()
    claude_flags = sorted(_CLAUDE_PASSTHROUGH_FLAGS)
    if shell == "fish":
        click.echo(_SHELL_INIT_FISH % {
            "passthrough": " ".join(_CODEX_PASSTHROUGH_SUBCOMMANDS),
            "claude_passthrough": " ".join(_CLAUDE_PASSTHROUGH_SUBCOMMANDS),
            "claude_flag_passthrough": " ".join(claude_flags),
        }, nl=False)
    else:
        # zsh and bash share this syntax; case patterns use `|`. The ksh-style
        # `function name {` form bypasses alias expansion of the name in BOTH
        # shells, so a pre-existing `alias claude=...` cannot break the parse
        # (an unadorned `claude() {` does break under zsh, and under bash with
        # expand_aliases). The alias itself keeps working at call time.
        click.echo(_SHELL_INIT_POSIX % {
            "passthrough": "|".join(_CODEX_PASSTHROUGH_SUBCOMMANDS),
            "claude_passthrough": "|".join(_CLAUDE_PASSTHROUGH_SUBCOMMANDS),
            "claude_flag_passthrough": "|".join(claude_flags),
        }, nl=False)


@cli.group()
def peer():
    """Manage default peer mapping inside the team."""
    pass


@peer.command("set")
@click.argument("left")
@click.argument("right")
def peer_set(left: str, right: str):
    """Persist a symmetric default peer pair."""
    _, t = _resolve_scoped_team(None, required=True)
    assert t is not None
    try:
        left_name, right_name = t.set_peer(left, right)
    except (KeyError, ValueError) as exc:
        _fail(str(exc))
    click.echo(f"Peer set: {left_name} <-> {right_name}.")


@peer.command("clear")
@click.argument("agent_name")
def peer_clear(agent_name: str):
    """Clear an explicit peer mapping for one agent."""
    _, t = _resolve_scoped_team(None, required=True)
    assert t is not None
    try:
        peer_name = t.clear_peer(agent_name)
    except KeyError as exc:
        _fail(str(exc))
    if not peer_name:
        click.echo(f"No explicit peer mapping to clear for '{agent_name}'.")
        return
    if t.peer_mode() == "implicit":
        click.echo(
            f"Explicit peer mapping cleared for '{agent_name}' and '{peer_name}'. "
            "Two-agent implicit peer resolution still applies."
        )
        return
    click.echo(f"Peer cleared: {agent_name} <-> {peer_name}.")


# --- worktree pool ----------------------------------------------------------


def _worktree_context() -> dict:
    """Owner / squad context for worktree commands (pane-anchored, cwd-free)."""
    binding = _discover_tmux_binding()
    window = binding.get("tmuxWindow") or (
        (tmux.get_current_window_target() or "") if tmux.is_inside_tmux() else ""
    )
    team = binding.get("team", "")
    squad_name = (tmux.get_window_option(window, "hive-squad-name") if window else None) or ""
    if window and not squad_name and tmux.get_window_option(window, "hive-crew-name"):
        # Pre-rename state must hard-fail, never pass as squad context and
        # never fall through to default-branch base (sub-PRs would aim wrong).
        _fail(
            "this window carries pre-rename '@hive-crew-name' state; cell/crew was "
            "renamed to duo/squad with no fallback — rebuild the team (hive squad init) "
            "before running worktree commands here"
        )
    if squad_name:
        integration: str | None = (
            tmux.get_window_option(window, "hive-squad-integration-branch") or ""
        )
        owner = f"squad:{squad_name}"
    else:
        integration = None
        owner = f"team:{team}" if team else "unbound"
    return {"owner": owner, "team": team, "squad": squad_name, "integration": integration}


@cli.group("worktree")
def worktree_cmd():
    """Per-feature worktree pool: start a feature, finish it, inspect state.

    Pool layout: <main checkout>/.claude/worktrees/<feature>, branch == feature.
    Hive creates/removes worktrees and records ownership in git config;
    entering/leaving the directory is the agent's own move (Claude:
    EnterWorktree path=<path> / ExitWorktree action=keep; Codex: cd).

    \b
    Examples:
      hive worktree start login-flow         # create worktree + branch, print JSON with path
      hive worktree status                   # pool state for this repo
      hive worktree done login-flow          # remove the worktree, keep the branch
    """


@worktree_cmd.command("start")
@click.argument("feature")
@click.option(
    "--base",
    "base_ref",
    default=None,
    help="Base ref override (default: squad integration branch, else detected default branch)",
)
@_json_default_options
def worktree_start_cmd(feature: str, base_ref: str | None, plain: bool):
    """Create (or re-attach) the worktree for FEATURE and print its path as JSON.

    Exit 0 = ready (mode created/existing/attached/adopted-existing-branch).
    Exit 1 with mode=needs-rebase = branch exists but does not contain the
    resolved base: rebase inside the worktree, then rerun start.
    """
    from . import worktree as wt_mod

    try:
        anchor = wt_mod.repo_anchor(os.getcwd())
        wctx = _worktree_context()
        base = wt_mod.resolve_base(anchor, base_ref, wctx["integration"])
        result = wt_mod.start(
            anchor,
            feature,
            base=base,
            owner=wctx["owner"],
            team=wctx["team"],
            squad_name=wctx["squad"],
            gh_merge_base=(wctx["integration"] or None),
        )
    except wt_mod.WorktreeError as e:
        _fail(str(e))
        raise AssertionError("unreachable")
    if plain:
        click.echo(result.path)
        click.echo(f"mode={result.mode} branch={result.branch} base={result.base}@{result.base_oid[:12]}")
        for w in result.warnings:
            click.echo(f"warning: {w}", err=True)
    else:
        click.echo(json.dumps(result.to_json(), indent=2))
    if not result.ready:
        sys.exit(1)


@worktree_cmd.command("done")
@click.argument("feature")
@click.option("--force", is_flag=True, help="Discard uncommitted work (destructive; prints a status summary first)")
@_json_default_options
def worktree_done_cmd(feature: str, force: bool, plain: bool):
    """Remove FEATURE's worktree. The branch is always kept (PRs live on it).

    Refuses while you are inside the worktree, while a git operation is in
    progress, or while there are uncommitted changes (unless --force).
    """
    from . import worktree as wt_mod

    try:
        anchor = wt_mod.repo_anchor(os.getcwd())
        result = wt_mod.done(anchor, feature, force=force, caller_cwd=os.getcwd())
    except wt_mod.WorktreeError as e:
        _fail(str(e))
        raise AssertionError("unreachable")
    if not plain:
        click.echo(json.dumps(result.to_json(), indent=2))
        return
    if result.status_summary:
        click.echo(result.status_summary, err=True)
    click.echo(f"removed {result.removed_path}")
    click.echo(f"branch {result.branch} kept (delete after PR merge via normal flow)")


@worktree_cmd.command("status")
@click.argument("feature", required=False)
@_json_default_options
def worktree_status_cmd(feature: str | None, plain: bool):
    """Read-only lifecycle view of FEATURE (or every hive-labeled worktree)."""
    from . import worktree as wt_mod

    try:
        anchor = wt_mod.repo_anchor(os.getcwd())
        payload: object
        if feature:
            payload = wt_mod.feature_status(anchor, feature)
        else:
            payload = wt_mod.pool_status(anchor)
    except wt_mod.WorktreeError as e:
        _fail(str(e))
        raise AssertionError("unreachable")
    if not plain:
        click.echo(json.dumps(payload, indent=2))
        return
    rows = payload if isinstance(payload, list) else [payload]
    if not rows:
        click.echo("no hive-labeled worktrees or branches")
        return
    for row in rows:
        flags = []
        if row["dirty"]:
            flags.append("dirty")
        if row["inProgress"]:
            flags.append("in-progress:" + ",".join(row["inProgress"]))
        if row["stale"]:
            flags.append("stale")
        suffix = f" [{' '.join(flags)}]" if flags else ""
        owner = f" owner={row['owner']}" if row["owner"] else ""
        click.echo(f"{row['feature']}: {row['state']}{owner} {row['worktreePath']}{suffix}".rstrip())
