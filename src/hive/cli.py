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
from . import crew_names
from . import notify_ui
from . import plugin_manager
from . import skill_sync
from . import tmux
from .agent import AGENT_STARTUP_TIMEOUT, Agent
from .agent_cli import AGENT_CLI_NAMES, anti_peer_cli, detect_profile_for_pane, family_for_pane, member_role_for_pane, normalize_command, peer_cli_for_family, resolve_peer_spawn, resolve_session_id_for_pane
from .team import HIVE_HOME, LEAD_AGENT_NAME, Team


_COMMAND_HELP_SECTIONS = {
    # Daily — per-turn agent collaboration loop.
    "init": "Daily",
    "team": "Daily",
    "send": "Daily",
    "reply": "Daily",
    "answer": "Daily",
    "notify": "Daily",
    "compact": "Daily",
    "skills": "Daily",
    # Handoff — hand a thread to another pane (same/new/forked).
    "handoff": "Handoff",
    "fork": "Handoff",
    "spawn": "Handoff",
    # Workflow — higher-level flows on top of Hive.
    "workflow": "Workflow",
    "crew": "Workflow",
    "cell": "Workflow",
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
    "delivery": "Debug",
    "thread": "Debug",
    "capture": "Debug",
    "inject": "Debug",
    "interrupt": "Debug",
    "kill": "Debug",
    # Extensions.
    "plugin": "Extensions",
    "config": "Extensions",
}
_COMMAND_HELP_SECTION_ORDER = [
    "Daily",
    "Handoff",
    "Workflow",
    "Team",
    "Human Helpers",
    "Debug",
    "Extensions",
    "Other Commands",
]
_COMMAND_HELP_SECTION_DESCRIPTIONS = {
    "Daily": "Core loop per turn: inspect context, talk to peers, pull the human in when blocked.",
    "Handoff": "Hand a thread to another worker — same pane, a fresh spawn, or a forked clone.",
    "Workflow": "Higher-level flows on top of Hive: load workflows and run crew squads.",
    "Team": "Create, extend, and wire up the tmux team around the current window.",
    "Human Helpers": "Popup editor and split helpers for the human (not the model). In Claude Code / Codex, type `!hive cvim` via shell escape. Requires tmux >= 3.2.",
    "Debug": "Troubleshoot delivery, runtime state, and low-level pane behavior. Not on the happy path.",
    "Extensions": "Manage first-party Hive plugins (Factory, Claude Code, Codex).",
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

# Reply & answer (continue an existing thread)
hive reply dodo "fixed"                      # auto-picks latest unanswered inbound
hive answer dodo "yes"                       # answer a pending AskUserQuestion

# Handoff, fork, spawn
hive handoff dodo --artifact /tmp/task.md    # delegate a thread
hive fork                                    # split the current pane into a clone
hive spawn claude                            # bring up a new agent pane

# Debug delivery / connectivity
hive delivery <msgId>                        # trace a send
hive doctor dodo                             # probe a peer's connectivity'''

_TMUX_REQUIRED_MESSAGE = "Hive requires tmux. Start or attach to a tmux session first."
_TMUX_OPTIONAL_ROOT_COMMANDS = {"plugin", "config", "shell-init", "codex", "skills"}
_SEND_GRACE_TIMEOUT = 3.0
_SEND_GRACE_POLL_INTERVAL = 0.2


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
    return _discover_tmux_binding().get("team")


def _default_agent() -> str | None:
    return _discover_tmux_binding().get("agent")


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
    return profile.name if profile else "droid"


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
    """Reject --to/--msg misuse and require a positional target agent."""
    if to_option is None and msg_option is None:
        if to_agent:
            return
        _fail(f"hive {command} requires <agent>. Usage: hive {command} <agent> \"<body>\".")
    _fail(
        f"hive {command} takes positional args: hive {command} <agent> \"<body>\". "
        "Drop --to/--msg."
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


def _default_team_name_for_window(
    session_name: str, window_id: str, window_index: str = "0", explicit_name: str = ""
) -> str:
    """Default team name derived from a window's stable id.

    Team name is a routing key, not a display title, so it must stay unique and
    stable. tmux window ids are never reused within a session, which avoids the
    cross-window collisions that window-index-derived names hit after break-out
    or window reorder (Bug A). An explicit ``--name`` always wins.
    """
    if explicit_name:
        return explicit_name
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
            "busy",
            "model",
            "sessionId",
            "inputState",
            "inputReason",
            "pendingQuestion",
            "turnPhase",
        ):
            value = runtime_fields.get(key)
            if value in ("", None):
                continue
            member[key] = value
        ctx = runtime_fields.get("context")
        if isinstance(ctx, dict):
            member["context"] = ctx
    needs_answer = runtime.get("needsAnswer")
    if isinstance(needs_answer, list) and needs_answer:
        payload["needsAnswer"] = needs_answer
    return payload


def _maybe_attach_compact_hint(
    payload: dict[str, Any],
    *,
    sender: str,
    team_name: str,
    workspace: Any,
) -> None:
    """Best-effort: attach ``compactHint`` when sender context > threshold."""
    if not sender or not team_name or not workspace:
        return
    from .sidecar import request_team_runtime
    from . import context_hint as _ctx_hint

    try:
        runtime = request_team_runtime(str(workspace), team=team_name)
    except Exception:
        return
    _ctx_hint.maybe_attach_hint(payload, self_name=sender, team_runtime=runtime)


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
        f"`hive {command_name}` was removed; use `hive send` to send messages, "
        "`hive answer` to respond to pending questions, "
        "and `hive team` to inspect runtime input state"
    )


def _tmux_runtime_required(argv: list[str]) -> bool:
    positional = [arg for arg in argv if arg and not arg.startswith("-")]
    if not positional:
        return False
    return positional[0] not in _TMUX_OPTIONAL_ROOT_COMMANDS


def _current_pane_agent_cli() -> str:
    if not tmux.is_inside_tmux():
        return ""
    pane_id = tmux.get_current_pane_id() or ""
    if not pane_id:
        return ""
    option_cli = normalize_command(tmux.get_pane_option(pane_id, "hive-cli") or "")
    if option_cli in AGENT_CLI_NAMES:
        return option_cli
    profile = detect_profile_for_pane(pane_id)
    if profile:
        return profile.name
    return ""


def _resolve_spawn_cli_name(cli_name: str | None) -> str:
    if cli_name in AGENT_CLI_NAMES:
        return cli_name
    current_pane = tmux.get_current_pane_id()
    option_cli = normalize_command(tmux.get_pane_option(current_pane, "hive-cli") or "") if current_pane else ""
    if option_cli in AGENT_CLI_NAMES:
        return option_cli
    profile = detect_profile_for_pane(current_pane) if current_pane else None
    return profile.name if profile else "droid"


def _request_send_payload(
    *,
    workspace: str,
    team: Team,
    sender_agent: str,
    target_agent: str,
    body: str,
    artifact: str = "",
    reply_to: str = "",
    wait: bool = False,
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
        wait=wait,
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


# Subcommands that must keep working even when the hive skill is stale —
# they are the recovery/diagnostic paths the user needs to fix the drift.
_SKILL_DRIFT_BYPASS_COMMANDS = {"doctor", "plugin", "shell-init", "codex", "skills"}
_CODEX_NATIVE_REQUIRED_BYPASS_COMMANDS = {
    "codex",
    "config",
    "current",
    "doctor",
    "inject",
    "plugin",
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


def _fail_if_current_pane_hive_skill_is_stale(invoked: str | None) -> None:
    """Abort with an error when the current pane's installed skill is stale.

    Runs unconditionally (no stderr TTY gate) so agent Bash-tool invocations
    also surface a non-zero exit code and the refresh command, not just a
    stderr warning that the caller may silently absorb.
    """
    if invoked in _SKILL_DRIFT_BYPASS_COMMANDS:
        return
    cli_name = _current_pane_agent_cli()
    if not cli_name:
        return
    payload = skill_sync.diagnose_hive_skill(cli_name)
    if payload.get("state") in {"missing", "stale"}:
        _fail(skill_sync.render_hive_skill_warning(payload))


@click.group(cls=SectionedHelpGroup)
@click.pass_context
def cli(ctx: click.Context):
    """Hive - tmux-first multi-agent collaboration runtime."""
    if ctx.resilient_parsing:
        return
    if any(arg in {"-h", "--help"} for arg in sys.argv[1:]):
        return
    _require_codex_native(ctx.invoked_subcommand)
    _fail_if_current_pane_hive_skill_is_stale(ctx.invoked_subcommand)
    if ctx.invoked_subcommand not in _TMUX_OPTIONAL_ROOT_COMMANDS and ctx.invoked_subcommand is not None and not tmux.is_inside_tmux():
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

    \b
    Examples:
      hive fork                                  # auto-detect split direction
      hive fork --split h                        # force horizontal split
      hive fork --join-as dodo-c1 --prompt "continue the thread"
    """
    if pane_id:
        team_name = tmux.get_pane_option(pane_id, "hive-team")
        if team_name:
            target_team = _load_team(team_name)
        else:
            _fail(f"pane '{pane_id}' is not bound to a Hive team")
    else:
        _, target_team = _resolve_scoped_team(None, required=True)

    if not join_as:
        window_target = target_team.tmux_window or tmux.get_current_window_target() or ""
        panes = tmux.list_panes_full(window_target) if window_target else []
        seen_names = _window_seen_names(target_team, panes)
        join_as = _derive_agent_name(seen_names)

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
        "Context is pre-bound. Hive messages will arrive inline as "
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
        agent.load_skill("hive")
        agent.send(_hive_join_message(agent_name, team_name))
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
        snapshot = request_runtime_snapshot(workspace, pane_id=current_pane) or {}
        sid = snapshot.get("sessionId")
        if sid and sid != "unresolved" and snapshot.get("_sessionIdFresh", True):
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


def _fork_boundary_prompt() -> str:
    """The boundary message every fork receives as its first user input.

    Static across workspaces and forks: the new pane resumes the parent's session
    so its transcript starts populated with mid-flight tool calls and intended
    actions. Without a boundary the child happily re-executes those (e.g.
    triggering another `hive fork` and recursing). The fork discovers its own
    name via `hive team` rather than having it baked in.
    """
    return _FORK_BOUNDARY_TEXT


def _fork_boundary_file() -> Path:
    """Cached static boundary file under ``$HIVE_HOME``.

    Lets the resume command stay short (``cat <path>`` rather than the full
    several-line text inline). Rewritten when the cached content drifts from the
    current code so updates take effect without manual cleanup.
    """
    path = HIVE_HOME / "fork-boundary.txt"
    if not path.exists() or path.read_text() != _FORK_BOUNDARY_TEXT:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(_FORK_BOUNDARY_TEXT)
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

    # Boundary text is static across workspaces and forks, so cache it under
    # $HIVE_HOME and expand via shell command substitution when there is no
    # prompt. With --prompt we inline boundary + marker + prompt together so
    # the fork sees both in one user message.
    cmd_base = profile.resume_cmd.format(session_id=session_id)
    if prompt:
        composed = f"{_fork_boundary_prompt()}\n\n{_FORK_NEW_TASK_MARKER}\n{prompt}"
        resume_cmd = f"{cmd_base} {shlex.quote(composed)}"
    else:
        resume_cmd = f"{cmd_base} \"$(cat {shlex.quote(str(_fork_boundary_file()))})\""
    new_pane = tmux.split_window(current_pane, horizontal=horizontal, cwd=source_cwd or None, detach=False)
    tmux.send_keys(new_pane, resume_cmd)
    registered_agent = _register_agent_member(
        t,
        pane_id=new_pane,
        team_name=t.name,
        agent_name=join_as,
        pane_cli=profile.name,
        cwd=source_cwd or os.getcwd(),
        notify=False,
    )
    return registered_agent, new_pane




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
    """Locate a pane by qualified agent name `<group>.<name>`.

    Scans every hive-tagged pane across all sessions. Returns
    ``(team_name, agent_name)`` on unique match or ``None`` if no match.
    Raises ``ValueError`` when multiple panes claim the same qualified
    name (group membership must be unique per qualified name).
    """
    if "." not in qualified:
        return None
    group_name, _, _ = qualified.partition(".")
    if not group_name:
        return None
    matches = [
        p for p in tmux.list_panes_all()
        if p.group == group_name and p.agent == qualified
    ]
    if not matches:
        return None
    if len(matches) > 1:
        raise ValueError(
            f"agent '{qualified}' matches {len(matches)} panes; "
            "group membership must be unique"
        )
    target = matches[0]
    return target.team, target.agent


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
                f"(check @hive-group tag on the target pane)"
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
    and task_closed count as idle, everything else is treated as 'busy'.
    """
    try:
        from .sidecar import _agent_runtime_payload
        runtime = _agent_runtime_payload(pane_id)
    except Exception:
        return False
    if not runtime.get("alive", True):
        return False
    phase = str(runtime.get("turnPhase") or "")
    if phase in {"turn_closed", "task_closed"}:
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


@cli.command("init")
@click.option("--name", "-n", default="", help="Team name (default: tmux session name)")
@click.option("--workspace", "-w", default="", help="Workspace path (default: /tmp/hive-<session>-<window>/)")
@click.option("--notify/--no-notify", default=True, help="Push hive skill + context to other panes")
def init_cmd(name: str, workspace: str, notify: bool):
    """Initialize a cell from the current tmux window: worker (= this pane) + anti-family validator.

    `hive init` is the bare entry into the cell topology (`/cell` and `/crew`
    are the explicit ones). Idempotent: re-running in a bound window reports the
    existing binding.
    """
    if not tmux.is_inside_tmux():
        _fail("hive init requires a tmux session. Run `tmux new-session` or `tmux attach` first, then rerun.")

    current_pane = tmux.get_current_pane_id()
    if detect_profile_for_pane(current_pane or "") is None:
        _fail("current pane must be running claude / codex / droid (this becomes the worker)")
    _require_codex_daemon_backed(current_pane or "")

    result = _create_standalone_cell(
        current_pane=current_pane or "",
        explicit_name=name,
        explicit_workspace=workspace,
        validator_cli=None,
    )
    click.echo(json.dumps(result, indent=2, ensure_ascii=False))


@cli.command("register")
@click.argument("pane_id")
@click.option("--as", "name_override", default="", help="Name for the new member (default: auto-derived)")
@click.option("--notify/--no-notify", default=True, help="Push hive skill + join message to the pane")
@click.option("--group", "group_name", default="", help="Cross-team group tag (e.g. 'crew'). Required for qualified-name routing.")
def register_cmd(pane_id: str, name_override: str, notify: bool, group_name: str):
    """Register an external pane into the current team."""
    if not tmux.is_inside_tmux():
        _fail("hive register requires a tmux session.")

    binding = _discover_tmux_binding()
    team_name = binding.get("team")
    if not team_name:
        _fail("no team bound to the current window. Run `hive cell init` or `hive crew init` first.")

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
@click.option("--cli", "cli_name", type=click.Choice(["droid", "claude", "codex"]), default=None, help="Agent CLI to spawn (default: same as current pane)")
def spawn(agent_name: str, model: str, prompt: str,
          cwd: str, skill: str, workflow: str, env: tuple[str, ...], cli_name: str | None):
    """Spawn an agent pane.

    Creates a new tmux pane in the current window and starts the chosen
    agent CLI. By default spawns the same CLI as the current pane; use
    `--cli droid|claude|codex` to pick a specific one. `--skill` loads
    a base skill on startup (`hive` by default), `--workflow` stacks a
    workflow skill on top, and `--prompt` sends an initial message.

    \b
    Examples:
      hive spawn dodo --cli codex
      hive spawn worker1 --prompt "start on task X" --workflow code-review
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


@cli.group()
def workflow():
    """Workflow helpers on top of Hive."""
    pass


@workflow.command("load")
@click.argument("agent_name")
@click.argument("workflow_name")
@click.option("--prompt", default="", help="Optional prompt to send after loading the workflow")
def workflow_load(agent_name: str, workflow_name: str, prompt: str):
    """Load a workflow into an existing agent pane."""
    team_name, t = _resolve_scoped_team(None, required=True)
    assert team_name is not None and t is not None
    agent = t.get(agent_name)
    agent.load_skill(workflow_name)
    if prompt:
        agent.send(prompt)
    click.echo(f"Workflow '{workflow_name}' loaded into {agent_name}.")


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


@cli.command("wait-status", hidden=True, context_settings={"ignore_unknown_options": True, "allow_extra_args": True})
@click.argument("legacy_args", nargs=-1, type=click.UNPROCESSED)
def wait_status(legacy_args: tuple[str, ...]):
    """Removed legacy status polling command."""
    del legacy_args
    _status_migration_failure("wait-status")


def _is_self_compact(sender: str, target: str, text: str) -> bool:
    return bool(sender) and sender == target and text.strip() == "/compact"


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
    agent.send(text)
    result = {
        "member": agent_name,
        "action": "inject",
        "pane": getattr(agent, "pane_id", "") or "",
        "success": True,
    }
    sender = _resolve_sender(None)
    if not _is_self_compact(sender, agent_name, text):
        ws = _resolve_workspace(t, required=False)
        _maybe_attach_compact_hint(result, sender=sender, team_name=team_name, workspace=ws)
    click.echo(json.dumps(result, indent=2, ensure_ascii=False))


@cli.command("compact")
@click.option("--pane", "pane_id", default="", help="Target pane ID (default: current pane via TMUX_PANE)")
def compact_cmd(pane_id: str):
    """Trigger /compact on your own pane.

    Equivalent to `hive inject <self> /compact`, but the response never
    carries a `compactHint` — `/compact` already addresses the condition
    the hint reports, so re-nagging would be noise.

    When wired into a tmux key binding, pass `--pane "#{pane_id}"` so the
    triggering pane is captured by tmux at keypress time rather than read
    from the (potentially stale) TMUX_PANE env in a detached subprocess.

    \b
    Examples:
      hive compact
      hive compact --pane %21
    """
    if pane_id:
        team_name = tmux.get_pane_option(pane_id, "hive-team")
        if not team_name:
            _fail(f"pane '{pane_id}' is not bound to a Hive team")
        sender = tmux.get_pane_option(pane_id, "hive-agent") or ""
        if not sender:
            _fail(f"pane '{pane_id}' has no hive-agent tag")
        # Compact the pane the keystroke fired in. Bind the Agent to THIS pane
        # rather than re-resolving the name through the team: when two cells share
        # a derived team name, that team holds two agents called "worker"/
        # "validator", and t.get(name) would land /compact on the same-named pane
        # in the other window.
        pane_cli = tmux.get_pane_option(pane_id, "hive-cli") or ""
        agent = Agent(name=sender, team_name=team_name, pane_id=pane_id, cli=pane_cli)
    else:
        team_name, t = _resolve_scoped_team(None, required=True)
        assert team_name is not None and t is not None
        sender = _resolve_sender(None)
        if not sender:
            _fail("cannot determine current agent (run inside a tmux pane bound to this team)")
        agent = t.get(sender)
    agent.send("/compact")
    result = {
        "member": sender,
        "action": "compact",
        "pane": getattr(agent, "pane_id", "") or "",
        "success": True,
    }
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
    if discovered.get("team"):
        _, t = _resolve_scoped_team(str(discovered.get("team")), required=False)
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
    result["hint"] = "No team bound. The cell-vs-crew choice is the user's — ask them with the blocking question tool, then init accordingly. Don't pick or init on your own."
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


def _dispatch_role_spec(pane: str, role: str) -> bool:
    """Inject ``hive skills get <role>`` into *pane* as the agent's first input.

    Role content lives in CLI-served specs (`hive skills get <role>`), not in
    installed skills — so a single bootstrap command loads the role, no
    per-role SKILL.md. Returns True if the pane runs a known agent CLI.
    """
    if detect_profile_for_pane(pane) is None:
        return False
    tmux.send_keys(pane, f"hive skills get {role}", enter=False)
    time.sleep(0.1)
    tmux.send_key(pane, "Enter")
    return True


def _role_bootstrap_prompt(role: str) -> str:
    """Spawn first-message for a no-human role pane: a framing preamble + the
    role's full spec, fed as the startup prompt so the agent operates on it
    directly — no bare command to run-and-stop, no get to chain. The preamble
    carries identity + the idle-wait discipline; the spec carries the protocol.
    """
    spec = _read_spec(role) or f"hive skills get {role}"
    return (
        f"你被 spawn 成这个 team 的 {role}。下面 `---` 之后是你的角色协议 —— 照它做。"
        f"先 `hive team` 确认身份,然后**等你的第一条任务消息**(orch / peer 会发来);"
        f"在那之前别自己找活、别翻库、别退出。\n\n---\n\n{spec}"
    )


@cli.group("cell")
def cell_cmd():
    """CELL atom (worker + anti-family validator) management."""


def _cell_neighbor_for_pairing(
    current_pane: str, window: str, my_family: str
) -> tmux.PaneInfo | None:
    """The current window's sole other pane, if it qualifies as the validator.

    Qualifies = an idle, unowned, anti-family agent. Returns None otherwise.
    Cell only ever conscripts here — the 2-pane case. 3+ panes break out
    rather than guess which neighbor to grab.
    """
    others = [p for p in tmux.list_panes_full(window) if p.pane_id != current_pane]
    if len(others) != 1:
        return None
    neighbor = others[0]
    if neighbor.team or neighbor.group:
        return None
    if detect_profile_for_pane(neighbor.pane_id) is None:
        return None
    other_family = family_for_pane(neighbor.pane_id)
    if my_family != "unknown" and other_family != "unknown" and my_family == other_family:
        return None
    if not _pane_is_idle_for_pairing(neighbor.pane_id):
        return None
    return neighbor


@dataclass
class _CellPlacement:
    """Where a cell's worker lands and who its validator will be, decided
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
    window_name: str = "cell"


_CELL_NAME_NOISE_PREFIXES = ("feat/", "fix/", "feature/", "bugfix/", "chore/", "hotfix/", "worktree-")


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


def _cell_window_name(worker_cwd: str) -> str:
    """A meaningful tmux window label for a cell.

    The worker cwd's git branch is what the cell is actually working on (Hive's
    per-feature worktree workflow makes branch == feature), so use it — minus
    noise prefixes (``feat/``, ``worktree-``, ...). On a default branch
    (main/master) or outside git, fall back to the project (cwd basename); a
    bare "cell" tells you nothing.
    """
    branch = _git_branch_for_cwd(worker_cwd)
    if branch and branch not in ("main", "master"):
        for prefix in _CELL_NAME_NOISE_PREFIXES:
            if branch.startswith(prefix):
                branch = branch[len(prefix):]
                break
        if branch:
            return branch
    base = os.path.basename(worker_cwd.rstrip("/")) if worker_cwd else ""
    return base or "cell"


def _unique_cell_window_name(base: str, this_window: str) -> str:
    """Disambiguate *base* against other live windows.

    Same-repo, same-branch cells compute the same label, so on a collision append
    ``-2``, ``-3``, .... *this_window* is excluded so the cell never collides with
    its own (already-set) name.
    """
    taken = {name for target, name in tmux.list_window_names() if target != this_window}
    if base not in taken:
        return base
    n = 2
    while f"{base}-{n}" in taken:
        n += 1
    return f"{base}-{n}"


def _prepare_cell_placement(
    current_pane: str, *, validator_cli: str | None = None
) -> _CellPlacement:
    """Decide cell placement without creating the team or tagging anything.

    Realized by the current window's pane count — 1: validator splits beside the
    worker; 2: adopt an idle/unowned/anti-family neighbor, else break out; 3+:
    break the worker to a fresh window. Breaking out here, before team creation,
    is what lets the team name/workspace follow the final window.
    """
    worker_cli = _resolve_spawn_cli_name(None)
    my_family = family_for_pane(current_pane)
    if validator_cli:
        v_cli, v_model = validator_cli, ""
    else:
        v_cli, v_model = resolve_peer_spawn(my_cli=worker_cli, my_family=my_family)
        if not v_cli:
            v_cli = anti_peer_cli(worker_cli)

    window = tmux.get_pane_window_target(current_pane) or ""
    if not window:
        _fail("cannot determine current window")
    count = tmux.get_pane_count(current_pane)
    worker_cwd = tmux.display_value(current_pane, "#{pane_current_path}") or ""
    window_name = _cell_window_name(worker_cwd)

    # Decide adopt-vs-spawn before mutating any windows.
    adopt = _cell_neighbor_for_pairing(current_pane, window, my_family) if count == 2 else None

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

    return _CellPlacement(
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


def _attach_cell_to_team(t: Team, *, placement: _CellPlacement, ws: str) -> dict[str, object]:
    """Form a cell on the already-created (final-window) team *t*.

    Tags the worker, spawns or adopts the anti-family validator, declares the
    worker↔validator pair, and dispatches each role spec. The window's
    ``@hive-team`` / ``@hive-workspace`` options were already written by
    ``Team.create_for_window``; this only owns the member panes. Returns a
    descriptor for the caller to echo. Shared by `hive cell init` and `hive init`.
    """
    window = placement.window
    worker_pane = placement.worker_pane
    worker_cli = placement.worker_cli
    worker_cwd = placement.worker_cwd or ws

    # Label the window after what the cell is working on, not a generic "cell";
    # disambiguate same-branch siblings with a -N suffix.
    tmux.rename_window(window, _unique_cell_window_name(placement.window_name, window))
    tmux.configure_hive_window(window)
    tmux.set_pane_option(worker_pane, "hive-role", "agent")
    tmux.set_pane_option(worker_pane, "hive-agent", "worker")
    tmux.set_pane_option(worker_pane, "hive-team", t.name)
    tmux.set_pane_option(worker_pane, "hive-group", "cell")
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
            notify=False,  # role loaded via /cell-validator dispatch below, not the generic hive join
            group="cell",
        )
        validator_pane, validator_cli_used, mode = placement.adopt_pane, placement.adopt_cli, "paired"
    else:
        validator_agent = Agent.spawn(
            name="validator",
            team_name=t.name,
            target_pane=worker_pane,
            cwd=worker_cwd,
            split_horizontal=layout_mod.split_horizontal(window, 2),
            split_size="50%",
            cli=placement.validator_cli,
            model=placement.validator_model,
            skill="none",
            prompt=_role_bootstrap_prompt("cell-validator"),
            workspace=ws,
        )
        t.agents["validator"] = validator_agent
        tmux.set_pane_option(validator_agent.pane_id, "hive-group", "cell")
        hive_context.save_context_for_pane(
            validator_agent.pane_id, team=t.name, workspace=ws, agent="validator"
        )
        validator_pane, validator_cli_used, mode = validator_agent.pane_id, placement.validator_cli, "spawned"

    # Declare the worker ↔ validator pair (reload so both names are visible).
    try:
        reloaded = Team.load(t.name, prefer_pane=worker_pane)
        reloaded.set_peer("worker", "validator")
    except (FileNotFoundError, KeyError, ValueError):
        pass

    layout_mod.apply_adaptive(window)

    # Hand each pane its role: the agent's first input is `hive skills get
    # <role>`. A spawned validator already got it as its startup prompt; an
    # adopted neighbor gets it injected here.
    dispatched: list[str] = []
    if _dispatch_role_spec(worker_pane, "cell-worker"):
        dispatched.append("worker")
    if mode == "paired":
        _dispatch_role_spec(validator_pane, "cell-validator")
    dispatched.append("validator")

    tmux.select_window(window)

    return {
        "team": t.name,
        "window": window,
        "group": "cell",
        "worker": {"pane": worker_pane, "name": "worker", "cli": worker_cli},
        "validator": {
            "pane": validator_pane,
            "name": "validator",
            "cli": validator_cli_used,
            "mode": mode,
        },
        "dispatched": dispatched,
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
                f"'{existing}' — run from a team pane, or start the cell elsewhere."
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


def _create_standalone_cell(
    *,
    current_pane: str,
    explicit_name: str = "",
    explicit_workspace: str = "",
    validator_cli: str | None = None,
) -> dict[str, object]:
    """Shared cell bring-up for `hive init` and `hive cell init`.

    Decides placement (which may break the worker out to a fresh window),
    derives the team name + workspace from the *final* window, creates the team
    there, then forms the cell. Idempotent: if the current pane is already bound
    the existing binding is returned untouched.
    """
    _gc_dead_teams()
    plugin_manager.cleanup_retired_plugins()

    existing = _discover_tmux_binding()
    if existing.get("team"):
        return existing

    session_name = tmux.get_current_session_name() or "hive"
    placement = _prepare_cell_placement(current_pane, validator_cli=validator_cli)
    final_window = placement.window
    final_window_id = tmux.get_window_id(final_window) or ""
    final_index = final_window.rsplit(":", 1)[-1] if ":" in final_window else "0"

    team_name = _default_team_name_for_window(session_name, final_window_id, final_index, explicit_name)
    _prepare_window_for_new_team(final_window, current_pane=placement.worker_pane)
    _claim_team_name(team_name, this_window=final_window, explicit=bool(explicit_name))

    using_auto_workspace = not explicit_workspace
    ws_path = (
        Path(explicit_workspace).expanduser()
        if explicit_workspace
        else _default_auto_workspace_path(session_name, final_window_id, final_index)
    )
    if using_auto_workspace:
        # A fresh cell on a reused window must not inherit the previous team's
        # event log or artifacts from the default auto workspace.
        from .sidecar import stop_sidecar

        stop_sidecar(str(ws_path))
        bus.reset_workspace(ws_path)
    else:
        bus.init_workspace(ws_path)

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
    result = _attach_cell_to_team(t, placement=placement, ws=str(ws_path))
    _ensure_team_sidecar(t, ws_path)
    return result


@cell_cmd.command("init")
@click.option(
    "--validator-cli",
    type=click.Choice(["claude", "codex", "droid"]),
    default=None,
    help="CLI for validator (default: anti-family of current pane's CLI; override if droid wraps an Anthropic model)",
)
def cell_init_cmd(validator_cli: str | None):
    """Set up a cell from the current pane: worker (=this pane) + anti-family validator.

    Standalone — no prior `hive init` needed. The current pane must be running
    an agent CLI (claude / codex / droid); it becomes the worker. Realized by
    the current window's pane count:

      1 pane   → split-spawn the validator beside the worker
      2 panes  → adopt the neighbor as validator if it's an idle, unowned,
                 anti-family agent; otherwise treat as 3+
      3+ panes → break the worker out to a fresh window, then spawn

    The validator runs the anti-family CLI (claude↔codex; droid defaults to
    claude) so review stays independent.
    """
    if not tmux.is_inside_tmux():
        _fail("must run inside tmux")
    current_pane = tmux.get_current_pane_id() or ""
    if not current_pane:
        _fail("cannot determine current pane")
    if detect_profile_for_pane(current_pane) is None:
        _fail("current pane must be running claude / codex / droid (this becomes the worker)")
    _require_codex_daemon_backed(current_pane)

    result = _create_standalone_cell(current_pane=current_pane, validator_cli=validator_cli)
    click.echo(json.dumps(result, indent=2, ensure_ascii=False))


@cli.group("crew")
def crew_cmd():
    """CREW squad (orch + challenger + on-demand cells) management."""


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


def _apply_crew_layout(window_target: str) -> str:
    """Apply the canonical CREW layout via the shared adaptive picker.

    Returns the orientation (``horizontal``/``vertical``/``""``) so the
    crew JSON payloads can keep exposing it.
    """
    from . import layout as layout_mod
    choice = layout_mod.apply_adaptive(window_target)
    return choice.orientation if choice is not None else ""


def _create_crew_main_team(*, window_target: str, lead_pane: str) -> Team:
    """Create a crew's internal main team bound to the *final* crew window.

    Standalone-friendly (mirrors `hive init`): derives the team name + workspace
    from ``window_target``'s stable id, resets the auto workspace, and starts the
    sidecar. The caller decides the final window (rename/break) first so identity
    follows where the crew lives, not the origin pane (Bug A).
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
            description=f"crew main team ({session_name} {window_target})",
            workspace=str(ws_path),
            tag_lead=False,
        )
    except ValueError as e:
        _fail(str(e))
        raise AssertionError("unreachable")

    _remember_context(team=team_name, workspace=str(ws_path), agent=LEAD_AGENT_NAME)
    _ensure_team_sidecar(t, ws_path)
    return t


@crew_cmd.command("init")
@click.option(
    "--peer-cli",
    type=click.Choice(["claude", "codex", "droid"]),
    default=None,
    help="CLI for challenger (default: anti-family of current pane's CLI; override if droid wraps an Anthropic model)",
)
@click.option(
    "--name",
    "crew_name",
    default=None,
    help=(
        "Crew instance name (public namespace for this squad). Picks an "
        "unused name from the canonical pool (peaky/krays/crips/jesse/triad/"
        "shelby/yakuza/bloods/dalton/bratva) when omitted."
    ),
)
@click.option(
    "--worker",
    "worker_cli",
    type=click.Choice(["claude", "codex", "droid"]),
    default=None,
    help="CLI for this crew's cell workers (default: orch's family; validator takes the anti-family review seat). e.g. --worker codex for backend-heavy crews.",
)
def crew_init_cmd(peer_cli: str | None, crew_name: str | None, worker_cli: str | None):
    """Break current pane into a dedicated crew window (orch + challenger).

    Standalone — no need to run `hive init` first. Must run from a pane that's
    already running an agent CLI (claude / codex / droid); that CLI becomes
    orch's session. If the pane isn't yet bound to a team, one is auto-created
    (mirrors `hive init`).

    Each crew gets a public namespace name (picked from the canonical pool
    unless overridden via --name). The window is renamed to the crew name;
    agents inside are addressed as ``<crew>.orch``, ``<crew>.challenger``,
    and on-demand ``<crew>.worker-<N>`` / ``<crew>.validator-<N>`` peers.
    This lets multiple crews coexist in the same tmux session without
    qualified-name collision.

    Layout auto-picks based on window aspect ratio:
      - horizontal (wide): orch left, challenger right
      - vertical (tall): orch / challenger stacked top-to-bottom

    Focus switches to the new crew window after init.
    """
    if not tmux.is_inside_tmux():
        _fail("must run inside tmux")

    current_pane = tmux.get_current_pane_id() or ""
    if not current_pane:
        _fail("cannot determine current pane")

    profile = detect_profile_for_pane(current_pane)
    if profile is None:
        _fail("current pane must be running claude / codex / droid (this will become orch)")

    # Idempotent: a pane already running as a crew orch returns its existing
    # binding untouched — before any crew-name claim, rename/break, retag, or a
    # second challenger spawn. Re-running `hive crew init` must be safe, and the
    # crew's own `@hive-group` must not be mistaken for a foreign name claim.
    existing = _discover_tmux_binding()
    if existing.get("team"):
        group = existing.get("group", "")
        agent = existing.get("agent", "")
        if group and group != "cell" and agent.endswith(".orch"):
            click.echo(json.dumps(existing, indent=2, ensure_ascii=False))
            return
        _fail(
            f"current pane is already bound to Hive team '{existing['team']}' as "
            f"'{agent or 'a member'}'; run `hive crew init` from an unbound pane."
        )

    if crew_name:
        ok, reason = crew_names.validate_name(crew_name)
        if not ok:
            _fail(reason)
        if crew_name in crew_names.claimed_names():
            _fail(f"crew name '{crew_name}' already in use on this tmux server")
    else:
        window_id_for_fallback = tmux.get_current_window_id() or ""
        crew_name = crew_names.pick_available_name(window_id_for_fallback)

    _gc_dead_teams()

    orch_cli = _resolve_spawn_cli_name(None)
    if peer_cli:
        peer_cli_name, peer_model_id = peer_cli, ""
    else:
        peer_cli_name, peer_model_id = resolve_peer_spawn(
            my_cli=orch_cli,
            my_family=family_for_pane(current_pane),
        )
        if not peer_cli_name:
            peer_cli_name = anti_peer_cli(orch_cli)

    orch_agent_name = f"{crew_name}.orch"
    challenger_agent_name = f"{crew_name}.challenger"

    # The pane is unbound here (bound crew orch returned early above). Decide the
    # final crew window before creating the main team, so the team identity
    # derives from where the crew actually lives (Bug A).
    window_display_name = f"crew {crew_name}"
    if tmux.get_pane_count(current_pane) <= 1:
        current_window = tmux.display_value(current_pane, "#{session_name}:#{window_index}")
        if not current_window:
            _fail("cannot determine current window")
        tmux.rename_window(current_window, window_display_name)
        crew_window, orch_pane = current_window, current_pane
    else:
        crew_window, orch_pane = tmux.break_pane(current_pane, name=window_display_name)
        if not crew_window:
            _fail("failed to break-pane into new window")
    t = _create_crew_main_team(window_target=crew_window, lead_pane=orch_pane)

    ws = _resolve_workspace(t, required=True)
    orch_cwd = tmux.display_value(orch_pane, "#{pane_current_path}") or ws

    session_for_base = tmux.get_current_session_name() or ""
    range_base = crew_names.pick_range_base(
        crew_name,
        _claimed_crew_bases(session_for_base) if session_for_base else set(),
    )

    tmux.set_window_option(crew_window, "@hive-team", t.name)
    tmux.set_window_option(crew_window, "@hive-workspace", t.workspace or ws)
    tmux.set_window_option(crew_window, "@hive-crew-name", crew_name)
    tmux.set_window_option(crew_window, "@hive-crew-base", str(range_base))
    if worker_cli:
        # Per-crew worker-family override; spawn-cell reads this when picking
        # which CLI a cell's worker runs (validator takes the anti-family).
        tmux.set_window_option(crew_window, "@hive-crew-worker", worker_cli)
    tmux.configure_hive_window(crew_window)
    if t.description:
        tmux.set_window_option(crew_window, "@hive-desc", t.description)
    tmux.set_window_option(crew_window, "@hive-created", str(t.created_at or time.time()))

    tmux.set_pane_option(orch_pane, "hive-role", "agent")
    tmux.set_pane_option(orch_pane, "hive-agent", orch_agent_name)
    tmux.set_pane_option(orch_pane, "hive-team", t.name)
    tmux.set_pane_option(orch_pane, "hive-group", crew_name)
    tmux.set_pane_option(orch_pane, "hive-cli", orch_cli)

    from . import layout as layout_mod

    # Use orch's cwd (user's project dir) for the challenger, not Hive's workspace
    # state dir — challenger needs to see the same codebase orch sees.
    challenger_agent = Agent.spawn(
        name=challenger_agent_name,
        team_name=t.name,
        target_pane=orch_pane,
        cwd=orch_cwd,
        split_horizontal=layout_mod.split_horizontal(crew_window, 2),
        split_size="50%",
        skill="none",
        prompt=_role_bootstrap_prompt("crew-challenger"),
        cli=peer_cli_name,
        model=peer_model_id,
    )

    tmux.set_pane_option(challenger_agent.pane_id, "hive-group", crew_name)

    orientation = _apply_crew_layout(crew_window)

    # Declare the orch ↔ challenger pair now that both panes are tagged. Reload
    # the team so set_peer sees both names in peer_member_names.
    try:
        reloaded = Team.load(t.name, prefer_pane=orch_pane)
        reloaded.set_peer(orch_agent_name, challenger_agent_name)
    except (FileNotFoundError, KeyError, ValueError):
        pass

    dispatched: list[str] = [challenger_agent_name]
    if _dispatch_role_spec(orch_pane, "crew-orch"):
        dispatched.insert(0, orch_agent_name)

    tmux.select_window(crew_window)

    click.echo(json.dumps({
        "team": t.name,
        "window": crew_window,
        "crewName": crew_name,
        "group": crew_name,
        "cellIndexRange": [range_base, range_base + 999],
        "orientation": orientation,
        "orch": {"pane": orch_pane, "name": orch_agent_name},
        "challenger": {"pane": challenger_agent.pane_id, "name": challenger_agent_name},
        "dispatched": dispatched,
    }, indent=2))


def _claimed_crew_bases(session: str) -> set[int]:
    """Return every ``@hive-crew-base`` index currently claimed in *session*.

    Scans live windows for the ``@hive-crew-base`` option (set at
    ``hive crew init`` time). Used by ``pick_range_base`` to avoid
    colliding ranges across crews coexisting in the same session.
    """
    claimed: set[int] = set()
    for idx in tmux.list_window_indices(session):
        target = f"{session}:{idx}"
        base_val = tmux.get_window_option(target, "hive-crew-base")
        if not base_val:
            continue
        try:
            claimed.add(int(base_val))
        except ValueError:
            continue
    return claimed


def _next_peer_index_in_range(session: str, base: int) -> int:
    """Next unused tmux window index inside *crew*'s range ``[base, base+999]``.

    Each crew owns a 1000-wide slice of peer indices (peaky 1000-1999,
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
            f"crew peer index range {base}-{range_end} exhausted in session '{session}'; "
            "retire old peers or run `hive crew cleanup` before spawning more"
        )
    return nxt


# Default tmux window name for a freshly-spawned crew peer before the
# atomic dispatch rename kicks in. Full lifecycle per crew:
# ``<crew>-pending`` → ``<crew>-<feature>-running`` → ``<crew>-<feature>-done``
# / ``<crew>-<feature>-fail``. The crew-name prefix groups peer windows
# visually under their owning crew in the tmux status bar.
_CREW_PEER_WINDOW_NAME_INITIAL = "pending"


def _resolve_crew_worker_cli(orch_pane: str, crew_window: str) -> str:
    """Which CLI a crew's cell worker runs. Default = orch's family — worker
    stays same-family as orch, the anti-family review seat goes to the
    validator (cross-family value lives in the review gate, not the handoff).

    Override precedence: ``@hive-crew-worker`` (set by ``crew init --worker``)
    > global ``crew.cellWorker`` config > orch's CLI.
    """
    tagged = tmux.get_window_option(crew_window, "hive-crew-worker") if crew_window else ""
    if tagged in AGENT_CLI_NAMES:
        return tagged
    from . import settings as user_settings

    configured = user_settings.get_setting("crew.cellWorker", "")
    if configured in AGENT_CLI_NAMES:
        return configured
    orch_cli = tmux.get_pane_option(orch_pane, "hive-cli") or _resolve_spawn_cli_name(None)
    return orch_cli if orch_cli in AGENT_CLI_NAMES else "claude"


@crew_cmd.command("spawn-cell")
@click.option(
    "--feature-id",
    "feature_id",
    required=True,
    help="Feature id (e.g. F1) — used for window name and dispatch body",
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
def crew_spawn_peer_cmd(feature_id: str, task_artifact: str, val_artifact: str):
    """Spawn a fresh cell (worker + validator) and dispatch the task atomically.

    Must run from an orch pane inside a crew window — inherits the crew
    instance name from the caller's ``@hive-group`` tag so worker/validator
    names carry the same prefix (e.g. ``peaky.worker-1000`` when orch is
    ``peaky.orch``).

    Atomic dispatch: once both halves are ready, the command renames the
    window to ``<crew>-<feature>-running`` and sends the task artifact to
    worker + a bootstrap message to validator. This closes the window
    between spawn and first task, stopping the cell from boot-exploring
    sqlite / artifacts on its own while waiting.

    Per-crew index range: each crew owns a 1000-wide slice of tmux cell
    window indices — peaky 1000-1999, krays 2000-2999, crips 3000-3999
    (canonical pool positions), non-pool fallbacks get the next unused
    1000-block. Cells within a crew are monotonic inside that slice, so
    `$session:1000` maps to team `<main>-cell-1000` / `<crew>.worker-1000`
    / `<crew>.validator-1000`, visually grouping by crew in the status bar.

    Worker runs the crew's configured family (default: orch's; override via
    ``crew init --worker`` or the ``crew.cellWorker`` config), validator the
    anti-family. Both tagged ``@hive-group=<crew>`` and
    ``@hive-owner=<crew>.orch`` for owner-bypass routing.
    """
    if not tmux.is_inside_tmux():
        _fail("must run inside tmux")

    current_pane = tmux.get_current_pane_id() or ""
    if not current_pane:
        _fail("cannot determine current pane")

    caller_group = tmux.get_pane_option(current_pane, "hive-group") or ""
    if not caller_group or caller_group == "crew":
        _fail("current pane is not part of a CREW; run from the orch pane after `hive crew init`")

    crew_name = caller_group
    ok, reason = crew_names.validate_name(crew_name)
    if not ok:
        _fail(f"current pane's @hive-group '{crew_name}' is not a valid crew name: {reason}")

    _, main_team = _resolve_scoped_team(None, required=True)
    if main_team is None:
        _fail("no team bound to current window")

    session = main_team.tmux_session or tmux.get_current_session_name() or ""
    if not session:
        _fail("cannot determine tmux session")

    # Read the crew's index-range base from the crew window. For crew
    # windows that pre-date the range scheme (or were tagged manually),
    # auto-compute + stamp now so future spawns are consistent.
    crew_window_target = main_team.tmux_window or ""
    range_base_val = tmux.get_window_option(crew_window_target, "hive-crew-base") if crew_window_target else None
    try:
        range_base = int(range_base_val) if range_base_val else 0
    except ValueError:
        range_base = 0
    if not range_base:
        range_base = crew_names.pick_range_base(crew_name, _claimed_crew_bases(session))
        if crew_window_target:
            tmux.set_window_option(crew_window_target, "@hive-crew-base", str(range_base))

    n = _next_peer_index_in_range(session, range_base)
    worker_name = f"{crew_name}.worker-{n}"
    validator_name = f"{crew_name}.validator-{n}"
    owner_name = f"{crew_name}.orch"
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
    # without stat'ing first. Idempotent; safe to call on every spawn-peer.
    if workspace:
        artifacts_root = Path(workspace) / "artifacts"
        for sub in ("tasks", "handoffs", "verdicts"):
            (artifacts_root / sub).mkdir(parents=True, exist_ok=True)
    peer_team_name = f"{main_team.name}-cell-{n}"
    # Window name carries the crew prefix so peer windows group visually
    # under their owning crew in tmux status bars. The `-pending` suffix
    # is momentary — the atomic dispatch block below renames to
    # `<crew>-<feature>-running` once both peers are ready.
    window_name = f"{crew_name}-{_CREW_PEER_WINDOW_NAME_INITIAL}"
    # Prefer orch pane's cwd (user's project dir) over Hive workspace state dir.
    cwd = tmux.display_value(current_pane, "#{pane_current_path}") or workspace or os.getcwd()

    peer_window, shell_pane = tmux.new_window(session, name=window_name, cwd=cwd, index=n)
    if not shell_pane:
        _fail(f"failed to create window {session}:{n}")

    tmux.set_window_option(peer_window, "@hive-team", peer_team_name)
    tmux.set_window_option(peer_window, "@hive-workspace", workspace)
    tmux.set_window_option(peer_window, "@hive-crew-name", crew_name)
    tmux.set_window_option(peer_window, "@hive-created", str(time.time()))
    tmux.configure_hive_window(peer_window)

    peer_team = Team(
        name=peer_team_name,
        workspace=workspace,
        tmux_session=session,
        tmux_window=peer_window,
        tmux_window_id=tmux.get_window_id(peer_window) or "",
    )

    worker_cli = _resolve_crew_worker_cli(current_pane, crew_window_target)
    validator_cli = anti_peer_cli(worker_cli)

    worker_agent = Agent.spawn(
        name=worker_name,
        team_name=peer_team_name,
        target_pane=shell_pane,
        cwd=cwd,
        split_window=False,
        skill="none",
        prompt=_role_bootstrap_prompt("crew-worker"),
        cli=worker_cli,
    )
    tmux.set_pane_option(worker_agent.pane_id, "hive-group", crew_name)
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
        prompt=_role_bootstrap_prompt("crew-validator"),
        cli=validator_cli,
    )
    tmux.set_pane_option(validator_agent.pane_id, "hive-group", crew_name)
    tmux.set_pane_option(validator_agent.pane_id, "hive-owner", owner_name)
    peer_team.agents[validator_name] = validator_agent

    orientation = _apply_crew_layout(peer_window)

    # Declare the worker ↔ validator pair so `hive team` reflects it explicitly.
    try:
        peer_team.set_peer(worker_name, validator_name)
    except (KeyError, ValueError):
        pass

    # Block until both peer agents settle into a quiescent phase before
    # returning success. A fresh CLI pane emits the prompt (inputState=ready)
    # before the skill file has finished loading, so an immediate send after
    # spawn-peer would race the skill. Poll sidecar team-runtime until both
    # worker and validator report ready + task_closed/turn_closed.
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
    running_window_name = f"{crew_name}-{feature_id}-running"
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
            command_name="crew-spawn-dispatch",
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
            command_name="crew-spawn-dispatch",
            warn_on_long_body=False,
        )
    except RuntimeError as exc:
        dispatch_errors.append({"target": validator_name, "error": str(exc)})

    result = {
        "group": "crew",
        "cellTeam": peer_team_name,
        "window": peer_window,
        "windowName": running_window_name,
        "workspace": workspace,
        "orientation": orientation,
        "featureId": feature_id,
        "dispatch": {
            "worker": {"target": worker_name, "artifact": task_path},
            "validator": {"target": validator_name, "artifact": val_path},
        },
        "panes": {
            worker_name: worker_agent.pane_id,
            validator_name: validator_agent.pane_id,
        },
    }
    if dispatch_errors:
        result["dispatchErrors"] = dispatch_errors
        result["hint"] = (
            "peer spawned and ready, but dispatch send failed. "
            "Retry manually via `hive send <agent> ... --artifact <path>`."
        )
        click.echo(json.dumps(result, indent=2))
        sys.exit(1)
    click.echo(json.dumps(result, indent=2))


@crew_cmd.command("layout")
def crew_layout_cmd():
    """Re-apply the canonical CREW layout to the current crew window.

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
    orientation = _apply_crew_layout(window_target)
    click.echo(json.dumps({"orientation": orientation, "window": window_target}, indent=2))


def _is_cell_team_name(name: str) -> bool:
    """True if *name* matches the `<main>-cell-<N>` pattern used by spawn-cell."""
    idx = name.rfind("-cell-")
    if idx < 0:
        return False
    suffix = name[idx + len("-cell-"):]
    return bool(suffix) and suffix.isdigit()


@crew_cmd.command("cleanup")
def crew_cleanup_cmd():
    """Kill all cell-N windows of the current crew.

    Run this only after every feature is DONE and the human has signed off —
    timing is enforced by the crew-orch skill, not the CLI. No flags, no
    `[OPEN]` safety checks. The main crew window (orch / challenger) is never
    touched.
    """
    if not tmux.is_inside_tmux():
        _fail("must run inside tmux")

    current_pane = tmux.get_current_pane_id() or ""
    if not current_pane:
        _fail("cannot determine current pane")

    caller_group = tmux.get_pane_option(current_pane, "hive-group") or ""
    if not caller_group or caller_group == "crew":
        _fail("current pane is not part of a CREW; run from the orch pane after `hive crew init`")
    ok, reason = crew_names.validate_name(caller_group)
    if not ok:
        _fail(f"current pane's @hive-group '{caller_group}' is not a valid crew name: {reason}")

    _, main_team = _resolve_scoped_team(None, required=True)
    assert main_team is not None

    if _is_cell_team_name(main_team.name):
        _fail(
            f"current pane is bound to cell team {main_team.name!r}; "
            "run cleanup from the main crew window (orch / challenger)"
        )

    from .team import list_teams

    prefix = f"{main_team.name}-cell-"
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


@cli.command("status-set", hidden=True, context_settings={"ignore_unknown_options": True, "allow_extra_args": True})
@click.argument("legacy_args", nargs=-1, type=click.UNPROCESSED)
def status_set(legacy_args: tuple[str, ...]):
    """Removed legacy status publishing command."""
    del legacy_args
    _status_migration_failure("status-set")


@cli.command("status", hidden=True, context_settings={"ignore_unknown_options": True, "allow_extra_args": True})
@click.argument("legacy_args", nargs=-1, type=click.UNPROCESSED)
def status_cmd(legacy_args: tuple[str, ...]):
    """Removed projected-status command."""
    del legacy_args
    _status_migration_failure("status")


@cli.command("statuses", hidden=True, context_settings={"ignore_unknown_options": True, "allow_extra_args": True})
@click.argument("legacy_args", nargs=-1, type=click.UNPROCESSED)
def statuses_cmd(legacy_args: tuple[str, ...]):
    """Backward-compatible alias for removed `hive status`."""
    del legacy_args
    _status_migration_failure("statuses")


@cli.command("status-show", hidden=True, context_settings={"ignore_unknown_options": True, "allow_extra_args": True})
@click.argument("legacy_args", nargs=-1, type=click.UNPROCESSED)
def status_show(legacy_args: tuple[str, ...]):
    """Backward-compatible alias for removed `hive status`."""
    del legacy_args
    _status_migration_failure("status-show")


@cli.command()
@click.argument("to_agent", required=False, default="")
@click.argument("body", required=False, default="")
@click.option("--artifact", default="", help="Artifact path for large payloads")
@click.option(
    "--wait",
    is_flag=True,
    help="Block up to 60s for target pane to render msgId; otherwise delivery=failed",
)
@click.option("--to", "to_option", hidden=True, default=None)
@click.option("--msg", "msg_option", hidden=True, default=None)
def send(
    to_agent: str,
    body: str,
    artifact: str,
    wait: bool,
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
    Delivery outcomes (in the `delivery` field of the response):
      success   Target pane rendered the msgId (via transcript or stream).
      pending   Submit OK; background tracking continues for up to 60s.
      failed    Submit error OR msgId never rendered before timeout.
                Retry; CLI also exits with status 2.

    \b
    Examples:
      hive send dodo "review this diff" --artifact /tmp/diff.md
      hive send dodo "see report" --artifact - <<'EOF'
      # Findings
      - item
      EOF
      hive send dodo "ack" --wait     # block up to 60s for confirmed delivery
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
            wait=wait,
            command_name="send",
        )
    except RuntimeError as exc:
        _fail(str(exc))
        return
    _maybe_attach_compact_hint(payload, sender=sender, team_name=t.name, workspace=ws)
    click.echo(json.dumps(payload, indent=2, ensure_ascii=False))
    if payload.get("delivery") == "failed":
        sys.exit(2)


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
@click.option(
    "--wait",
    is_flag=True,
    help="Block up to 60s for target pane to render msgId; otherwise delivery=failed",
)
@click.option("--to", "to_option", hidden=True, default=None)
@click.option("--msg", "msg_option", hidden=True, default=None)
def reply(
    to_agent: str,
    body: str,
    artifact: str,
    reply_to_override: str,
    wait: bool,
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
            wait=wait,
            command_name="reply",
        )
    except RuntimeError as exc:
        _fail(str(exc))
        return
    if not reply_to_override:
        payload["autoReplyTo"] = resolved_reply_to
    _maybe_attach_compact_hint(payload, sender=sender, team_name=t.name, workspace=ws)
    click.echo(json.dumps(payload, indent=2, ensure_ascii=False))
    if payload.get("delivery") == "failed":
        sys.exit(2)


@cli.command()
@click.argument("agent_name")
@click.argument("text")
def answer(agent_name: str, text: str):
    """Answer a pending AskUserQuestion in another agent's pane.

    Only works when the target agent is waiting for a user answer.
    Use ``hive team`` to see which agents need answers.
    """
    team_name, t = _resolve_scoped_team(None, required=True)
    assert team_name is not None and t is not None
    sender = _resolve_sender(None)
    ws = _resolve_workspace(t, required=True)
    from .sidecar import request_answer

    _ensure_team_sidecar(t, ws)
    payload = request_answer(
        str(ws),
        team=t.name,
        sender_agent=sender,
        target_agent=agent_name,
        text=text,
    )
    if not payload:
        _fail("sidecar unavailable")
    if payload.get("ok") is False:
        _fail(str(payload.get("error", "answer failed")))
    payload.pop("ok", None)
    _maybe_attach_compact_hint(payload, sender=sender, team_name=t.name, workspace=ws)
    click.echo(json.dumps(payload, indent=2, ensure_ascii=False))


@cli.command()
@click.argument("message_id")
def delivery(message_id: str):
    """Check delivery status of a sent message by ID.

    Use after `hive send` returned `delivery=pending` or `failed` to
    see the sidecar's tracking state and any observation events.

    \b
    Example:
      hive delivery aBc1
    """
    _, t = _resolve_scoped_team(None, required=True)
    assert t is not None
    ws = _resolve_workspace(t, required=True)
    from .sidecar import request_delivery

    _ensure_team_sidecar(t, ws)
    payload = request_delivery(str(ws), message_id)
    if not payload:
        _fail("sidecar unavailable")
    if payload.get("ok") is False:
        _fail(str(payload.get("error", "delivery lookup failed")))
    payload.pop("ok", None)
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
@click.option("--skills", "include_skills", is_flag=True, help="Include local hive skill installation diagnostics for the target CLI.")
def doctor(agent_name: str, include_skills: bool):
    """Diagnose agent connectivity and session state.

    With no argument, probes yourself. With an agent name, probes that
    peer — pane liveness, transcript readability, sidecar heartbeat,
    runtime input state. `--skills` adds local `hive` skill installation
    diagnostics (version, path, drift vs. shipped SKILL.md) for the
    target CLI; useful after `pipx upgrade hive` when agents start
    warning about stale skills.

    \b
    Examples:
      hive doctor                  # probe self
      hive doctor dodo             # probe a peer
      hive doctor --skills         # check for hive skill drift on self's CLI
      hive doctor dodo --skills
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
    if include_skills:
        payload["skills"] = skill_sync.diagnose_hive_skill(_resolve_member_cli_name(t, target_name))
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


@cli.command("cvim", context_settings={"ignore_unknown_options": True, "allow_extra_args": True})
@click.argument("args", nargs=-1, type=click.UNPROCESSED)
def cvim_cmd(args: tuple[str, ...]) -> None:
    """Human-only: open vim seeded with the previous assistant message and send the diff back.

    Intended to be typed by the human via the agent's shell escape (e.g. `!hive cvim`)
    in Claude Code or Codex. Not meant for the model to invoke on its own.
    """
    _exec_cvim("cvim", args)


@cli.command("vim", context_settings={"ignore_unknown_options": True, "allow_extra_args": True})
@click.argument("args", nargs=-1, type=click.UNPROCESSED)
def vim_cmd(args: tuple[str, ...]) -> None:
    """Human-only: open a blank vim buffer and send the final result back to the agent pane.

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
        subprocess.run(
            ["tmux", "run-shell", "-b", f"sleep 0.2 && tmux send-keys -t {shlex.quote(reply_pane)} Escape"],
            check=False,
        )


@cli.command("vfork", context_settings={"ignore_unknown_options": True, "allow_extra_args": True})
@click.argument("args", nargs=-1, type=click.UNPROCESSED)
def vfork_cmd(args: tuple[str, ...]) -> None:
    """Human-only: fork the current Hive session into a vertical split.

    Intended to be typed by the human via the agent's shell escape (e.g. `!hive vfork`)
    in Claude Code or Codex. Not meant for the model to invoke on its own.
    """
    _exec_fork_split("v", args)


@cli.command("hfork", context_settings={"ignore_unknown_options": True, "allow_extra_args": True})
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
    skills = [str(item) for item in payload.get("skills", [])]
    command_names = list(
        dict.fromkeys(
            path.stem if path.suffix == ".md" else path.name
            for path in (Path(item) for item in commands)
        )
    )
    skill_names = list(dict.fromkeys(Path(path).name for path in skills))

    if install_root:
        lines.append(f"  install root: {install_root}")
    if command_names:
        lines.append(f"  commands: {', '.join(command_names)}")
    if skill_names:
        lines.append(f"  skills: {', '.join(skill_names)}")
    lines.append(
        "  note: existing Codex panes may not reload plugin settings dynamically; "
        "restart them if old hooks or commands still run."
    )
    return "\n".join(lines)


@plugin.command("list")
@click.option("--json", "json_output", is_flag=True, help="Emit machine-readable JSON")
def plugin_list(json_output: bool) -> None:
    """List available plugins and whether they are enabled."""
    rows = plugin_manager.list_plugins()
    if json_output:
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


@plugin.command("enable")
@click.argument("name")
@click.option("--json", "json_output", is_flag=True, help="Emit machine-readable JSON")
def plugin_enable(name: str, json_output: bool) -> None:
    """Enable a plugin and materialize its commands/skills."""
    try:
        payload = plugin_manager.enable_plugin(name)
        if json_output:
            click.echo(json.dumps(payload, ensure_ascii=False))
            return
        click.echo(_render_plugin_mutation_result("enabled", payload))
    except ValueError as e:
        _fail(str(e))


@plugin.command("disable")
@click.argument("name")
@click.option("--json", "json_output", is_flag=True, help="Emit machine-readable JSON")
def plugin_disable(name: str, json_output: bool) -> None:
    """Disable a plugin and remove its commands/skills."""
    try:
        payload = plugin_manager.disable_plugin(name)
        if json_output:
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
    "--help", "-h", "--version", "-V",
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
    context_settings={"ignore_unknown_options": True, "allow_extra_args": True},
)
@click.pass_context
def codex_cmd(ctx: click.Context):
    """Launch codex bound to a per-pane app-server daemon (hive-managed).

    Usually invoked through the `hive shell-init` shell function rather than by
    hand; all arguments are forwarded to codex. Replaces the current process
    with codex and never returns on success.
    """
    _exec_codex_managed(list(ctx.args))


_SHELL_INIT_POSIX = """\
# hive codex integration — bind interactive codex launches to a per-pane daemon.
# Bypass anytime with `command codex`. Edit/remove by deleting this function.
codex() {
  if [ -z "$TMUX" ]; then command codex "$@"; return; fi
  case "$1" in
    %(passthrough)s)
      command codex "$@"; return ;;
  esac
  hive codex "$@" || command codex "$@"
}
"""

_SHELL_INIT_FISH = """\
# hive codex integration — bind interactive codex launches to a per-pane daemon.
# Bypass anytime with `command codex`.
function codex
    if test -z "$TMUX"
        command codex $argv
        return
    end
    switch "$argv[1]"
        case %(passthrough)s
            command codex $argv
            return
    end
    hive codex $argv; or command codex $argv
end
"""


@cli.command("shell-init")
@click.argument("shell", required=False, default="")
def shell_init_cmd(shell: str):
    """Print the codex shell integration for your shell.

    Add to your shell rc to make interactive `codex` launches hive-managed:

    \b
      # ~/.zshrc or ~/.bashrc
      eval "$(hive shell-init zsh)"
      # ~/.config/fish/config.fish
      hive shell-init fish | source

    The function only acts inside tmux on interactive launches; management
    subcommands and `command codex` pass straight through to real codex.
    """
    shell = (shell or os.path.basename(os.environ.get("SHELL", "") or "zsh")).strip()
    passthrough = " ".join(_CODEX_PASSTHROUGH_SUBCOMMANDS)
    if shell == "fish":
        click.echo(_SHELL_INIT_FISH % {"passthrough": passthrough}, nl=False)
    else:
        # zsh and bash share POSIX function syntax; case patterns use `|`.
        click.echo(_SHELL_INIT_POSIX % {"passthrough": "|".join(_CODEX_PASSTHROUGH_SUBCOMMANDS)}, nl=False)


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
